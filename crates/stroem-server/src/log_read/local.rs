//! Reads of a job's local log file: `{job}.jsonl`, else the legacy `{job}.log`.

use super::splitter::filtered_stream;
use super::{matcher, CHUNK, READ_SLACK};
use bytes::Bytes;
use futures_core::stream::BoxStream;
use std::io::{self, SeekFrom};
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use tokio_util::io::ReaderStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalKind {
    /// Written by `append_log`: every line ends with `\n`.
    Jsonl,
    /// Pre-JSONL plain text; no line contract.
    Legacy,
}

pub(crate) struct LocalFile {
    pub file: File,
    pub kind: LocalKind,
    /// Length when opened — the snapshot every read of this file respects.
    pub len: u64,
}

pub(crate) struct LocalTail {
    pub logs: String,
    pub truncated: bool,
    /// Snapshot length, plus one when a legacy file's unterminated last
    /// line was returned with a synthesized newline. An upper bound on
    /// `logs.len()`, never a lower one: `tail_step` always terminates a
    /// kept line with `\n`, even when the source line itself had none.
    pub len: u64,
}

fn to_usize(n: u64) -> io::Result<usize> {
    usize::try_from(n).map_err(io::Error::other)
}

fn utf8(buf: Vec<u8>) -> io::Result<String> {
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// `.jsonl` first, then legacy `.log`. `NotFound` on both is `Ok(None)`:
/// retention can delete a file mid-request, which must not be a 500.
pub(crate) async fn open_local(jsonl: &Path, legacy: &Path) -> io::Result<Option<LocalFile>> {
    for (path, kind) in [(jsonl, LocalKind::Jsonl), (legacy, LocalKind::Legacy)] {
        match File::open(path).await {
            Ok(file) => {
                let len = file.metadata().await?.len();
                return Ok(Some(LocalFile { file, kind, len }));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Replace `buf` with the bytes `[start, end)`. `buf` must have spare
/// capacity for them plus `READ_SLACK` so tokio never grows it.
pub(crate) async fn read_range_exact(
    file: &mut File,
    start: u64,
    end: u64,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(start)).await?;
    buf.clear();
    let before_capacity = buf.capacity();
    let n = (&mut *file).take(end - start).read_to_end(buf).await?;
    debug_assert_eq!(
        buf.capacity(),
        before_capacity,
        "read_range_exact must not reallocate a preallocated buffer"
    );
    if (n as u64) < end - start {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "log file shrank while being read",
        ));
    }
    Ok(())
}

/// The last `t` bytes of the snapshot, cut to whole lines.
pub(crate) async fn tail_unfiltered(f: &mut LocalFile, t: u64) -> io::Result<LocalTail> {
    let len = f.len;
    let start = len.saturating_sub(t);
    // One byte before the window tells whether its first line is complete.
    let from = start.saturating_sub(1);
    let mut buf = Vec::with_capacity(to_usize(len - from)? + READ_SLACK);
    read_range_exact(&mut f.file, from, len, &mut buf).await?;
    let head = if start == 0 {
        0
    } else if buf[0] == b'\n' {
        1
    } else {
        buf.iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |p| p + 1)
    };
    let mut end = buf.len();
    let mut torn = false;
    if f.kind == LocalKind::Jsonl && end > head && buf[end - 1] != b'\n' {
        end = buf[head..end]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(head, |p| head + p + 1);
        torn = true;
    }
    buf.truncate(end);
    buf.drain(..head);
    Ok(LocalTail {
        logs: utf8(buf)?,
        truncated: start > 0 || torn,
        len,
    })
}

/// Collects matching lines from the end of a result buffer backwards.
struct StepScan<'a> {
    step: &'a str,
    max_line: usize,
    out: &'a mut [u8],
    /// `out[w..]` holds the collected lines.
    w: usize,
    /// The next completed line is the text after the file's last `\n`.
    first_line: bool,
    /// Set when the file's very last line (a Legacy file's possibly
    /// unterminated tail; a Jsonl file's torn tail never reaches this,
    /// since [`Self::complete`] returns before writing it) was written to
    /// `out`. Tells the caller a synthesized trailing `\n` was added past
    /// the snapshot's real length.
    first_line_written: bool,
    jsonl: bool,
    truncated: bool,
    stop: bool,
}

