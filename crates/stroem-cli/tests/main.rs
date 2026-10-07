//! The crate's one integration-test binary: every file under `tests/` is a
//! module here (`autotests = false` in Cargo.toml), never its own target.

mod cli_error_report;
mod json_input;
mod run_exit_code;
