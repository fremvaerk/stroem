//! Terminal-job archive reads: ranged gzip decoding, tails, streams, merge.

use super::local::{read_range_exact, LocalFile, LocalKind};
use super::splitter::{filtered_stream, LineRef, LineSplitter};
use super::{count_lines, matcher, retain_matching_lines, trim_torn, CHUNK, RANGE, READ_SLACK};
use crate::blob_storage::{ArchiveObject, BlobArchive};
use crate::config::LogReadConfig;
use crate::log_storage::merge_jsonl_logs;
use anyhow::Context as _;
use async_compression::tokio::bufread::GzipDecoder;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::future::BoxFuture;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, ReadBuf};
use tokio_util::io::ReaderStream;

/// `AsyncBufRead` over an opened archive object, fetched in ranges of
/// `range` bytes into ONE buffer that moves into each fetch and back, so a
/// finished range never coexists with the next.
pub(crate) struct ArchiveRangeReader {
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    range: u64,
    pos: u64,
    buf: Vec<u8>,
    consumed: usize,
    fetch: Option<BoxFuture<'static, anyhow::Result<Vec<u8>>>>,
}

impl ArchiveRangeReader {
    pub(crate) fn new(archive: Arc<dyn BlobArchive>, obj: Arc<ArchiveObject>) -> Self {
        Self::with_range(archive, obj, RANGE)
    }

    pub(crate) fn with_range(
        archive: Arc<dyn BlobArchive>,
        obj: Arc<ArchiveObject>,
        range: u64,
    ) -> Self {
        Self {
            archive,
            obj,
            range,
            pos: 0,
            buf: Vec::with_capacity(range as usize + READ_SLACK),
            consumed: 0,
            fetch: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn range_buffer_capacity(&self) -> usize {
        self.buf.capacity()
    }
}

impl AsyncBufRead for ArchiveRangeReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.consumed < this.buf.len() {
            return Poll::Ready(Ok(&this.buf[this.consumed..]));
        }
        if this.fetch.is_none() {
            if this.pos >= this.obj.size {
                return Poll::Ready(Ok(&[]));
            }
            let mut buf = std::mem::take(&mut this.buf);
            buf.clear();
            this.consumed = 0;
            let (archive, obj, pos, range) = (
                Arc::clone(&this.archive),
                Arc::clone(&this.obj),
                this.pos,
                this.range,
            );
            this.fetch = Some(Box::pin(async move {
                archive.read_range(&obj, pos, range, &mut buf).await?;
                Ok(buf)
            }));
        }
        let fetch = this.fetch.as_mut().expect("fetch was just set");
        let result = ready!(fetch.as_mut().poll(cx));
        this.fetch = None;
        match result {
            Ok(buf) if buf.is_empty() => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("archive object {} is shorter than its size", this.obj.key),
            ))),
            Ok(buf) => {
                debug_assert_eq!(
                    buf.capacity(),
                    this.range as usize + READ_SLACK,
                    "range buffer must not reallocate across fetches"
                );
                this.pos += buf.len() as u64;
                this.buf = buf;
                Poll::Ready(Ok(&this.buf[..]))
            }
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        this.consumed = (this.consumed + amt).min(this.buf.len());
    }
}

impl AsyncRead for ArchiveRangeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let avail = ready!(Pin::new(&mut *this).poll_fill_buf(cx))?;
        let n = avail.len().min(out.remaining());
        out.put_slice(&avail[..n]);
        this.consumed += n;
        Poll::Ready(Ok(()))
    }
}

/// The decompressed length from a gzip member's trailer (ISIZE, mod 2^32).
/// Our uploads are one member (`GzEncoder` … `finish()`), so this is exact
/// under 4 GiB; callers still bound every decompression with `take`.
pub(crate) async fn gzip_isize(
    archive: &dyn BlobArchive,
    obj: &ArchiveObject,
) -> anyhow::Result<u64> {
    anyhow::ensure!(
        obj.size >= 18,
        "archive object {} is {} bytes, too short to be gzip",
        obj.key,
        obj.size
    );
    let mut trailer = Vec::with_capacity(4 + READ_SLACK);
    archive
        .read_range(obj, obj.size - 4, 4, &mut trailer)
        .await?;
    let bytes: [u8; 4] = trailer
        .as_slice()
        .try_into()
        .with_context(|| format!("short gzip trailer read from {}", obj.key))?;
    Ok(u64::from(u32::from_le_bytes(bytes)))
}