impl StepScan<'_> {
    fn oversize(&mut self, bytes: u64) {
        self.first_line = false;
        self.truncated = true;
        tracing::warn!(bytes, "log line over max_line_bytes skipped in a step tail");
    }

    fn complete(&mut self, line: &[u8]) {
        let is_first = std::mem::take(&mut self.first_line);
        if is_first && !line.is_empty() && self.jsonl {
            // A torn record: the writer finishes it later.
            self.truncated = true;
            return;
        }
        if line.is_empty() {
            return;
        }
        if line.len() > self.max_line {
            self.truncated = true;
            tracing::warn!(
                bytes = line.len() as u64,
                "log line over max_line_bytes skipped in a step tail"
            );
            return;
        }
        if !matcher::matches_bytes(line, self.step) {
            return;
        }
        let need = line.len() + 1;
        if need > self.w {
            self.truncated = true;
            self.stop = true;
            return;
        }
        self.w -= need;
        self.out[self.w..self.w + line.len()].copy_from_slice(line);
        self.out[self.w + line.len()] = b'\n';
        if is_first {
            self.first_line_written = true;
        }
    }
}

/// `oversize_bytes` accumulates the size at which `max_line` was first
/// crossed for the run `oversize` is currently tracking; a further prepend
/// while already oversize is a no-op, so it never overstates the count.
fn prepend(
    pending: &mut Vec<u8>,
    oversize: &mut bool,
    oversize_bytes: &mut u64,
    seg: &[u8],
    max_line: usize,
) {
    if *oversize {
        return;
    }
    if pending.len() + seg.len() > max_line {
        *oversize = true;
        *oversize_bytes = (pending.len() + seg.len()) as u64;
        pending.clear();
        return;
    }
    pending.splice(0..0, seg.iter().copied());
}

/// The newest lines of `step`, at most `t` bytes, scanning backwards in
/// windows of `t` bytes. `truncated` is exact when `false`.
pub(crate) async fn tail_step(
    f: &mut LocalFile,
    step: &str,
    t: u64,
    max_line: usize,
    scan_max: u64,
) -> io::Result<LocalTail> {
    let len = f.len;
    let window_len = t.min(len).max(1);
    let mut window = Vec::with_capacity(to_usize(window_len)? + READ_SLACK);
    // `+ 1`: a kept legacy final line that lacked a trailing `\n` in the
    // file still gets one synthesized in the joined output, so the whole
    // file can need one more byte than `len` to render without a false
    // truncation. `min` with `t` first means this never raises the result
    // above the caller's actual `t`-byte budget when `t` is the binding
    // constraint (`t <= len` implies `t.min(len + 1) == t`).
    let cap = to_usize(t.min(len.saturating_add(1)))?;
    let mut out = vec![0u8; cap];
    let mut pending: Vec<u8> = Vec::with_capacity(max_line.min(to_usize(len)?));
    let mut pending_oversize = false;
    let mut pending_oversize_bytes: u64 = 0;
    let mut scan = StepScan {
        step,
        max_line,
        out: &mut out,
        w: cap,
        first_line: true,
        first_line_written: false,
        jsonl: f.kind == LocalKind::Jsonl,
        truncated: false,
        stop: false,
    };
    let mut pos = len;
    let mut scanned = 0u64;
    while pos > 0 && !scan.stop {
        if scanned >= scan_max {
            scan.truncated = true;
            break;
        }
        let start = pos.saturating_sub(window_len);
        read_range_exact(&mut f.file, start, pos, &mut window).await?;
        scanned += pos - start;
        let mut i = window.len();
        while !scan.stop {
            match window[..i].iter().rposition(|&b| b == b'\n') {
                Some(k) => {
                    let seg = &window[k + 1..i];
                    if pending.is_empty() && !pending_oversize {
                        scan.complete(seg);
                    } else {
                        prepend(
                            &mut pending,
                            &mut pending_oversize,
                            &mut pending_oversize_bytes,
                            seg,
                            max_line,
                        );
                        if pending_oversize {
                            scan.oversize(pending_oversize_bytes);
                        } else {
                            scan.complete(&pending);
                        }
                        pending.clear();
                        pending_oversize = false;
                        pending_oversize_bytes = 0;
                    }
                    i = k;
                }
                None => {
                    prepend(
                        &mut pending,
                        &mut pending_oversize,
                        &mut pending_oversize_bytes,
                        &window[..i],
                        max_line,
                    );
                    break;
                }
            }
        }
        pos = start;
    }
    if pos == 0 && !scan.stop {
        // The file's first line has no newline before it.
        if pending_oversize {
            scan.oversize(pending_oversize_bytes);
        } else {
            scan.complete(&pending);
        }
    }
    // Read every scalar out of `scan` in one statement: it still holds
    // `out` borrowed mutably, so `out.drain` below can't run until `scan`'s
    // last use is behind it.
    let (w, truncated, first_line_written) =
        (scan.w, scan.truncated || pos > 0, scan.first_line_written);
    out.drain(..w);
    // `first_line_written` is only ever set for a Legacy file (a Jsonl
    // file's torn tail returns from `complete` before writing) whose true
    // last line had no trailing `\n` in the source (the segment after the
    // file's last `\n` is empty otherwise, so `complete` returns on the
    // empty-line check without writing) — exactly the case where the `\n`
    // in `logs` past that line was synthesized, not read from the file.
    let reported_len = if first_line_written {
        len.saturating_add(1)
    } else {
        len
    };
    Ok(LocalTail {
        logs: utf8(out)?,
        truncated,
        len: reported_len,
    })
}

