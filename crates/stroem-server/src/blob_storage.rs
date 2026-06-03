use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// A retrieved blob plus its content type.
#[derive(Debug, Clone)]
pub struct Blob {
    pub content_type: String,
    pub bytes: Bytes,
}

/// Unified pluggable storage for logs, state snapshots, and artifacts.
///
/// Keys are opaque slash-separated paths; each consumer owns its key namespace.
/// `put`/`get` are the primary API; streaming overrides exist for callers that
/// need to avoid buffering large payloads (artifacts up to 100 MiB).
#[async_trait]
pub trait BlobArchive: Send + Sync {
    async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Option<Blob>>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn delete_prefix(&self, prefix: &str) -> Result<()>;

    async fn put_stream(
        &self,
        key: &str,
        content_type: &str,
        body: BoxStream<'static, Result<Bytes>>,
    ) -> Result<()> {
        let mut chunks: Vec<Bytes> = Vec::new();
        let mut stream = body;
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk?);
        }
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        let mut combined = Vec::with_capacity(total);
        for c in &chunks {
            combined.extend_from_slice(c);
        }
        self.put(key, content_type, Bytes::from(combined)).await
    }

    async fn get_stream(
        &self,
        key: &str,
    ) -> Result<Option<(String, BoxStream<'static, Result<Bytes>>)>> {
        match self.get(key).await? {
            None => Ok(None),
            Some(blob) => {
                let one_shot = futures_util::stream::once(async move { Ok(blob.bytes) });
                Ok(Some((blob.content_type, Box::pin(one_shot))))
            }
        }
    }
}

/// Local-filesystem `BlobArchive` backend. Keys map directly to relative paths
/// under `root`. Content type is stored alongside the blob as `<file>.ct`.
pub struct LocalBlobArchive {
    root: PathBuf,
}

impl LocalBlobArchive {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn validate_key(key: &str) -> Result<()> {
        if key.is_empty() || key.starts_with('/') {
            anyhow::bail!("invalid key: {key}");
        }
        for part in key.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                anyhow::bail!("invalid key: {key}");
            }
            if part.contains('\0') {
                anyhow::bail!("invalid key: {key}");
            }
        }
        Ok(())
    }

    fn path_for(&self, key: &str) -> Result<PathBuf> {
        Self::validate_key(key)?;
        Ok(self.root.join(key))
    }

    /// Sidecar path: append `.ct` to the full file name so `foo.tar.gz`
    /// becomes `foo.tar.gz.ct` (not `foo.tar.ct`, which would collide with
    /// a real blob named `foo.tar`).
    fn ct_path_for(path: &Path) -> PathBuf {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        path.with_file_name(format!("{file_name}.ct"))
    }
}

