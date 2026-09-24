//! Splits a reader into lines lent from one preallocated buffer.

use super::{matcher, CHUNK};
use bytes::Bytes;
use futures_core::stream::BoxStream;
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// One item from [`LineSplitter::next`], borrowed until the next call.
#[derive(Debug, PartialEq, Eq)]
pub enum LineRef<'a> {
    /// A line that ended with `\n` (the newline is not included).
    Line(&'a [u8]),
    /// The input's final line, which had no `\n`.
    Unterminated(&'a [u8]),
    /// A line longer than `max_line`, discarded without being buffered.
    Skipped { bytes: u64 },
}

/// Lends each line of `reader` from one accumulator of `max_line` bytes,
/// allocated once. Not `FramedRead`: that grows its buffer by doubling and
/// ends the stream after any decoder error, so an oversize line would end
/// the read (`tokio-util/src/codec/framed_impl.rs`).
pub struct LineSplitter<R> {
    reader: R,
    acc: Vec<u8>,
    max_line: usize,
    /// `Some(bytes so far)` while an oversize line is being discarded.
    discarding: Option<u64>,
    /// `acc` holds a line lent by the previous `next` call.
    lent: bool,
    consumed: u64,
}

impl<R: AsyncBufRead + Unpin> LineSplitter<R> {
    pub fn new(reader: R, max_line: usize) -> Self {
        Self {
            reader,
            acc: Vec::with_capacity(max_line),
            max_line,
            discarding: None,
            lent: false,
            consumed: 0,
        }
    }

    pub fn max_line(&self) -> usize {
        self.max_line
    }

    /// Bytes read from the underlying reader so far.
    pub fn bytes_consumed(&self) -> u64 {
        self.consumed
    }

    #[cfg(test)]
    pub fn accumulator_capacity(&self) -> usize {
        self.acc.capacity()
    }

    /// The next line. Cancel-safe: bytes are consumed from the reader only
    /// after they are copied, and a partial line stays in the accumulator.
    pub async fn next(&mut self) -> io::Result<Option<LineRef<'_>>> {
        if self.lent {
            self.acc.clear();
            self.lent = false;
        }
        loop {
            let buf = self.reader.fill_buf().await?;
            if buf.is_empty() {
                if let Some(bytes) = self.discarding.take() {
                    return Ok(Some(LineRef::Skipped { bytes }));
                }
                if self.acc.is_empty() {
                    return Ok(None);
                }
                self.lent = true;
                return Ok(Some(LineRef::Unterminated(&self.acc)));
            }
            let newline = buf.iter().position(|&b| b == b'\n');
            let take = newline.unwrap_or(buf.len());
            let consume = take + usize::from(newline.is_some());
            if let Some(discarded) = self.discarding.as_mut() {
                *discarded += take as u64;
                self.reader.consume(consume);
                self.consumed += consume as u64;
                if newline.is_some() {
                    let bytes = self.discarding.take().unwrap_or_default();
                    return Ok(Some(LineRef::Skipped { bytes }));
                }
                continue;
            }
            if self.acc.len() + take > self.max_line {
                // Discard from here on; the branch above handles these same
                // bytes on the next iteration.
                self.discarding = Some(self.acc.len() as u64);
                self.acc.clear();
                continue;
            }
            self.acc.extend_from_slice(&buf[..take]);
            self.reader.consume(consume);
            self.consumed += consume as u64;
            if newline.is_some() {
                self.lent = true;
                return Ok(Some(LineRef::Line(&self.acc)));
            }
        }
    }
}

