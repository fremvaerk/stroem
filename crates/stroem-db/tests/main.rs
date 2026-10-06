//! The crate's one integration-test binary: every file under `tests/` is a
//! module here (`autotests = false` in Cargo.toml), never its own target.

mod common;
mod git_refs_test;
mod integration_test;
mod job_artifact_repo;
mod job_step_status_tests;
mod migration_test;
mod tarball_keep_revisions_test;
