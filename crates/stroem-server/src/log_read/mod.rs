//! Bounded log reads: tails, full streams and the terminal-job merge.
//! Spec: `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md`.

pub(crate) mod archive;
pub(crate) mod local;
pub(crate) mod matcher;
pub(crate) mod splitter;

/// Chunk size of streamed bodies and reader buffers (K).
pub const CHUNK: usize = 64 * 1024;
/// Size of one archive range read (R).
pub const RANGE: u64 = 1024 * 1024;
/// Spare capacity so tokio's `read_to_end` never grows a preallocated
/// buffer: it reserves only when fewer than 32 bytes of spare capacity
/// remain (`tokio/src/io/util/vec_with_initialized.rs`).
pub const READ_SLACK: usize = 32;

/// Which lines of a job log a read returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepFilter<'a> {
    All,
    Step(&'a str),
}

impl<'a> StepFilter<'a> {
    pub fn step(self) -> Option<&'a str> {
        match self {
            Self::All => None,
            Self::Step(s) => Some(s),
        }
    }
}

use bytes::Bytes;
use futures_core::stream::BoxStream;
use serde::Serialize;

// `human_bytes` lives in `stroem_common::format` — the CLI (a later task)
// needs the same formatter and both crates depend on stroem-common.
pub use stroem_common::format::human_bytes;

/// Response header naming the source that served a log read.
pub const LOG_SOURCE_HEADER: &str = "x-stroem-log-source";

/// Which source served a log read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Local,
    Archive,
    Merged,
    None,
}

impl LogSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Archive => "archive",
            Self::Merged => "merged",
            Self::None => "none",
        }
    }
}

/// A bounded read of a log's end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// Whole JSONL lines, at most the requested `tail_bytes`.
    pub logs: String,
    /// `true` when something that exists was left out; `false` is exact.
    pub truncated: bool,
    /// Upper bound on the size of the complete job log.
    pub total_bytes: u64,
    pub source: LogSource,
}

impl Tail {
    pub fn empty() -> Self {
        Self {
            logs: String::new(),
            truncated: false,
            total_bytes: 0,
            source: LogSource::None,
        }
    }

    pub fn returned_bytes(&self) -> u64 {
        self.logs.len() as u64
    }
}

/// Serialise the tail envelope into a buffer preallocated to its worst
/// case: serde_json escapes one byte into at most six (`\u00XX`), and 256
/// bytes cover the field names, punctuation, a bool and two `u64`s.
pub fn tail_body(tail: &Tail) -> anyhow::Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        logs: &'a str,
        truncated: bool,
        total_bytes: u64,
        returned_bytes: u64,
    }
    let mut body = Vec::with_capacity(6 * tail.logs.len() + 256);
    serde_json::to_writer(
        &mut body,
        &Envelope {
            logs: &tail.logs,
            truncated: tail.truncated,
            total_bytes: tail.total_bytes,
            returned_bytes: tail.returned_bytes(),
        },
    )?;
    Ok(body)
}

/// Non-empty lines in a JSONL buffer; an unterminated last line counts.
pub(crate) fn count_lines(buf: &[u8]) -> usize {
    buf.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count()
}

/// Drop whole lines from the front until `s` is at most `max` bytes, in
/// place. Returns whether anything was dropped.
pub(crate) fn cut_front_to_lines(mut s: String, max: u64) -> (String, bool) {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    if s.len() <= max {
        return (s, false);
    }
    let start = s.len() - max;
    let bytes = s.as_bytes();
    let cut = if bytes[start - 1] == b'\n' {
        start
    } else {
        bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(s.len(), |p| start + p + 1)
    };
    s.drain(..cut);
    (s, true)
}

/// Drop an unterminated last line. Every writer of a `.jsonl` log ends each
/// line with `\n`, so an unterminated suffix is a write in progress.
/// Returns whether anything was dropped.
pub(crate) fn trim_torn(buf: &mut Vec<u8>) -> bool {
    if buf.last().is_none_or(|&b| b == b'\n') {
        return false;
    }
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    buf.truncate(end);
    true
}

