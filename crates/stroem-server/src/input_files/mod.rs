//! Uploaded input files (`type: file` task inputs), spec
//! `docs/superpowers/specs/2026-10-29-file-inputs-design.md`.

pub mod accept;
pub mod stream;
pub mod upload;

pub use accept::{attach_all_tx, preflight, task_file_values, FileGrant};
