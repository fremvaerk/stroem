#![cfg(feature = "s3")]

use anyhow::Result;
use bytes::Bytes;
use std::sync::Arc;
use stroem_server::blob_storage::{BlobArchive, S3BlobArchive};
use stroem_server::log_read::StepFilter;
use stroem_server::log_storage::{JobLogMeta, LogStorage};
use tempfile::TempDir;
use uuid::Uuid;

use crate::common::minio;

/// `read_tail`'s tail budget for tests that want the whole log — well above
/// anything these fixtures write.
const ALL: u64 = 16 * 1024 * 1024;

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
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
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
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
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

    // get_log should fall back to S3 — is_terminal=true because the
    // archive is only consulted for terminal jobs (pre-terminal archive
    // reads are guaranteed 404s — the upload happens at terminal time).
    let log = storage
        .read_tail(job_id, &meta, true, StepFilter::All, ALL)
        .await?
        .logs;
    assert_eq!(log, content);

    Ok(())
}

#[tokio::test]
async fn test_s3_local_preferred_over_s3() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
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
    let log = storage
        .read_tail(job_id, &meta, false, StepFilter::All, ALL)
        .await?
        .logs;
    assert_eq!(log, local_content);

    Ok(())
}

#[tokio::test]
async fn test_s3_key_with_prefix() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
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
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
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

    // get_step_log should filter from S3 content (is_terminal=true so the
    // archive is consulted).
    let build_logs = storage
        .read_tail(job_id, &meta, true, StepFilter::Step("build"), ALL)
        .await?
        .logs;
    assert_eq!(build_logs, format!("{}\n", build_line));

    let test_logs = storage
        .read_tail(job_id, &meta, true, StepFilter::Step("test"), ALL)
        .await?
        .logs;
    assert_eq!(test_logs, format!("{}\n", test_line));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn s3_delete_prefix_removes_only_matching() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
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

#[tokio::test]
async fn s3_open_and_read_range_are_bounded_and_version_pinned() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;
    let archive = S3BlobArchive::from_client(client.clone(), bucket.clone());

    assert!(archive.open("missing").await?.is_none());
    archive
        .put("k", "text/plain", Bytes::from_static(b"0123456789"))
        .await?;
    let obj = archive.open("k").await?.unwrap();
    assert_eq!(obj.size, 10);

    let mut v = Vec::with_capacity(64);
    archive.read_range(&obj, 2, 3, &mut v).await?;
    assert_eq!(v, b"234");
    v.clear();
    archive.read_range(&obj, 8, 10, &mut v).await?;
    assert_eq!(v, b"89");
    v.clear();
    archive.read_range(&obj, 10, 5, &mut v).await?;
    assert!(v.is_empty());

    archive
        .put("k", "text/plain", Bytes::from_static(b"replaced!!"))
        .await?;
    let err = archive
        .read_range(&obj, 0, 5, &mut Vec::new())
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("changed since it was opened"),
        "{err:#}"
    );
    Ok(())
}

// ─── put_stream / get_stream (file inputs, spec § 9) ────────────────────

use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use stroem_server::input_files::stream::S3_PART_BYTES;

const MIB: usize = 1024 * 1024;

/// `len` bytes that do not repeat on any part boundary.
fn pattern(len: usize) -> Bytes {
    (0..len)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<u8>>()
        .into()
}

/// `data` cut into `chunk`-byte pieces (the last one shorter).
fn pieces(data: &Bytes, chunk: usize) -> Vec<anyhow::Result<Bytes>> {
    (0..data.len())
        .step_by(chunk)
        .map(|i| Ok(data.slice(i..(i + chunk).min(data.len()))))
        .collect()
}

fn body_of(parts: Vec<anyhow::Result<Bytes>>) -> BoxStream<'static, anyhow::Result<Bytes>> {
    Box::pin(futures_util::stream::iter(parts))
}

async fn read_back(archive: &S3BlobArchive, key: &str) -> Result<(String, Vec<u8>, usize)> {
    let (ct, mut stream) = archive.get_stream(key).await?.expect("object exists");
    let mut got = Vec::new();
    let mut frames = 0;
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk?);
        frames += 1;
    }
    Ok((ct, got, frames))
}

async fn open_uploads(client: &aws_sdk_s3::Client, bucket: &str) -> Result<usize> {
    let out = client
        .list_multipart_uploads()
        .bucket(bucket)
        .send()
        .await?;
    Ok(out.uploads().len())
}