pub(crate) struct ArchiveTail {
    pub logs: String,
    pub truncated: bool,
    /// Decompressed bytes counted while streaming (never the trailer).
    pub decompressed: u64,
}

fn decoder(
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
) -> GzipDecoder<ArchiveRangeReader> {
    GzipDecoder::new(ArchiveRangeReader::new(archive, obj))
}

/// The newest `t` bytes of the archived log (whole lines), or of its lines
/// for one step. Computed completely before the caller answers, so any
/// archive error can still fall back to local.
pub(crate) async fn tail(
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    filter: Option<&str>,
    t: u64,
    max_line: usize,
) -> anyhow::Result<ArchiveTail> {
    let reader = BufReader::with_capacity(CHUNK, decoder(archive, obj));
    Ok(match filter {
        None => byte_ring_tail(reader, t).await?,
        Some(step) => line_ring_tail(reader, step, t, max_line).await?,
    })
}

/// A ring of the last `t` decompressed bytes. No line splitter, so a line
/// of any length survives exactly as it does from the local file.
async fn byte_ring_tail<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    t: u64,
) -> io::Result<ArchiveTail> {
    let cap = usize::try_from(t).map_err(io::Error::other)?;
    let mut ring: VecDeque<u8> = VecDeque::with_capacity(cap);
    let mut total = 0u64;
    // The byte just before the ring's first byte, once anything was evicted.
    let mut before_ring: Option<u8> = None;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        total += n as u64;
        if n >= cap {
            before_ring = if n > cap {
                Some(buf[n - cap - 1])
            } else {
                ring.back().copied().or(before_ring)
            };
            ring.clear();
            ring.extend(buf[n - cap..].iter().copied());
        } else {
            let overflow = (ring.len() + n).saturating_sub(cap);
            if overflow > 0 {
                before_ring = ring.get(overflow - 1).copied();
                ring.drain(..overflow);
            }
            ring.extend(buf.iter().copied());
        }
        reader.consume(n);
    }
    let evicted = total > ring.len() as u64;
    let mut v = Vec::from(ring); // never reallocates
    let head = if evicted && before_ring != Some(b'\n') {
        v.iter()
            .position(|&b| b == b'\n')
            .map_or(v.len(), |p| p + 1)
    } else {
        0
    };
    v.drain(..head);
    let torn = trim_torn(&mut v);
    let logs = String::from_utf8(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(ArchiveTail {
        logs,
        truncated: evicted || torn,
        decompressed: total,
    })
}

/// A ring of whole matching lines, at most `t` bytes, oldest evicted first.
async fn line_ring_tail<R: tokio::io::AsyncBufRead + Unpin>(
    reader: R,
    step: &str,
    t: u64,
    max_line: usize,
) -> io::Result<ArchiveTail> {
    let cap = usize::try_from(t).map_err(io::Error::other)?;
    let mut ring: VecDeque<u8> = VecDeque::with_capacity(cap);
    let mut split = LineSplitter::new(reader, max_line);
    let mut truncated = false;
    loop {
        match split.next().await? {
            None => break,
            // Oversize, or a torn record at the end of a .jsonl snapshot.
            Some(LineRef::Skipped { .. }) | Some(LineRef::Unterminated(_)) => truncated = true,
            Some(LineRef::Line(line)) => {
                if line.is_empty() || !matcher::matches_bytes(line, step) {
                    continue;
                }
                let need = line.len() + 1;
                if need > cap {
                    // This match alone doesn't fit the window: an unfiltered
                    // byte ring would have evicted everything before it too
                    // (the same forward eviction the `else` branch below
                    // performs when a smaller match doesn't fit next to
                    // what's already held), so every match collected so far
                    // is now behind a span wider than the whole budget and
                    // must be dropped, not just this one line.
                    ring.clear();
                    truncated = true;
                    continue;
                }
                if ring.len() + need > cap {
                    let must_free = ring.len() + need - cap;
                    let cut = ring
                        .iter()
                        .skip(must_free - 1)
                        .position(|&b| b == b'\n')
                        .map_or(ring.len(), |p| must_free + p);
                    ring.drain(..cut);
                    truncated = true;
                }
                ring.extend(line.iter().copied());
                ring.push_back(b'\n');
            }
        }
    }
    let decompressed = split.bytes_consumed();
    let logs = String::from_utf8(Vec::from(ring))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(ArchiveTail {
        logs,
        truncated,
        decompressed,
    })
}