/// Offset just past the last `\n` before `len` (0 if none).
async fn last_line_end(file: &mut File, len: u64) -> io::Result<u64> {
    let mut buf = Vec::with_capacity(CHUNK + READ_SLACK);
    let mut pos = len;
    while pos > 0 {
        let start = pos.saturating_sub(CHUNK as u64);
        read_range_exact(file, start, pos, &mut buf).await?;
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        pos = start;
    }
    Ok(0)
}

/// The whole snapshot as a stream; a `.jsonl` file stops at its last
/// newline (torn-line rule). Filtered reads go through the line splitter.
pub(crate) async fn full_stream(
    mut f: LocalFile,
    step: Option<String>,
    max_line: usize,
) -> io::Result<BoxStream<'static, io::Result<Bytes>>> {
    let end = match f.kind {
        LocalKind::Jsonl => last_line_end(&mut f.file, f.len).await?,
        LocalKind::Legacy => f.len,
    };
    f.file.seek(SeekFrom::Start(0)).await?;
    let body = f.file.take(end);
    Ok(match step {
        None => Box::pin(ReaderStream::with_capacity(body, CHUNK)),
        Some(step) => filtered_stream(
            BufReader::with_capacity(CHUNK, body),
            step,
            max_line,
            f.kind == LocalKind::Legacy,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use tempfile::TempDir;

    fn jl(step: &str, msg: &str) -> String {
        format!(
            r#"{{"ts":"2026-09-22T00:00:00Z","stream":"stdout","step":"{step}","line":"{msg}"}}"#
        )
    }

    /// A JSONL line of exactly `len` bytes, newline excluded.
    fn sized(step: &str, len: usize) -> String {
        let base = format!(r#"{{"step":"{step}","line":""}}"#).len();
        assert!(len >= base, "line too short for step {step}");
        format!(r#"{{"step":"{step}","line":"{}"}}"#, "x".repeat(len - base))
    }

    async fn open(dir: &TempDir, kind: LocalKind, content: &str) -> LocalFile {
        let (jsonl, legacy) = (dir.path().join("j.jsonl"), dir.path().join("j.log"));
        let path = if kind == LocalKind::Jsonl {
            &jsonl
        } else {
            &legacy
        };
        tokio::fs::write(path, content).await.unwrap();
        open_local(&jsonl, &legacy).await.unwrap().unwrap()
    }

    async fn full(f: LocalFile, step: Option<&str>) -> String {
        let chunks: Vec<Bytes> = full_stream(f, step.map(str::to_owned), 1024)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        String::from_utf8(chunks.concat()).unwrap()
    }

    #[tokio::test]
    async fn open_local_prefers_jsonl_then_legacy_then_none() {
        let dir = TempDir::new().unwrap();
        let (jsonl, legacy) = (dir.path().join("j.jsonl"), dir.path().join("j.log"));
        assert!(open_local(&jsonl, &legacy).await.unwrap().is_none());
        tokio::fs::write(&legacy, "old\n").await.unwrap();
        assert_eq!(
            open_local(&jsonl, &legacy).await.unwrap().unwrap().kind,
            LocalKind::Legacy
        );
        tokio::fs::write(&jsonl, "").await.unwrap();
        let f = open_local(&jsonl, &legacy).await.unwrap().unwrap();
        assert_eq!((f.kind, f.len), (LocalKind::Jsonl, 0));
    }

    #[tokio::test]
    async fn unfiltered_tail_returns_whole_lines_from_the_end() {
        let dir = TempDir::new().unwrap();
        let lines: Vec<String> = (0..10).map(|i| jl("s", &format!("line-{i}"))).collect();
        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let per = lines[0].len() as u64 + 1;
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let tail = tail_unfiltered(&mut f, 3 * per + 5).await.unwrap();
        assert_eq!(
            tail.logs,
            format!("{}\n{}\n{}\n", lines[7], lines[8], lines[9])
        );
        assert!(tail.truncated);
        assert_eq!(tail.len, content.len() as u64);
    }

    #[tokio::test]
    async fn unfiltered_tail_keeps_a_first_line_that_starts_on_the_boundary() {
        let dir = TempDir::new().unwrap();
        let lines: Vec<String> = (0..5).map(|i| jl("s", &format!("line-{i}"))).collect();
        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let per = lines[0].len() as u64 + 1;
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let tail = tail_unfiltered(&mut f, 3 * per).await.unwrap();
        assert_eq!(
            tail.logs,
            format!("{}\n{}\n{}\n", lines[2], lines[3], lines[4])
        );
    }

    #[tokio::test]
    async fn unfiltered_tail_edge_cases() {
        let dir = TempDir::new().unwrap();
        let mut f = open(&dir, LocalKind::Jsonl, "a\nb\n").await;
        let whole = tail_unfiltered(&mut f, 1024).await.unwrap();
        assert_eq!((whole.logs.as_str(), whole.truncated), ("a\nb\n", false));

        let mut empty = open(&dir, LocalKind::Jsonl, "").await;
        let t = tail_unfiltered(&mut empty, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", false));

        let mut long = open(&dir, LocalKind::Jsonl, &format!("{}\n", "x".repeat(100))).await;
        let t = tail_unfiltered(&mut long, 10).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", true));
    }

    #[tokio::test]
    async fn unfiltered_tail_reads_exactly_the_snapshot() {
        let dir = TempDir::new().unwrap();
        let mut f = open(&dir, LocalKind::Jsonl, "a\nb\n").await;
        {
            use std::io::Write;
            let mut w = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join("j.jsonl"))
                .unwrap();
            w.write_all(b"appended-after-snapshot\n").unwrap();
        }
        let t = tail_unfiltered(&mut f, 1024).await.unwrap();
        assert_eq!(t.logs, "a\nb\n");
        assert!(t.logs.len() as u64 <= t.len);
    }

    #[tokio::test]
    async fn torn_last_line_is_dropped_from_jsonl_and_kept_in_legacy() {
        let dir = TempDir::new().unwrap();
        let mut j = open(&dir, LocalKind::Jsonl, "a-line\nhalf").await;
        let t = tail_unfiltered(&mut j, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("a-line\n", true));

        let dir2 = TempDir::new().unwrap();
        let mut l = open(&dir2, LocalKind::Legacy, "legacy content").await;
        let t = tail_unfiltered(&mut l, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("legacy content", false));
    }

    #[tokio::test]
    async fn step_tail_assembles_lines_across_windows() {
        let dir = TempDir::new().unwrap();
        let (a, b, c) = (sized("a", 39), sized("b", 39), sized("a", 39));
        let mut f = open(&dir, LocalKind::Jsonl, &format!("{a}\n{b}\n{c}\n")).await;
        let t = tail_step(&mut f, "a", 100, 1024, u64::MAX).await.unwrap();
        assert_eq!(t.logs, format!("{a}\n{c}\n"));
        assert!(!t.truncated, "scan reached the start with nothing excluded");
    }

    #[tokio::test]
    async fn step_tail_finds_a_quiet_step_behind_a_chatty_one() {
        let dir = TempDir::new().unwrap();
        let quiet = sized("quiet", 40);
        let mut content = format!("{quiet}\n");
        for _ in 0..20 {
            content.push_str(&sized("chatty", 40));
            content.push('\n');
        }
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let t = tail_step(&mut f, "quiet", 128, 1024, u64::MAX)
            .await
            .unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{quiet}\n"), false));
    }

    #[tokio::test]
    async fn step_tail_does_not_take_a_match_that_would_cross_the_budget() {
        let dir = TempDir::new().unwrap();
        let lines: Vec<String> = (0..3).map(|_| sized("a", 59)).collect();
        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let t = tail_step(&mut f, "a", 100, 1024, u64::MAX).await.unwrap();
        assert_eq!(t.logs, format!("{}\n", lines[2]));
        assert!(t.truncated);
    }

    #[tokio::test]
    async fn step_tail_stops_at_the_scan_cap() {
        let dir = TempDir::new().unwrap();
        let mut content = format!("{}\n", sized("quiet", 40));
        for _ in 0..100 {
            content.push_str(&sized("chatty", 40));
            content.push('\n');
        }
        let mut f = open(&dir, LocalKind::Jsonl, &content).await;
        let t = tail_step(&mut f, "quiet", 256, 1024, 1024).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", true));
    }

    #[tokio::test]
    async fn step_tail_matches_exactly() {
        let dir = TempDir::new().unwrap();
        let (b, n) = (jl("build", "compiling"), jl("build-notify", "build done"));
        let mut f = open(&dir, LocalKind::Jsonl, &format!("{b}\n{n}\n")).await;
        let t = tail_step(&mut f, "build", 4096, 1024, u64::MAX)
            .await
            .unwrap();
        assert_eq!(t.logs, format!("{b}\n"));
    }

    #[tokio::test]
    async fn step_tail_skips_lines_over_the_line_cap() {
        let dir = TempDir::new().unwrap();
        let (long, short) = (sized("a", 80), sized("a", 30));
        // Whole oversize line inside one window.
        let mut f = open(&dir, LocalKind::Jsonl, &format!("{long}\n{short}\n")).await;
        let t = tail_step(&mut f, "a", 4096, 50, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{short}\n"), true));
        // Oversize line assembled across windows.
        let dir2 = TempDir::new().unwrap();
        // `sized("a", _)`'s minimum is 22 bytes (the fixed JSON shell around
        // an empty `line` value for a 1-byte step name); 25 is the smallest
        // round number clear of that floor.
        let (long2, short2) = (sized("a", 100), sized("a", 25));
        let mut g = open(&dir2, LocalKind::Jsonl, &format!("{long2}\n{short2}\n")).await;
        let t = tail_step(&mut g, "a", 32, 40, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{short2}\n"), true));
    }

    #[tokio::test]
    async fn step_tail_torn_newest_line() {
        let dir = TempDir::new().unwrap();
        let first = jl("a", "1");
        let mut f = open(
            &dir,
            LocalKind::Jsonl,
            &format!("{first}\n{{\"step\":\"a\",\"li"),
        )
        .await;
        let t = tail_step(&mut f, "a", 4096, 1024, u64::MAX).await.unwrap();
        assert_eq!((t.logs, t.truncated), (format!("{first}\n"), true));

        let dir2 = TempDir::new().unwrap();
        let second = jl("a", "2");
        let mut g = open(&dir2, LocalKind::Legacy, &format!("{first}\n{second}")).await;
        let t = tail_step(&mut g, "a", 4096, 1024, u64::MAX).await.unwrap();
        assert_eq!(
            (t.logs, t.truncated),
            (format!("{first}\n{second}\n"), false)
        );
    }

    #[tokio::test]
    async fn step_tail_on_an_empty_file() {
        let dir = TempDir::new().unwrap();
        let mut f = open(&dir, LocalKind::Jsonl, "").await;
        let t = tail_step(&mut f, "a", 1024, 1024, u64::MAX).await.unwrap();
        assert_eq!((t.logs.as_str(), t.truncated), ("", false));
    }

    #[tokio::test]
    async fn step_tail_legacy_len_bounds_the_synthesized_newline() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (jl("x", "1"), jl("x", "2"));
        // No trailing `\n`: the file's raw length is one byte short of what
        // the joined, newline-terminated output needs.
        let mut f = open(&dir, LocalKind::Legacy, &format!("{a}\n{b}")).await;
        let raw_len = f.len;
        let t = tail_step(&mut f, "x", raw_len + 1, 1024, u64::MAX)
            .await
            .unwrap();
        assert_eq!(t.logs, format!("{a}\n{b}\n"));
        assert!(!t.truncated);
        assert!(t.logs.len() as u64 <= t.len);
        assert_eq!(t.len, raw_len + 1);
    }

    #[tokio::test]
    async fn full_stream_stops_at_the_last_newline_of_a_jsonl_file() {
        let dir = TempDir::new().unwrap();
        let f = open(&dir, LocalKind::Jsonl, "a\nb\nhalf").await;
        assert_eq!(full(f, None).await, "a\nb\n");
        let dir2 = TempDir::new().unwrap();
        let g = open(&dir2, LocalKind::Legacy, "a\nb\nhalf").await;
        assert_eq!(full(g, None).await, "a\nb\nhalf");
    }

    #[tokio::test]
    async fn full_stream_filters_and_chunks() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (jl("a", "1"), jl("b", "2"));
        let f = open(&dir, LocalKind::Jsonl, &format!("{a}\n{b}\n{a}\n")).await;
        assert_eq!(full(f, Some("a")).await, format!("{a}\n{a}\n"));

        let dir2 = TempDir::new().unwrap();
        let big: String = (0..4000)
            .map(|i| format!("{}\n", jl("s", &i.to_string())))
            .collect();
        let g = open(&dir2, LocalKind::Jsonl, &big).await;
        let chunks: Vec<Bytes> = full_stream(g, None, 1024)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), big.into_bytes());
    }
}