async fn etag(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Result<String> {
    let head = client.head_object().bucket(bucket).key(key).send().await?;
    Ok(head.e_tag().unwrap_or_default().to_string())
}

#[tokio::test]
async fn s3_put_stream_small_single_put() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;
    let archive = S3BlobArchive::from_client(client.clone(), bucket.clone());

    let data = pattern(3 * 1024 + 7);
    archive
        .put_stream(
            "inputs/ws/file_small",
            "text/csv",
            body_of(pieces(&data, 1024)),
        )
        .await?;
    // A multipart object's ETag carries a `-<parts>` suffix.
    assert!(!etag(&client, &bucket, "inputs/ws/file_small")
        .await?
        .contains('-'));
    let (ct, got, _) = read_back(&archive, "inputs/ws/file_small").await?;
    assert_eq!(ct, "text/csv");
    assert_eq!(got, data);

    // A body of exactly one part is still a single PUT.
    let data = pattern(S3_PART_BYTES);
    archive
        .put_stream(
            "inputs/ws/file_one_part",
            "application/octet-stream",
            body_of(pieces(&data, 64 * 1024)),
        )
        .await?;
    assert!(!etag(&client, &bucket, "inputs/ws/file_one_part")
        .await?
        .contains('-'));
    assert_eq!(
        read_back(&archive, "inputs/ws/file_one_part").await?.1,
        data
    );

    // An empty body is an empty object.
    archive
        .put_stream("inputs/ws/file_empty", "text/plain", body_of(vec![]))
        .await?;
    assert!(read_back(&archive, "inputs/ws/file_empty")
        .await?
        .1
        .is_empty());

    assert!(archive
        .get_stream("inputs/ws/file_missing")
        .await?
        .is_none());
    assert_eq!(open_uploads(&client, &bucket).await?, 0);
    Ok(())
}

#[tokio::test]
async fn s3_put_stream_multipart_roundtrip() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;
    let archive = S3BlobArchive::from_client(client.clone(), bucket.clone());

    let data = pattern(20 * MIB);
    // 100 000-byte chunks straddle every part boundary.
    archive
        .put_stream(
            "inputs/ws/file_multi",
            "application/zip",
            body_of(pieces(&data, 100_000)),
        )
        .await?;
    let tag = etag(&client, &bucket, "inputs/ws/file_multi").await?;
    assert!(
        tag.trim_matches('"').ends_with("-3"),
        "8 + 8 + 4 MiB parts, ETag {tag}"
    );
    let (ct, got, frames) = read_back(&archive, "inputs/ws/file_multi").await?;
    assert_eq!(ct, "application/zip");
    assert!(got == data, "content differs ({} bytes read)", got.len());
    assert!(
        frames >= 2,
        "the download must stream, got {frames} frame(s)"
    );
    assert_eq!(open_uploads(&client, &bucket).await?, 0);
    Ok(())
}

#[tokio::test]
async fn s3_put_stream_error_aborts_multipart() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;
    let archive = S3BlobArchive::from_client(client.clone(), bucket.clone());

    // Two full parts and a bit, then the body fails. When the failing item
    // is asked for, two parts are already uploaded: record what S3 holds.
    let data = pattern(2 * S3_PART_BYTES + 64 * 1024);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let failing = {
        let (client, bucket, seen) = (client.clone(), bucket.clone(), seen.clone());
        futures_util::stream::once(async move {
            let uploads = client
                .list_multipart_uploads()
                .bucket(&bucket)
                .send()
                .await?;
            let parts = match uploads.uploads().first().and_then(|u| u.upload_id()) {
                Some(id) => client
                    .list_parts()
                    .bucket(&bucket)
                    .key("inputs/ws/file_cut")
                    .upload_id(id)
                    .send()
                    .await?
                    .parts()
                    .len(),
                None => 0,
            };
            *seen.lock().unwrap() = Some((uploads.uploads().len(), parts));
            Err::<Bytes, _>(anyhow::anyhow!("client went away"))
        })
    };
    let body = futures_util::stream::iter(pieces(&data, 64 * 1024)).chain(failing);
    let err = archive
        .put_stream("inputs/ws/file_cut", "text/plain", Box::pin(body))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("client went away"), "{err:#}");
    assert_eq!(
        *seen.lock().unwrap(),
        Some((1, 2)),
        "(uploads, parts) before the error"
    );
    assert_eq!(open_uploads(&client, &bucket).await?, 0);
    assert!(archive.get_stream("inputs/ws/file_cut").await?.is_none());
    Ok(())
}