/// The archived log as a stream, served as stored (no torn-line trim: the
/// end is only known after it was sent, and an unterminated final record is
/// kept, filtered or not -- there is only one source, so nothing else could
/// complete it). Primed before returning: the first range read, the gzip
/// header and the first inflate all run here (`fill_buf` on the `BufReader`
/// wrapping the decoder), so a failure that would otherwise surface only
/// after the response's headers were already committed instead comes back
/// as an `Err` the caller can still fall back from -- local when present,
/// else an empty `LogSource::None` response, the same as an `open` failure.
/// The primed reader is threaded into the returned stream so its buffered
/// bytes are not lost.
pub(crate) async fn full_stream(
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    step: Option<String>,
    max_line: usize,
) -> io::Result<BoxStream<'static, io::Result<Bytes>>> {
    let mut reader = BufReader::with_capacity(CHUNK, decoder(archive, obj));
    reader.fill_buf().await?;
    Ok(match step {
        None => Box::pin(ReaderStream::with_capacity(reader, CHUNK)),
        Some(step) => filtered_stream(reader, step, max_line, true),
    })
}

/// The union of the local snapshot and the archive, when both fit the caps.
/// `Ok(None)` means "over a cap, or the trailer disagrees with the body":
/// the caller streams a single source instead. Nothing is sent before this
/// returns, so every failure here can still fall back.
pub(crate) async fn read_merged_full(
    local: &mut LocalFile,
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    filter: Option<&str>,
    cfg: &LogReadConfig,
) -> anyhow::Result<Option<String>> {
    let isize = gzip_isize(archive.as_ref(), &obj).await?;
    if local.len.saturating_add(isize) > cfg.merge_max_bytes {
        return Ok(None);
    }
    let mut l = Vec::with_capacity(usize::try_from(local.len)? + READ_SLACK);
    read_range_exact(&mut local.file, 0, local.len, &mut l)
        .await
        .context("read local log for merge")?;
    let mut a = Vec::with_capacity(usize::try_from(isize)? + 1 + READ_SLACK);
    let a_capacity = a.capacity();
    let limited = decoder(archive, obj).take(isize + 1);
    let mut limited = std::pin::pin!(limited);
    let n = limited
        .read_to_end(&mut a)
        .await
        .context("decompress archive for merge")?;
    debug_assert_eq!(
        a.capacity(),
        a_capacity,
        "merge archive buffer must not reallocate a preallocated buffer"
    );
    if n as u64 != isize {
        return Ok(None); // wrong, wrapped or stale trailer
    }
    if local.kind == LocalKind::Jsonl {
        trim_torn(&mut l);
    }
    trim_torn(&mut a);
    if let Some(step) = filter {
        retain_matching_lines(&mut l, step, cfg.max_line_bytes);
        retain_matching_lines(&mut a, step, cfg.max_line_bytes);
    }
    if count_lines(&l) + count_lines(&a) > cfg.merge_max_lines {
        return Ok(None);
    }
    let l = String::from_utf8(l).context("local log is not UTF-8")?;
    let a = String::from_utf8(a).context("archived log is not UTF-8")?;
    Ok(Some(merge_jsonl_logs(&l, &a)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_storage::LocalBlobArchive;
    use tokio::io::AsyncReadExt;

    pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    pub(crate) async fn put(
        dir: &tempfile::TempDir,
        bytes: Vec<u8>,
    ) -> (Arc<dyn BlobArchive>, Arc<ArchiveObject>) {
        let store: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(dir.path().to_path_buf()));
        store
            .put("ws/t/obj.jsonl.gz", "application/gzip", bytes.into())
            .await
            .unwrap();
        let obj = store.open("ws/t/obj.jsonl.gz").await.unwrap().unwrap();
        (store, Arc::new(obj))
    }

    #[tokio::test]
    async fn range_reader_reads_the_whole_object_in_ranges() {
        for data in [&b"0123456789"[..], &b"012345678"[..], &b""[..]] {
            let dir = tempfile::TempDir::new().unwrap();
            let (store, obj) = put(&dir, data.to_vec()).await;
            let mut reader = ArchiveRangeReader::with_range(store, obj, 3);
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, data);
            assert_eq!(
                reader.range_buffer_capacity(),
                3 + READ_SLACK,
                "range buffer must be reused"
            );
        }
    }

    #[tokio::test]
    async fn gzip_isize_reads_the_trailer() {
        let dir = tempfile::TempDir::new().unwrap();
        let (store, obj) = put(&dir, gzip(b"hello world")).await;
        assert_eq!(gzip_isize(store.as_ref(), &obj).await.unwrap(), 11);

        let dir2 = tempfile::TempDir::new().unwrap();
        let (store2, obj2) = put(&dir2, b"short".to_vec()).await;
        assert!(gzip_isize(store2.as_ref(), &obj2).await.is_err());
    }

    use crate::config::LogReadConfig;
    use crate::log_read::local::{open_local, LocalKind};
    use futures_util::TryStreamExt;

    fn jl(step: &str, msg: &str) -> String {
        format!(
            r#"{{"ts":"2026-09-22T00:00:0{}Z","step":"{step}","line":"{msg}"}}"#,
            msg.len() % 10
        )
    }

    fn lines(step: &str, n: usize) -> String {
        (0..n)
            .map(|i| format!("{}\n", jl(step, &format!("m{i:04}"))))
            .collect()
    }

    /// A JSONL line for `step` of exactly `len` bytes, newline excluded.
    fn sized(step: &str, len: usize) -> String {
        let base = format!(r#"{{"step":"{step}","line":""}}"#).len();
        assert!(len >= base, "line too short for step {step}");
        format!(r#"{{"step":"{step}","line":"{}"}}"#, "x".repeat(len - base))
    }

    #[tokio::test]
    async fn unfiltered_tail_keeps_the_newest_whole_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = lines("s", 10);
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let per = content.lines().next().unwrap().len() as u64 + 1;
        let t = tail(store.clone(), obj.clone(), None, 3 * per + 5, 1024)
            .await
            .unwrap();
        let expected: String = content.lines().skip(7).map(|l| format!("{l}\n")).collect();
        assert_eq!(t.logs, expected);
        assert!(t.truncated);
        assert_eq!(t.decompressed, content.len() as u64);

        let whole = tail(store.clone(), obj.clone(), None, 1 << 20, 1024)
            .await
            .unwrap();
        assert_eq!((whole.logs, whole.truncated), (content.clone(), false));

        let exact = tail(store, obj, None, 3 * per, 1024).await.unwrap();
        assert_eq!(
            exact.logs, expected,
            "a window that starts on a line boundary keeps that line"
        );
    }

    #[tokio::test]
    async fn unfiltered_tail_keeps_a_long_line_the_filtered_cap_would_drop() {
        let dir = tempfile::TempDir::new().unwrap();
        let long = format!("{}\n", "x".repeat(2 << 20));
        let (store, obj) = put(&dir, gzip(long.as_bytes())).await;
        let t = tail(store, obj, None, 4 << 20, 1 << 20).await.unwrap();
        assert_eq!((t.logs.len(), t.truncated), (long.len(), false));
    }

    #[tokio::test]
    async fn a_torn_archive_line_is_dropped_and_flagged() {
        let dir = tempfile::TempDir::new().unwrap();
        let (store, obj) = put(&dir, gzip(b"a\nb\nhalf")).await;
        let t = tail(store, obj, None, 1 << 20, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("a\nb\n", true));
    }

    #[tokio::test]
    async fn filtered_tail_keeps_the_newest_matching_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut content = String::new();
        for i in 0..20 {
            content.push_str(&jl(if i % 2 == 0 { "a" } else { "b" }, &format!("m{i:04}")));
            content.push('\n');
        }
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let a_lines: Vec<&str> = content
            .lines()
            .filter(|l| l.contains(r#""step":"a""#))
            .collect();
        let per = a_lines[0].len() as u64 + 1;
        let t = tail(store.clone(), obj.clone(), Some("a"), 2 * per, 1024)
            .await
            .unwrap();
        assert_eq!(t.logs, format!("{}\n{}\n", a_lines[8], a_lines[9]));
        assert!(t.truncated, "older matches were evicted");

        let all = tail(store, obj, Some("a"), 1 << 20, 1024).await.unwrap();
        assert_eq!(all.logs.lines().count(), 10);
        assert!(!all.truncated);
    }

    #[tokio::test]
    async fn filtered_tail_flags_skipped_and_torn_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = format!(
            "{}\n{}\n{}",
            jl("a", &"y".repeat(200)),
            jl("a", "ok"),
            r#"{"step":"a","li"#
        );
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let t = tail(store, obj, Some("a"), 1 << 20, 100).await.unwrap();
        assert_eq!(t.logs, format!("{}\n", jl("a", "ok")));
        assert!(t.truncated);
    }

    #[tokio::test]
    async fn filtered_tail_drops_older_matches_behind_a_match_larger_than_the_window() {
        // A match that alone needs more than the whole window must evict
        // everything collected before it, the same as the unfiltered byte
        // ring: `[A, B(too big), C]` keeps only `C`, matching what a local
        // scan (which stops entirely once a match can't fit) and the
        // unfiltered ring (which would have overwritten `A` with the tail
        // of `B`) both return.
        let dir = tempfile::TempDir::new().unwrap();
        let (a, b, c) = (sized("a", 30), sized("a", 200), sized("a", 30));
        let content = format!("{a}\n{b}\n{c}\n");
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let t = tail(store, obj, Some("a"), 100, 1024).await.unwrap();
        assert_eq!(t.logs, format!("{c}\n"));
        assert!(t.truncated);
    }

    #[tokio::test]
    async fn filtered_tail_over_window_match_with_nothing_after_it_is_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let (a, b) = (sized("a", 30), sized("a", 200));
        let content = format!("{a}\n{b}\n");
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let t = tail(store, obj, Some("a"), 100, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", true));
    }

    #[tokio::test]
    async fn a_corrupt_object_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (store, obj) = put(&dir, b"this is not gzip data at all".to_vec()).await;
        assert!(tail(store, obj, None, 1024, 1024).await.is_err());
    }

    #[tokio::test]
    async fn full_stream_decompresses_and_filters() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = format!("{}{}", lines("a", 3), lines("b", 2));
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let all: Vec<Bytes> = full_stream(store.clone(), obj.clone(), None, 1024)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(all.concat(), content.as_bytes());
        let a: Vec<Bytes> = full_stream(store, obj, Some("a".into()), 1024)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(String::from_utf8(a.concat()).unwrap(), lines("a", 3));
    }

    #[tokio::test]
    async fn full_stream_filtered_keeps_an_unterminated_final_record() {
        // The single-source archive stream is served exactly as stored,
        // filtered or not: an unterminated final record (a snapshot taken
        // mid-write) is kept, not dropped, since there is only one source
        // and nothing else could ever complete it.
        let dir = tempfile::TempDir::new().unwrap();
        let (a1, a2) = (jl("a", "one"), jl("a", "two"));
        let content = format!("{a1}\n{a2}"); // no trailing newline
        let (store, obj) = put(&dir, gzip(content.as_bytes())).await;
        let out: Vec<Bytes> = full_stream(store, obj, Some("a".into()), 1024)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(out.concat()).unwrap(),
            format!("{a1}\n{a2}\n")
        );
    }

    async fn local_with(dir: &tempfile::TempDir, kind: LocalKind, content: &str) -> LocalFile {
        let (jsonl, legacy) = (dir.path().join("l.jsonl"), dir.path().join("l.log"));
        tokio::fs::write(
            if kind == LocalKind::Jsonl {
                &jsonl
            } else {
                &legacy
            },
            content,
        )
        .await
        .unwrap();
        open_local(&jsonl, &legacy).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn merged_full_unions_both_sources_under_the_caps() {
        let (d1, d2) = (
            tempfile::TempDir::new().unwrap(),
            tempfile::TempDir::new().unwrap(),
        );
        let (only_local, shared, only_archive) =
            (jl("s", "local"), jl("s", "shared"), jl("s", "archive"));
        let mut local =
            local_with(&d1, LocalKind::Jsonl, &format!("{only_local}\n{shared}\n")).await;
        let (store, obj) = put(&d2, gzip(format!("{shared}\n{only_archive}\n").as_bytes())).await;
        let merged = read_merged_full(&mut local, store, obj, None, &LogReadConfig::default())
            .await
            .unwrap()
            .expect("under both caps");
        for l in [&only_local, &shared, &only_archive] {
            assert_eq!(merged.matches(l.as_str()).count(), 1, "{l}");
        }
    }

    #[tokio::test]
    async fn merged_full_filters_by_step() {
        let (d1, d2) = (
            tempfile::TempDir::new().unwrap(),
            tempfile::TempDir::new().unwrap(),
        );
        let mut local = local_with(
            &d1,
            LocalKind::Jsonl,
            &format!("{}\n{}\n", jl("a", "1"), jl("b", "2")),
        )
        .await;
        let (store, obj) = put(&d2, gzip(format!("{}\n", jl("a", "3")).as_bytes())).await;
        let merged = read_merged_full(&mut local, store, obj, Some("a"), &LogReadConfig::default())
            .await
            .unwrap()
            .unwrap();
        assert!(merged.contains(r#""line":"1""#) && merged.contains(r#""line":"3""#));
        assert!(!merged.contains(r#""step":"b""#));
    }

    #[tokio::test]
    async fn merged_full_is_abandoned_over_either_cap_or_on_a_lying_trailer() {
        let content = lines("s", 50);
        let cfg = LogReadConfig::default();
        let over_bytes = LogReadConfig {
            merge_max_bytes: 100,
            max_line_bytes: 100,
            ..cfg
        };
        let over_lines = LogReadConfig {
            merge_max_lines: 10,
            ..cfg
        };
        for c in [over_bytes, over_lines] {
            let (d1, d2) = (
                tempfile::TempDir::new().unwrap(),
                tempfile::TempDir::new().unwrap(),
            );
            let mut local = local_with(&d1, LocalKind::Jsonl, &content).await;
            let (store, obj) = put(&d2, gzip(content.as_bytes())).await;
            assert!(read_merged_full(&mut local, store, obj, None, &c)
                .await
                .unwrap()
                .is_none());
        }
        // A trailer claiming 3 bytes for a 50-line body.
        let (d1, d2) = (
            tempfile::TempDir::new().unwrap(),
            tempfile::TempDir::new().unwrap(),
        );
        let mut local = local_with(&d1, LocalKind::Jsonl, "x\n").await;
        let mut lying = gzip(content.as_bytes());
        let n = lying.len();
        lying[n - 4..].copy_from_slice(&3u32.to_le_bytes());
        let (store, obj) = put(&d2, lying).await;
        assert!(read_merged_full(&mut local, store, obj, None, &cfg)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn merged_full_keeps_a_legacy_unterminated_line_and_trims_torn_jsonl() {
        let (d1, d2) = (
            tempfile::TempDir::new().unwrap(),
            tempfile::TempDir::new().unwrap(),
        );
        let mut local = local_with(&d1, LocalKind::Legacy, "legacy tail").await;
        let (store, obj) = put(&d2, gzip(format!("{}\nhalf", jl("s", "arch")).as_bytes())).await;
        let merged = read_merged_full(&mut local, store, obj, None, &LogReadConfig::default())
            .await
            .unwrap()
            .unwrap();
        assert!(merged.contains("legacy tail\n"));
        assert!(!merged.contains("half"));
    }
}