#[async_trait]
impl BlobArchive for LocalBlobArchive {
    async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create parent dir for {}", path.display()))?;
        }
        let ct_path = Self::ct_path_for(&path);

        // Crash-safe write: stream to per-call uniquely-suffixed temp files,
        // fsync each, then atomically rename both into the final names. On any
        // error before the rename, both temp files are unlinked so we never
        // leave torn data or stale sidecars in place.
        let suffix = Uuid::new_v4().simple().to_string();
        let tmp_data = path.with_file_name(format!(
            "{}.tmp.{suffix}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        let tmp_ct = ct_path.with_file_name(format!(
            "{}.tmp.{suffix}",
            ct_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));

        let write_result = async {
            // Data temp file: write, flush, fsync.
            let mut f = fs::File::create(&tmp_data)
                .await
                .with_context(|| format!("create temp {}", tmp_data.display()))?;
            f.write_all(&data)
                .await
                .with_context(|| format!("write temp {}", tmp_data.display()))?;
            f.flush()
                .await
                .with_context(|| format!("flush temp {}", tmp_data.display()))?;
            f.sync_all()
                .await
                .with_context(|| format!("fsync temp {}", tmp_data.display()))?;
            drop(f);

            // Sidecar temp file: write, flush, fsync.
            let mut g = fs::File::create(&tmp_ct)
                .await
                .with_context(|| format!("create temp {}", tmp_ct.display()))?;
            g.write_all(content_type.as_bytes())
                .await
                .with_context(|| format!("write temp {}", tmp_ct.display()))?;
            g.flush()
                .await
                .with_context(|| format!("flush temp {}", tmp_ct.display()))?;
            g.sync_all()
                .await
                .with_context(|| format!("fsync temp {}", tmp_ct.display()))?;
            drop(g);

            // Atomic renames into the published names. The data rename comes
            // first so that even if the sidecar rename fails, readers see
            // fresh bytes with at worst a stale content-type (preferable to
            // a stale blob with a fresh content-type).
            fs::rename(&tmp_data, &path)
                .await
                .with_context(|| format!("rename {} -> {}", tmp_data.display(), path.display()))?;
            fs::rename(&tmp_ct, &ct_path)
                .await
                .with_context(|| format!("rename {} -> {}", tmp_ct.display(), ct_path.display()))?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if write_result.is_err() {
            // Best-effort cleanup of whichever temp file(s) still exist. We
            // ignore NotFound (the rename above may have already consumed it)
            // and other errors (the original write error is what matters).
            let _ = fs::remove_file(&tmp_data).await;
            let _ = fs::remove_file(&tmp_ct).await;
        }
        write_result
    }

    async fn get(&self, key: &str) -> Result<Option<Blob>> {
        let path = self.path_for(key)?;
        let bytes = match fs::read(&path).await {
            Ok(b) => Bytes::from(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let ct_path = Self::ct_path_for(&path);
        let content_type = fs::read_to_string(&ct_path)
            .await
            .unwrap_or_else(|_| "application/octet-stream".to_string());
        Ok(Some(Blob {
            content_type,
            bytes,
        }))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path_for(key)?;
        let ct_path = Self::ct_path_for(&path);
        let _ = fs::remove_file(&ct_path).await;
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        Self::validate_key(prefix.trim_end_matches('/'))?;
        let dir = self.root.join(prefix);
        match fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(feature = "s3")]
pub use s3_blob_archive::S3BlobArchive;

#[cfg(feature = "s3")]
mod s3_blob_archive {
    use super::*;
    use crate::config::ArchiveConfig;
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::types::{Delete, ObjectIdentifier};

    pub struct S3BlobArchive {
        client: aws_sdk_s3::Client,
        bucket: String,
    }

    impl S3BlobArchive {
        pub async fn from_config(config: &ArchiveConfig) -> Result<Self> {
            let region = config
                .region
                .as_deref()
                .context("S3 blob archive requires 'region'")?;
            let bucket = config
                .bucket
                .as_deref()
                .context("S3 blob archive requires 'bucket'")?
                .to_string();

            let mut builder = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new(region.to_string()));
            if let Some(ref endpoint) = config.endpoint {
                builder = builder.endpoint_url(endpoint);
            }
            let cfg = builder.load().await;
            let s3_cfg = aws_sdk_s3::config::Builder::from(&cfg)
                .force_path_style(config.endpoint.is_some())
                .build();
            let client = aws_sdk_s3::Client::from_conf(s3_cfg);
            tracing::info!("S3 blob archive enabled: bucket={}", bucket);
            Ok(Self { client, bucket })
        }

        pub fn from_client(client: aws_sdk_s3::Client, bucket: String) -> Self {
            Self { client, bucket }
        }
    }

    #[async_trait]
    impl BlobArchive for S3BlobArchive {
        async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type(content_type)
                .body(ByteStream::from(data))
                .send()
                .await
                .with_context(|| format!("S3 PUT {key}"))?;
            Ok(())
        }

        async fn get(&self, key: &str) -> Result<Option<Blob>> {
            let out = match self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    if matches!(e.as_service_error(), Some(GetObjectError::NoSuchKey(_))) {
                        return Ok(None);
                    }
                    return Err(anyhow::anyhow!(e)).context(format!("S3 GET {key}"));
                }
            };
            let content_type = out
                .content_type()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let body = out
                .body
                .collect()
                .await
                .with_context(|| format!("S3 read body {key}"))?
                .into_bytes();
            Ok(Some(Blob {
                content_type,
                bytes: body,
            }))
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .with_context(|| format!("S3 DELETE {key}"))?;
            Ok(())
        }

        async fn delete_prefix(&self, prefix: &str) -> Result<()> {
            let mut continuation: Option<String> = None;
            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&self.bucket)
                    .prefix(prefix);
                if let Some(c) = continuation.clone() {
                    req = req.continuation_token(c);
                }
                let resp = req
                    .send()
                    .await
                    .with_context(|| format!("S3 LIST {prefix}"))?;
                let objects: Vec<ObjectIdentifier> = resp
                    .contents()
                    .iter()
                    .filter_map(|o| {
                        o.key()
                            .map(|k| ObjectIdentifier::builder().key(k).build().unwrap())
                    })
                    .collect();
                if !objects.is_empty() {
                    let delete = Delete::builder()
                        .set_objects(Some(objects))
                        .build()
                        .map_err(|e| anyhow::anyhow!(e))?;
                    self.client
                        .delete_objects()
                        .bucket(&self.bucket)
                        .delete(delete)
                        .send()
                        .await
                        .with_context(|| format!("S3 BATCH DELETE {prefix}"))?;
                }
                if resp.is_truncated().unwrap_or(false) {
                    continuation = resp.next_continuation_token().map(|s| s.to_string());
                } else {
                    break;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    struct InMemoryBlob {
        map: Mutex<HashMap<String, Blob>>,
    }

    impl InMemoryBlob {
        fn new() -> Self {
            Self {
                map: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl BlobArchive for InMemoryBlob {
        async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
            self.map.lock().await.insert(
                key.to_string(),
                Blob {
                    content_type: content_type.to_string(),
                    bytes: data,
                },
            );
            Ok(())
        }
        async fn get(&self, key: &str) -> Result<Option<Blob>> {
            Ok(self.map.lock().await.get(key).cloned())
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.map.lock().await.remove(key);
            Ok(())
        }
        async fn delete_prefix(&self, prefix: &str) -> Result<()> {
            let mut m = self.map.lock().await;
            m.retain(|k, _| !k.starts_with(prefix));
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_put_stream_buffers_into_put() {
        use futures_util::stream;
        let store = InMemoryBlob::new();
        let body: BoxStream<'static, Result<Bytes>> = Box::pin(stream::iter(vec![
            Ok(Bytes::from_static(b"hello ")),
            Ok(Bytes::from_static(b"world")),
        ]));
        store.put_stream("k1", "text/plain", body).await.unwrap();
        let got = store.get("k1").await.unwrap().unwrap();
        assert_eq!(got.content_type, "text/plain");
        assert_eq!(&got.bytes[..], b"hello world");
    }

    #[tokio::test]
    async fn default_get_stream_yields_full_blob() {
        use futures_util::StreamExt;
        let store = InMemoryBlob::new();
        store
            .put("k", "application/octet-stream", Bytes::from_static(b"abc"))
            .await
            .unwrap();
        let (ct, mut s) = store.get_stream("k").await.unwrap().unwrap();
        assert_eq!(ct, "application/octet-stream");
        let chunk = s.next().await.unwrap().unwrap();
        assert_eq!(&chunk[..], b"abc");
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn delete_prefix_removes_matching_keys() {
        let store = InMemoryBlob::new();
        store
            .put("a/x", "t", Bytes::from_static(b"1"))
            .await
            .unwrap();
        store
            .put("a/y", "t", Bytes::from_static(b"2"))
            .await
            .unwrap();
        store
            .put("b/z", "t", Bytes::from_static(b"3"))
            .await
            .unwrap();
        store.delete_prefix("a/").await.unwrap();
        assert!(store.get("a/x").await.unwrap().is_none());
        assert!(store.get("a/y").await.unwrap().is_none());
        assert!(store.get("b/z").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn local_blob_roundtrip_and_prefix_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());

        store
            .put(
                "ws/job1/step/foo.txt",
                "text/plain",
                Bytes::from_static(b"hello"),
            )
            .await
            .unwrap();
        store
            .put(
                "ws/job1/step/bar.png",
                "image/png",
                Bytes::from_static(b"png"),
            )
            .await
            .unwrap();
        store
            .put(
                "ws/job2/step/baz.txt",
                "text/plain",
                Bytes::from_static(b"keep"),
            )
            .await
            .unwrap();

        let got = store.get("ws/job1/step/foo.txt").await.unwrap().unwrap();
        assert_eq!(got.content_type, "text/plain");
        assert_eq!(&got.bytes[..], b"hello");

        store.delete_prefix("ws/job1/").await.unwrap();
        assert!(store.get("ws/job1/step/foo.txt").await.unwrap().is_none());
        assert!(store.get("ws/job1/step/bar.png").await.unwrap().is_none());
        assert!(store.get("ws/job2/step/baz.txt").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn local_blob_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let err = store
            .put("../escape.txt", "text/plain", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid key"), "unexpected: {err}");
    }

    /// Regression: the sidecar path used `with_extension`, which replaced the
    /// final extension. `foo.tar.gz` became `foo.tar.ct` (clobbering an
    /// unrelated `foo.tar` blob). The sidecar must be appended to the *full*
    /// file name so the canonical naming is `foo.tar.gz.ct`.
    #[tokio::test]
    async fn local_blob_sidecar_appends_to_full_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());

        store
            .put(
                "ws/job/step/foo.tar.gz",
                "application/gzip",
                Bytes::from_static(b"gz-bytes"),
            )
            .await
            .unwrap();

        // Sidecar lives at `foo.tar.gz.ct` — not `foo.tar.ct`.
        let sidecar = tmp.path().join("ws/job/step/foo.tar.gz.ct");
        let wrong_sidecar = tmp.path().join("ws/job/step/foo.tar.ct");
        assert!(
            sidecar.exists(),
            "expected sidecar at {}, got missing",
            sidecar.display()
        );
        assert!(
            !wrong_sidecar.exists(),
            "stale sidecar location {} should not exist",
            wrong_sidecar.display()
        );
        let ct_contents = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(ct_contents, "application/gzip");

        // Round-trip still resolves the right sidecar.
        let got = store.get("ws/job/step/foo.tar.gz").await.unwrap().unwrap();
        assert_eq!(got.content_type, "application/gzip");
        assert_eq!(&got.bytes[..], b"gz-bytes");

        // Delete cleans up both files (no orphan sidecar left behind).
        store.delete("ws/job/step/foo.tar.gz").await.unwrap();
        assert!(!sidecar.exists(), "sidecar should be deleted");
        assert!(
            !tmp.path().join("ws/job/step/foo.tar.gz").exists(),
            "blob should be deleted"
        );
    }

    /// Regression: the previous implementation truncated the live blob on
    /// `File::create` *before* writing the new bytes. A crash anywhere between
    /// `create` and the final `flush` left a truncated/half-written file
    /// readable by subsequent `get` calls, while the sidecar still claimed the
    /// old content-type.
    ///
    /// The atomic version writes to a uniquely-suffixed temp file, fsyncs,
    /// then renames into place. We can't literally kill the process inside a
    /// unit test, but we can:
    ///   1. plant a stale `.tmp.*` file (the artifact a previous crash would
    ///      leave behind) and verify it does not poison the next `put`;
    ///   2. confirm the live blob is **never** visible in a half-written state
    ///      — readers either see the prior bytes or the new bytes, never a
    ///      mix — by holding the old put concurrently and observing that the
    ///      live path always returns a complete value.
    #[tokio::test]
    async fn local_blob_put_is_crash_safe_and_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());

        // Initial put — establishes the baseline blob + sidecar.
        store
            .put("ws/j/s/data.bin", "text/plain", Bytes::from_static(b"v1"))
            .await
            .unwrap();
        let baseline = store.get("ws/j/s/data.bin").await.unwrap().unwrap();
        assert_eq!(&baseline.bytes[..], b"v1");
        assert_eq!(baseline.content_type, "text/plain");

        // Simulate a prior crash mid-put: plant stale temp files at the
        // *same directory* under the same naming scheme. A correct atomic
        // implementation uses a per-call UUID suffix, so these stale files
        // must not interfere with subsequent puts.
        let stale_data = tmp.path().join("ws/j/s/data.bin.tmp.deadbeefcafebabe");
        let stale_ct = tmp.path().join("ws/j/s/data.bin.ct.tmp.deadbeefcafebabe");
        std::fs::write(&stale_data, b"GARBAGE_FROM_PREVIOUS_CRASH").unwrap();
        std::fs::write(&stale_ct, b"application/x-corrupt").unwrap();

        // Re-put with new bytes — must succeed and must not consume the
        // stale temps. Reader sees the new values, never a partial blend.
        store
            .put(
                "ws/j/s/data.bin",
                "application/octet-stream",
                Bytes::from_static(b"v2-much-longer-than-v1"),
            )
            .await
            .unwrap();
        let after = store.get("ws/j/s/data.bin").await.unwrap().unwrap();
        assert_eq!(&after.bytes[..], b"v2-much-longer-than-v1");
        assert_eq!(after.content_type, "application/octet-stream");

        // Stale temp files are still present (the new put used a different
        // UUID suffix and did not touch them). They are inert — they don't
        // affect get and a subsequent delete clears the live pair without
        // touching them.
        assert!(stale_data.exists(), "stale temp must not be consumed");
        assert!(stale_ct.exists(), "stale temp sidecar must not be consumed");

        store.delete("ws/j/s/data.bin").await.unwrap();
        assert!(store.get("ws/j/s/data.bin").await.unwrap().is_none());

        // Atomicity property — repeatedly overwrite while readers race; the
        // live file is published via rename, so every observed read returns
        // a complete value (one of the two written versions, never empty,
        // never partial).
        store
            .put(
                "ws/j/s/race.bin",
                "text/plain",
                Bytes::from_static(b"aaaaaaaaaa"),
            )
            .await
            .unwrap();
        use std::sync::Arc;
        let store = Arc::new(store);
        let writer = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                for i in 0..50u8 {
                    let payload = if i % 2 == 0 {
                        Bytes::from_static(b"aaaaaaaaaa")
                    } else {
                        Bytes::from_static(b"BBBBBBBBBB")
                    };
                    store
                        .put("ws/j/s/race.bin", "text/plain", payload)
                        .await
                        .unwrap();
                }
            })
        };
        let reader = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                for _ in 0..200 {
                    if let Some(blob) = store.get("ws/j/s/race.bin").await.unwrap() {
                        // Every observed value is one of the two complete
                        // payloads — never empty, never half written.
                        assert!(
                            &blob.bytes[..] == b"aaaaaaaaaa" || &blob.bytes[..] == b"BBBBBBBBBB",
                            "torn read observed: {:?}",
                            &blob.bytes[..]
                        );
                    }
                    tokio::task::yield_now().await;
                }
            })
        };
        writer.await.unwrap();
        reader.await.unwrap();
    }
}
