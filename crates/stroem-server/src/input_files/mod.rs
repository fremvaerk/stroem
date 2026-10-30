//! Uploaded input files (`type: file` task inputs), spec
//! `docs/superpowers/specs/2026-10-29-file-inputs-design.md`.

pub mod accept;
pub mod attachments;
pub mod stream;
pub mod upload;

pub use accept::{attach_all_tx, preflight, task_file_values, FileGrant};
pub use attachments::{claimed_files, job_files, load_attachments};

/// The fixed text for "this server has no input-file storage" (spec § 8):
/// upload, download, creation and claim all answer with it.
pub const STORAGE_NOT_CONFIGURED: &str = "file storage is not configured";

/// A creation whose input carries a `file` value, on a server without
/// input-file storage (spec § 4.3): refused before the transaction, so no
/// job exists whose files can never be delivered. A value-free leaf (no
/// source); the HTTP routes answer 503 (`classify_execute_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputStorageUnavailable;

impl std::fmt::Display for InputStorageUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(STORAGE_NOT_CONFIGURED)
    }
}

impl std::error::Error for InputStorageUnavailable {}