/// The request future is dropped mid-body (client disconnect): the open
/// multipart upload is still aborted.
#[tokio::test(flavor = "multi_thread")]
async fn s3_put_stream_dropped_aborts_multipart() -> Result<()> {
    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let client = minio::s3_client(&endpoint);
    create_bucket(&client, &bucket).await?;
    let archive = std::sync::Arc::new(S3BlobArchive::from_client(client.clone(), bucket.clone()));

    let data = pattern(S3_PART_BYTES + 64 * 1024);
    let body =
        futures_util::stream::iter(pieces(&data, 64 * 1024)).chain(futures_util::stream::pending());
    let task = {
        let archive = archive.clone();
        tokio::spawn(async move {
            archive
                .put_stream("inputs/ws/file_drop", "text/plain", Box::pin(body))
                .await
        })
    };
    let mut started = false;
    for _ in 0..200 {
        if open_uploads(&client, &bucket).await? == 1 {
            started = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(started, "the multipart upload never started");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let mut left = usize::MAX;
    for _ in 0..200 {
        left = open_uploads(&client, &bucket).await?;
        if left == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(left, 0, "the dropped upload was not aborted");
    assert!(archive.get_stream("inputs/ws/file_drop").await?.is_none());
    Ok(())
}

/// One request: `"METHOD uri"` and the names of its checksum headers.
type SeenRequest = (String, Vec<String>);

/// Records, per request other than GET/HEAD, the names of its checksum
/// headers (`x-amz-checksum-*`, `x-amz-sdk-checksum-algorithm`, …).
#[derive(Debug, Clone, Default)]
struct ChecksumHeaders(std::sync::Arc<std::sync::Mutex<Vec<SeenRequest>>>);

impl aws_sdk_s3::config::Intercept for ChecksumHeaders {
    fn name(&self) -> &'static str {
        "ChecksumHeaders"
    }

    fn read_before_transmit(
        &self,
        context: &aws_sdk_s3::config::interceptors::BeforeTransmitInterceptorContextRef<'_>,
        _runtime_components: &aws_sdk_s3::config::RuntimeComponents,
        _cfg: &mut aws_sdk_s3::config::ConfigBag,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = context.request();
        if matches!(request.method(), "GET" | "HEAD") {
            return Ok(());
        }
        let names: Vec<String> = request
            .headers()
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .filter(|name| name.contains("checksum"))
            .collect();
        let what = format!("{} {}", request.method(), request.uri());
        self.0.lock().unwrap().push((what, names));
        Ok(())
    }
}

/// Under `request_checksum_calculation = WhenRequired` (AWS's advice for
/// third-party S3-compatible stores) a multipart upload names no checksum
/// algorithm anywhere; under the default it uses CRC32 throughout. Both
/// round-trip.
#[tokio::test]
async fn s3_put_stream_multipart_follows_the_client_checksum_setting() -> Result<()> {
    use aws_sdk_s3::config::RequestChecksumCalculation;

    let (_container, endpoint) = minio::start().await?;
    let bucket = format!("test-{}", Uuid::new_v4());
    let base = minio::s3_client(&endpoint);
    create_bucket(&base, &bucket).await?;
    let data = pattern(S3_PART_BYTES + 3 * MIB);

    for (setting, key) in [
        (None, "inputs/ws/file_default"),
        (
            Some(RequestChecksumCalculation::WhenRequired),
            "inputs/ws/file_when_required",
        ),
    ] {
        let seen = ChecksumHeaders::default();
        let mut config = base.config().to_builder().interceptor(seen.clone());
        if let Some(setting) = setting {
            config = config.request_checksum_calculation(setting);
        }
        let client = aws_sdk_s3::Client::from_conf(config.build());
        let archive = S3BlobArchive::from_client(client, bucket.clone());

        archive
            .put_stream(key, "text/plain", body_of(pieces(&data, 64 * 1024)))
            .await?;
        let tag = etag(&base, &bucket, key).await?;
        assert!(
            tag.trim_matches('"').ends_with("-2"),
            "multipart, ETag {tag}"
        );
        assert!(
            read_back(&archive, key).await?.1 == data,
            "{key}: content differs"
        );

        let requests = seen.0.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            4,
            "create, 2 parts, complete: {requests:#?}"
        );
        if setting.is_some() {
            assert!(
                requests.iter().all(|(_, names)| names.is_empty()),
                "WhenRequired must name no checksum: {requests:#?}"
            );
        } else {
            assert!(
                requests[..3].iter().all(|(_, names)| !names.is_empty()),
                "the default declares CRC32 on create and every part: {requests:#?}"
            );
        }
    }
    assert_eq!(open_uploads(&base, &bucket).await?, 0);
    Ok(())
}
