//! MinIO container + S3 client for the S3 tests. Not part of `common/mod.rs`:
//! `s3_integration_test.rs` and `log_peak_alloc_test.rs` include this file
//! with `#[path = "common/minio.rs"] mod minio;`, so neither drags in the
//! other shared fixtures.

use anyhow::Result;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// Neither Docker Hub (404 since 2026-09) nor quay.io (401 since 2026-09-24)
/// serves minio/minio any more; Chainguard's build of the same server does
/// (`latest` only).
const IMAGE: &str = "cgr.dev/chainguard/minio";
const TAG: &str = "latest";

/// MinIO's built-in root credentials (the container sets none).
const ACCESS_KEY: &str = "minioadmin";
const SECRET_KEY: &str = "minioadmin";

/// Starts MinIO and returns the container (keep it alive for the test) and
/// the S3 endpoint URL.
///
/// Same settings the `testcontainers-modules` 0.15 `MinIO` module applied
/// (that crate does not support `testcontainers` 0.28): `server /data`,
/// console on `:9001`, ready once `API:` appears on stderr.
pub async fn start() -> Result<(ContainerAsync<GenericImage>, String)> {
    let container = GenericImage::new(IMAGE, TAG)
        .with_wait_for(WaitFor::message_on_stderr("API:"))
        .with_env_var("MINIO_CONSOLE_ADDRESS", ":9001")
        .with_cmd(["server", "/data"])
        // The image declares no EXPOSE; host port 0 = a free port Docker picks.
        .with_mapped_port(0, ContainerPort::Tcp(9000))
        .start()
        .await?;
    let port = container.get_host_port_ipv4(9000).await?;
    Ok((container, format!("http://127.0.0.1:{port}")))
}

/// An S3 client for the MinIO at `endpoint`.
pub fn s3_client(endpoint: &str) -> aws_sdk_s3::Client {
    let creds = aws_sdk_s3::config::Credentials::new(ACCESS_KEY, SECRET_KEY, None, None, "test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}
