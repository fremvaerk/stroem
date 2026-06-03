#![cfg(feature = "s3")]

use anyhow::Result;
use bytes::Bytes;
use std::sync::Arc;
use stroem_server::blob_storage::{BlobArchive, S3BlobArchive};
use stroem_server::log_storage::{JobLogMeta, LogStorage};
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::minio::MinIO;
use uuid::Uuid;

// ─── Helpers ────────────────────────────────────────────────────────────

fn jsonl_line(step: &str, stream: &str, line: &str) -> String {
    serde_json::json!({
        "ts": "2025-02-12T10:00:00Z",
        "stream": stream,
        "step": step,
        "line": line,
    })
    .to_string()
}

fn test_meta() -> JobLogMeta {
    JobLogMeta {
        workspace: "myws".to_string(),
        task_name: "mytask".to_string(),
        created_at: chrono::DateTime::parse_from_rfc3339("2025-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    }
}

async fn setup_minio() -> Result<(testcontainers::ContainerAsync<MinIO>, String)> {
    let container = MinIO::default().start().await?;
    let port = container.get_host_port_ipv4(9000).await?;
    let endpoint = format!("http://127.0.0.1:{}", port);
    Ok((container, endpoint))
}

fn test_s3_client(endpoint: &str) -> aws_sdk_s3::Client {
    let creds =
        aws_sdk_s3::config::Credentials::new("minioadmin", "minioadmin", None, None, "test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

async fn create_bucket(client: &aws_sdk_s3::Client, bucket: &str) -> Result<()> {
    client.create_bucket().bucket(bucket).send().await?;
    Ok(())
}

fn make_log_storage(
    temp_dir: &TempDir,
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
) -> LogStorage {
    let archive = S3BlobArchive::from_client(client.clone(), bucket.to_string());
    LogStorage::new(temp_dir.path()).with_archive(Arc::new(archive), prefix.to_string())
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_s3_upload_and_download() -> Result<()> {
    let (_container, endpoint) = setup_minio().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = test_s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;

    let temp_dir = TempDir::new()?;
    let storage = make_log_storage(&temp_dir, &client, &bucket, "");

    let job_id = Uuid::new_v4();
    let meta = test_meta();
    let line1 = jsonl_line("build", "stdout", "compiling...");
    let line2 = jsonl_line("build", "stdout", "done");
    let content = format!("{}\n{}\n", line1, line2);

    storage.append_log(job_id, &content).await?;
    storage.upload_to_archive(job_id, &meta).await?;

    // Verify the object exists in MinIO with structured key and is gzipped
    let key = format!(
        "myws/mytask/2025/01/15/2025-01-15T10-30-00_{}.jsonl.gz",
        job_id
    );
    let obj = client.get_object().bucket(&bucket).key(&key).send().await?;
    let bytes = obj.body.collect().await?.into_bytes();

    // Decompress and verify content
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = String::new();
    decoder.read_to_string(&mut decompressed)?;
    assert_eq!(decompressed, content);

    Ok(())
}

#[tokio::test]
async fn test_s3_read_fallback_when_local_missing() -> Result<()> {
    let (_container, endpoint) = setup_minio().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = test_s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;

    let temp_dir = TempDir::new()?;
    let storage = make_log_storage(&temp_dir, &client, &bucket, "");

    let job_id = Uuid::new_v4();
    let meta = test_meta();
    let line = jsonl_line("build", "stdout", "s3 content");
    let content = format!("{}\n", line);

    // Write locally and upload to S3
    storage.append_log(job_id, &content).await?;
    storage.upload_to_archive(job_id, &meta).await?;

    // Delete local file
    let local_path = temp_dir.path().join(format!("{}.jsonl", job_id));
    tokio::fs::remove_file(&local_path).await?;
    assert!(!local_path.exists());

    // get_log should fall back to S3
    let log = storage.get_log(job_id, &meta).await?;
    assert_eq!(log, content);

    Ok(())
}

#[tokio::test]
async fn test_s3_local_preferred_over_s3() -> Result<()> {
    let (_container, endpoint) = setup_minio().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = test_s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;

    let temp_dir = TempDir::new()?;
    let storage = make_log_storage(&temp_dir, &client, &bucket, "");

    let job_id = Uuid::new_v4();
    let meta = test_meta();
    let local_line = jsonl_line("build", "stdout", "local content");
    let local_content = format!("{}\n", local_line);

    // Write local content
    storage.append_log(job_id, &local_content).await?;

    // Upload different content to S3 directly (gzipped, with structured key)
    let s3_content = format!("{}\n", jsonl_line("build", "stdout", "s3 content"));
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(s3_content.as_bytes())?;
    let compressed = encoder.finish()?;
    let key = format!(
        "myws/mytask/2025/01/15/2025-01-15T10-30-00_{}.jsonl.gz",
        job_id
    );
    client
        .put_object()
        .bucket(&bucket)
        .key(&key)
        .body(compressed.into())
        .send()
        .await?;

    // get_log should return local content (preferred over S3)
    let log = storage.get_log(job_id, &meta).await?;
    assert_eq!(log, local_content);

    Ok(())
}

#[tokio::test]
async fn test_s3_key_with_prefix() -> Result<()> {
    let (_container, endpoint) = setup_minio().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = test_s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;

    let temp_dir = TempDir::new()?;
    let storage = make_log_storage(&temp_dir, &client, &bucket, "logs/");

    let job_id = Uuid::new_v4();
    let meta = test_meta();
    let content = format!("{}\n", jsonl_line("build", "stdout", "prefixed"));

    storage.append_log(job_id, &content).await?;
    storage.upload_to_archive(job_id, &meta).await?;

    // Verify S3 key has the prefix and structured path
    let key = format!(
        "logs/myws/mytask/2025/01/15/2025-01-15T10-30-00_{}.jsonl.gz",
        job_id
    );
    let obj = client.get_object().bucket(&bucket).key(&key).send().await?;
    let bytes = obj.body.collect().await?.into_bytes();

    // Decompress and verify
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = String::new();
    decoder.read_to_string(&mut decompressed)?;
    assert_eq!(decompressed, content);

    Ok(())
}

#[tokio::test]
async fn test_s3_get_step_log_falls_back_to_s3() -> Result<()> {
    let (_container, endpoint) = setup_minio().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = test_s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;

    let temp_dir = TempDir::new()?;
    let storage = make_log_storage(&temp_dir, &client, &bucket, "");

    let job_id = Uuid::new_v4();
    let meta = test_meta();
    let build_line = jsonl_line("build", "stdout", "compiling...");
    let test_line = jsonl_line("test", "stdout", "running tests...");
    let content = format!("{}\n{}\n", build_line, test_line);

    // Write and upload
    storage.append_log(job_id, &content).await?;
    storage.upload_to_archive(job_id, &meta).await?;

    // Delete local file
    let local_path = temp_dir.path().join(format!("{}.jsonl", job_id));
    tokio::fs::remove_file(&local_path).await?;

    // get_step_log should filter from S3 content
    let build_logs = storage.get_step_log(job_id, "build", &meta).await?;
    assert_eq!(build_logs, format!("{}\n", build_line));

    let test_logs = storage.get_step_log(job_id, "test", &meta).await?;
    assert_eq!(test_logs, format!("{}\n", test_line));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn s3_delete_prefix_removes_only_matching() -> Result<()> {
    let (_container, endpoint) = setup_minio().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = test_s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;

    let blob = S3BlobArchive::from_client(client, bucket);

    blob.put("ws/job1/a.txt", "text/plain", Bytes::from_static(b"1"))
        .await?;
    blob.put("ws/job1/b.txt", "text/plain", Bytes::from_static(b"2"))
        .await?;
    blob.put("ws/job2/c.txt", "text/plain", Bytes::from_static(b"3"))
        .await?;

    blob.delete_prefix("ws/job1/").await?;
    assert!(blob.get("ws/job1/a.txt").await?.is_none());
    assert!(blob.get("ws/job1/b.txt").await?.is_none());
    assert!(blob.get("ws/job2/c.txt").await?.is_some());

    Ok(())
}

// ─── delete_prefix partial-failure regression ───────────────────────────
//
// MinIO doesn't let us provoke per-object failures from DeleteObjects, so we
// stand up a tiny axum mock that speaks just enough of the S3 wire protocol
// (ListObjectsV2 + DeleteObjects) to return a partial-success response. The
// regression we're guarding against: the SDK call succeeds (HTTP 200) but the
// response body lists individual keys that failed; the previous implementation
// ignored these and reported the prefix wipe as successful, causing the
// retention sweeper to drop DB rows that still pointed at live blobs.

fn mock_s3_client(endpoint: &str) -> aws_sdk_s3::Client {
    let creds = aws_sdk_s3::config::Credentials::new("k", "s", None, None, "test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

#[tokio::test(flavor = "multi_thread")]
async fn s3_delete_prefix_surfaces_partial_failures() -> Result<()> {
    use axum::extract::Query;
    use axum::http::StatusCode;
    use axum::routing::{any, get};
    use axum::Router;
    use std::collections::HashMap;

    // ListObjectsV2 (`GET /{bucket}/?list-type=2&prefix=...`) returns three keys.
    async fn list_handler(Query(q): Query<HashMap<String, String>>) -> (StatusCode, String) {
        // We only need to answer list-type=2 calls; reject anything else loudly so
        // a misconfigured test fails fast instead of hanging.
        if q.get("list-type").map(|s| s.as_str()) != Some("2") {
            return (StatusCode::BAD_REQUEST, "expected list-type=2".to_string());
        }
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name>
  <Prefix>p/</Prefix>
  <KeyCount>3</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents><Key>p/ok.txt</Key><Size>1</Size><LastModified>2025-01-01T00:00:00Z</LastModified><ETag>"x"</ETag></Contents>
  <Contents><Key>p/denied.txt</Key><Size>1</Size><LastModified>2025-01-01T00:00:00Z</LastModified><ETag>"x"</ETag></Contents>
  <Contents><Key>p/broken.txt</Key><Size>1</Size><LastModified>2025-01-01T00:00:00Z</LastModified><ETag>"x"</ETag></Contents>
</ListBucketResult>"#;
        (StatusCode::OK, body.to_string())
    }

    // DeleteObjects (`POST /{bucket}/?delete`) returns 200 with one Deleted and
    // two per-object Errors — the exact shape AWS itself uses for partial
    // success. The SDK call succeeds; the failures must propagate as an Err.
    async fn delete_handler() -> (StatusCode, String) {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Deleted><Key>p/ok.txt</Key></Deleted>
  <Error><Key>p/denied.txt</Key><Code>AccessDenied</Code><Message>denied by policy</Message></Error>
  <Error><Key>p/broken.txt</Key><Code>InternalError</Code><Message>boom</Message></Error>
</DeleteResult>"#;
        (StatusCode::OK, body.to_string())
    }

    let app = Router::new()
        // path-style: `/{bucket}` and `/{bucket}/` both reachable.
        .route("/b", get(list_handler).post(delete_handler))
        .route("/b/", get(list_handler).post(delete_handler))
        .fallback(any(|| async {
            (StatusCode::NOT_FOUND, "unhandled".to_string())
        }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let endpoint = format!("http://{addr}");
    let client = mock_s3_client(&endpoint);
    let blob = S3BlobArchive::from_client(client, "b".to_string());

    let err = blob
        .delete_prefix("p/")
        .await
        .expect_err("partial-failure DeleteObjects must propagate as Err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("2 object(s) failed"),
        "missing failure count, got: {msg}"
    );
    assert!(
        msg.contains("p/denied.txt") && msg.contains("AccessDenied"),
        "missing first failed key/code, got: {msg}"
    );
    assert!(
        msg.contains("p/broken.txt") && msg.contains("InternalError"),
        "missing second failed key/code, got: {msg}"
    );
    // The single successfully-deleted key MUST NOT appear in the error message —
    // operators read this to know which keys to retry.
    assert!(
        !msg.contains("p/ok.txt"),
        "successful key leaked into error: {msg}"
    );

    server.abort();
    Ok(())
}