/// Stream the lines of `reader` whose step is `step`, each followed by
/// `\n`, in chunks of at least `CHUNK` bytes (the last may be shorter).
/// Output buffers are `CHUNK + max_line + 1` bytes, so a match always fits
/// and no buffer grows. Oversize lines are skipped with a warning; an
/// unterminated final line is kept only when `keep_unterminated` (legacy
/// `.log` files, which have no line contract).
pub fn filtered_stream<R>(
    reader: R,
    step: String,
    max_line: usize,
    keep_unterminated: bool,
) -> BoxStream<'static, io::Result<Bytes>>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    struct State<R> {
        split: LineSplitter<R>,
        step: String,
        keep_unterminated: bool,
        done: bool,
    }
    enum Next {
        More,
        Eof,
        Fail(io::Error),
    }

    let state = State {
        split: LineSplitter::new(reader, max_line),
        step,
        keep_unterminated,
        done: false,
    };
    Box::pin(futures_util::stream::unfold(state, |mut st| async move {
        if st.done {
            return None;
        }
        let mut out: Vec<u8> = Vec::with_capacity(CHUNK + st.split.max_line() + 1);
        loop {
            let next = match st.split.next().await {
                Err(e) => Next::Fail(e),
                Ok(None) => Next::Eof,
                Ok(Some(LineRef::Skipped { bytes })) => {
                    tracing::warn!(
                        bytes,
                        "log line over max_line_bytes skipped in a filtered read"
                    );
                    Next::More
                }
                Ok(Some(LineRef::Unterminated(line))) => {
                    if st.keep_unterminated && matcher::matches_bytes(line, &st.step) {
                        out.extend_from_slice(line);
                        out.push(b'\n');
                    }
                    Next::More
                }
                Ok(Some(LineRef::Line(line))) => {
                    if matcher::matches_bytes(line, &st.step) {
                        out.extend_from_slice(line);
                        out.push(b'\n');
                    }
                    Next::More
                }
            };
            match next {
                Next::More if out.len() < CHUNK => continue,
                Next::More => return Some((Ok(Bytes::from(out)), st)),
                Next::Eof => {
                    st.done = true;
                    return (!out.is_empty()).then(|| (Ok(Bytes::from(out)), st));
                }
                Next::Fail(e) => {
                    st.done = true;
                    return Some((Err(e), st));
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, BufReader, ReadBuf};

    async fn collect(input: &[u8], max_line: usize, buf_cap: usize) -> Vec<String> {
        let mut split = LineSplitter::new(BufReader::with_capacity(buf_cap, input), max_line);
        let mut out = Vec::new();
        while let Some(item) = split.next().await.unwrap() {
            out.push(match item {
                LineRef::Line(l) => format!("L:{}", String::from_utf8_lossy(l)),
                LineRef::Unterminated(l) => format!("U:{}", String::from_utf8_lossy(l)),
                LineRef::Skipped { bytes } => format!("S:{bytes}"),
            });
        }
        out
    }

    #[tokio::test]
    async fn splits_lines_and_reports_an_unterminated_final_line() {
        assert_eq!(
            collect(b"a\nbb\nccc", 16, 4).await,
            ["L:a", "L:bb", "U:ccc"]
        );
    }

    #[tokio::test]
    async fn lines_split_across_fill_buf_boundaries() {
        assert_eq!(
            collect(b"hello world\nx\n", 64, 3).await,
            ["L:hello world", "L:x"]
        );
    }

    #[tokio::test]
    async fn an_oversize_line_is_skipped_and_the_next_line_delivered() {
        assert_eq!(
            collect(b"ok\n0123456789\nnext\n", 5, 4).await,
            ["L:ok", "S:10", "L:next"]
        );
    }

    #[tokio::test]
    async fn an_oversize_final_line_without_newline_is_skipped() {
        assert_eq!(collect(b"ok\n0123456789", 5, 4).await, ["L:ok", "S:10"]);
    }

    #[tokio::test]
    async fn empty_input_yields_nothing() {
        assert!(collect(b"", 8, 4).await.is_empty());
    }

    #[tokio::test]
    async fn counts_consumed_bytes() {
        let mut split = LineSplitter::new(BufReader::with_capacity(4, &b"ab\ncdef\ng"[..]), 3);
        while split.next().await.unwrap().is_some() {}
        assert_eq!(split.bytes_consumed(), 9);
    }

    #[tokio::test]
    async fn the_accumulator_never_grows() {
        let mut input = Vec::new();
        for i in 0..10_000 {
            input.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        input.extend_from_slice(&[b'x'; 100]);
        input.push(b'\n');
        let mut split = LineSplitter::new(BufReader::with_capacity(7, &input[..]), 32);
        assert_eq!(split.accumulator_capacity(), 32);
        while split.next().await.unwrap().is_some() {
            assert_eq!(split.accumulator_capacity(), 32);
        }
    }

    fn line(step: &str, n: usize) -> String {
        format!(r#"{{"step":"{step}","line":"{n}"}}"#)
    }

    #[tokio::test]
    async fn filtered_stream_keeps_matching_lines_in_several_chunks() {
        let mut input = String::new();
        let mut expected = String::new();
        for i in 0..10_000 {
            let l = line(if i % 2 == 0 { "a" } else { "b" }, i);
            if i % 2 == 0 {
                expected.push_str(&l);
                expected.push('\n');
            }
            input.push_str(&l);
            input.push('\n');
        }
        let reader = BufReader::new(std::io::Cursor::new(input.into_bytes()));
        let chunks: Vec<Bytes> = filtered_stream(reader, "a".into(), 1024, false)
            .try_collect()
            .await
            .unwrap();
        assert!(
            chunks.len() > 1,
            "a body over CHUNK must arrive in several chunks"
        );
        assert!(chunks.iter().all(|c| c.len() <= CHUNK + 1024 + 1));
        assert_eq!(chunks.concat(), expected.into_bytes());
    }

    #[tokio::test]
    async fn filtered_stream_keeps_an_unterminated_line_only_when_asked() {
        let input = format!("{}\n{}", line("a", 1), line("a", 2));
        for (keep, want) in [
            (true, format!("{}\n{}\n", line("a", 1), line("a", 2))),
            (false, format!("{}\n", line("a", 1))),
        ] {
            let reader = BufReader::new(std::io::Cursor::new(input.clone().into_bytes()));
            let chunks: Vec<Bytes> = filtered_stream(reader, "a".into(), 1024, keep)
                .try_collect()
                .await
                .unwrap();
            assert_eq!(String::from_utf8(chunks.concat()).unwrap(), want);
        }
    }

    /// Returns `Pending` before every byte.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        pend: bool,
    }

    impl AsyncRead for Trickle {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let was_pending = self.pend;
            self.pend = !was_pending;
            if was_pending {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if self.pos < self.data.len() {
                let b = self.data[self.pos];
                buf.put_slice(&[b]);
                self.pos += 1;
            }
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn filtered_stream_survives_pending_in_the_middle_of_a_line() {
        let input = format!("{}\n{}\n{}\n", line("a", 1), line("b", 2), line("a", 3));
        let reader = BufReader::with_capacity(
            4,
            Trickle {
                data: input.into_bytes(),
                pos: 0,
                pend: true,
            },
        );
        let chunks: Vec<Bytes> = filtered_stream(reader, "a".into(), 1024, false)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(chunks.concat()).unwrap(),
            format!("{}\n{}\n", line("a", 1), line("a", 3))
        );
    }
}
