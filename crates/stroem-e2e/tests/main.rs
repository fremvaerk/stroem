//! The crate's one integration-test binary: every file under `tests/` is a
//! module here (`autotests = false` in Cargo.toml), never its own target.

mod e2e_test;
mod harness;
