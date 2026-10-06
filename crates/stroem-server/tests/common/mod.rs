//! Shared integration-test fixtures, reached as `crate::common::…` from every
//! module of the integration binary (`tests/main.rs`).
#[cfg(feature = "s3")]
pub mod minio;
pub mod pinned;
pub mod tera_fixtures;
