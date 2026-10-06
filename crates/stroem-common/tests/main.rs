//! The crate's one integration-test binary: every file under `tests/` is a
//! module here (`autotests = false` in Cargo.toml), never its own target.

mod integration_test;
mod raw_detail_guard;
mod test_targets_guard;
