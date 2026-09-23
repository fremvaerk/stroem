//! Bounded log reads: tails, full streams and the terminal-job merge.
//! Spec: `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md`.
// Pieces land task by task and are wired into `LogStorage` in Task 8;
// Task 10 removes this allow.
#![allow(dead_code)]

pub(crate) mod matcher;

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