/// Keep only the lines of `buf` whose step is `step`, in place. Lines
/// longer than `max_line` are dropped, as every filtered read does.
pub(crate) fn retain_matching_lines(buf: &mut Vec<u8>, step: &str, max_line: usize) {
    let len = buf.len();
    let (mut read, mut write) = (0, 0);
    while read < len {
        let end = buf[read..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(len, |p| read + p + 1);
        let keep = {
            let line = &buf[read..end];
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            body.len() <= max_line && matcher::matches_bytes(body, step)
        };
        if keep {
            buf.copy_within(read..end, write);
            write += end - read;
        }
        read = end;
    }
    buf.truncate(write);
}

/// Serve an in-memory string as a body of `CHUNK`-sized slices without
/// copying it.
pub(crate) fn bytes_stream(s: String) -> BoxStream<'static, std::io::Result<Bytes>> {
    let bytes = Bytes::from(s);
    let len = bytes.len();
    Box::pin(futures_util::stream::iter(
        (0..len)
            .step_by(CHUNK)
            .map(move |i| Ok(bytes.slice(i..(i + CHUNK).min(len)))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;

    // `human_bytes` moved to `stroem_common::format`; its unit test lives
    // there (`crates/stroem-common/src/format.rs`).

    #[test]
    fn count_lines_skips_blank_lines_and_counts_an_unterminated_one() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a\n\nb\n"), 2);
        assert_eq!(count_lines(b"a\nb"), 2);
    }

    #[test]
    fn cut_front_to_lines_keeps_whole_lines_only() {
        let s = || String::from("aa\nbb\ncc\n");
        assert_eq!(cut_front_to_lines(s(), 100), (s(), false));
        assert_eq!(cut_front_to_lines(s(), 6), ("bb\ncc\n".to_string(), true));
        assert_eq!(cut_front_to_lines(s(), 5), ("cc\n".to_string(), true));
        assert_eq!(
            cut_front_to_lines("aaaaaa\n".to_string(), 3),
            (String::new(), true)
        );
    }

    #[test]
    fn cut_front_to_lines_does_not_allocate() {
        let s = String::from("aa\nbb\ncc\n");
        let cap = s.capacity();
        let (out, _) = cut_front_to_lines(s, 6);
        assert_eq!(out.capacity(), cap);
    }

    #[test]
    fn trim_torn_drops_only_an_unterminated_suffix() {
        let mut a = b"a\nb".to_vec();
        assert!(trim_torn(&mut a));
        assert_eq!(a, b"a\n");
        let mut b = b"a\n".to_vec();
        assert!(!trim_torn(&mut b));
        assert_eq!(b, b"a\n");
        let mut c = b"abc".to_vec();
        assert!(trim_torn(&mut c));
        assert!(c.is_empty());
        let mut d = Vec::new();
        assert!(!trim_torn(&mut d));
    }

    #[test]
    fn retain_matching_lines_filters_in_place() {
        let mut buf = concat!(
            r#"{"step":"a","line":"1"}"#,
            "\n",
            r#"{"step":"b","line":"2"}"#,
            "\n",
            r#"{"step":"a","line":"3"}"#
        )
        .as_bytes()
        .to_vec();
        let cap = buf.capacity();
        retain_matching_lines(&mut buf, "a", 1024);
        assert_eq!(
            String::from_utf8(buf.clone()).unwrap(),
            concat!(
                r#"{"step":"a","line":"1"}"#,
                "\n",
                r#"{"step":"a","line":"3"}"#
            )
        );
        assert_eq!(buf.capacity(), cap);
    }

    #[test]
    fn retain_matching_lines_drops_lines_over_the_line_cap() {
        let long = format!(r#"{{"step":"a","line":"{}"}}"#, "x".repeat(100));
        let mut buf = format!("{long}\n{}\n", r#"{"step":"a","line":"1"}"#).into_bytes();
        retain_matching_lines(&mut buf, "a", 50);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\"step\":\"a\",\"line\":\"1\"}\n"
        );
    }

    #[test]
    fn tail_body_never_reallocates_for_worst_case_escapes() {
        let tail = Tail {
            logs: "\u{1}".repeat(4096),
            truncated: true,
            total_bytes: u64::MAX,
            source: LogSource::Merged,
        };
        let body = tail_body(&tail).unwrap();
        assert_eq!(
            body.capacity(),
            6 * 4096 + 256,
            "the envelope buffer must not grow"
        );
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["returned_bytes"], 4096);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["total_bytes"], u64::MAX);
        assert_eq!(v["logs"].as_str().unwrap().len(), 4096);
    }

    #[test]
    fn log_source_header_values() {
        assert_eq!(LogSource::Local.as_str(), "local");
        assert_eq!(LogSource::Archive.as_str(), "archive");
        assert_eq!(LogSource::Merged.as_str(), "merged");
        assert_eq!(LogSource::None.as_str(), "none");
        assert_eq!(Tail::empty().source, LogSource::None);
    }

    #[tokio::test]
    async fn bytes_stream_yields_chunk_sized_slices() {
        let s = "x".repeat(CHUNK * 2 + 10);
        let chunks: Vec<bytes::Bytes> = bytes_stream(s.clone()).try_collect().await.unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), s.into_bytes());
    }
}
