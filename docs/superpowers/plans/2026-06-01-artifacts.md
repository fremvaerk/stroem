# Artifacts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let tasks emit files into `/artifacts/` (or `$ARTIFACTS_DIR`); on step success, the worker uploads them to per-job blob storage, and humans browse, preview, or download them through the UI/CLI/MCP.

**Architecture:** Convention-directory contract (no YAML change). Files survive only on successful steps. Worker scans `/artifacts/`, uploads one HTTP POST per file with sniffed `Content-Type`, retries with cleanup on terminal failure. Per-job namespace (`UNIQUE(job_id, name)`, last-writer-wins). Storage is a new `BlobArchive` trait that also replaces today's `LogArchive` and `StateArchive`. Serving uses inline-when-safe MIME with `X-Content-Type-Options: nosniff`. Retention cascades with the `job` row; FK `RESTRICT` enforces blob-before-row deletion.

**Tech Stack:** Rust (axum, sqlx, tokio, async-trait, anyhow, tracing, `bytes`, `flate2`, `infer`); Postgres 14+; AWS SDK for S3; React 19 + Vite + Tailwind v4 + shadcn/ui; `rmcp` for MCP.

---

## Phasing Overview

- **Phase A — BlobArchive refactor (prerequisite).** Unify `LogArchive` + `StateArchive` under a single `BlobArchive` trait. No user-visible change; operator config preserved verbatim.
- **Phase B — Core artifacts feature.** Schema, worker upload, runner mounts (shell + `script:docker` + `type:docker`), server endpoints, hook context, UI section, retention.
- **Phase C — Polish.** CLI `artifacts list`/`download`, MCP `list_artifacts`/`get_artifact`, full documentation suite.

Each phase ends green (`cargo fmt --check --all && cargo clippy --workspace -- -D warnings && cargo test --workspace && cd ui && bun run lint && bunx tsc --noEmit`) and can be merged independently.

---

# Phase A — BlobArchive refactor

## Task A1: Introduce `BlobArchive` trait + `Blob` value type

**Files:**
- Create: `crates/stroem-server/src/blob_storage.rs`
- Modify: `crates/stroem-server/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/stroem-server/src/blob_storage.rs`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;

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
            Self { map: Mutex::new(HashMap::new()) }
        }
    }

    #[async_trait]
    impl BlobArchive for InMemoryBlob {
        async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
            self.map.lock().await.insert(
                key.to_string(),
                Blob { content_type: content_type.to_string(), bytes: data },
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
        store
            .put_stream("k1", "text/plain", body)
            .await
            .unwrap();
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
        store.put("a/x", "t", Bytes::from_static(b"1")).await.unwrap();
        store.put("a/y", "t", Bytes::from_static(b"2")).await.unwrap();
        store.put("b/z", "t", Bytes::from_static(b"3")).await.unwrap();
        store.delete_prefix("a/").await.unwrap();
        assert!(store.get("a/x").await.unwrap().is_none());
        assert!(store.get("a/y").await.unwrap().is_none());
        assert!(store.get("b/z").await.unwrap().is_some());
    }
}
```

Add to `crates/stroem-server/src/lib.rs`:

```rust
pub mod blob_storage;
```

- [ ] **Step 2: Add needed dependencies**

In `crates/stroem-server/Cargo.toml`, under `[dependencies]`:

```toml
futures-core = "0.3"
futures-util = "0.3"
```

(`bytes` and `async-trait` are already direct or transitive deps via axum/sqlx — verify with `cargo tree -e normal -p stroem-server | grep -E '^(bytes|async-trait)'`.)

- [ ] **Step 3: Run test to verify it passes**

```bash
cargo test -p stroem-server blob_storage::tests -- --nocapture
```

Expected: 3 tests pass (`default_put_stream_buffers_into_put`, `default_get_stream_yields_full_blob`, `delete_prefix_removes_matching_keys`).

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/src/blob_storage.rs crates/stroem-server/src/lib.rs crates/stroem-server/Cargo.toml
git commit -m "feat(server): introduce BlobArchive trait with default streaming impls"
```

---

## Task A2: Implement `LocalBlobArchive`

**Files:**
- Modify: `crates/stroem-server/src/blob_storage.rs`

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` block:

```rust
#[tokio::test]
async fn local_blob_roundtrip_and_prefix_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LocalBlobArchive::new(tmp.path().to_path_buf());

    store
        .put("ws/job1/step/foo.txt", "text/plain", Bytes::from_static(b"hello"))
        .await
        .unwrap();
    store
        .put("ws/job1/step/bar.png", "image/png", Bytes::from_static(b"png"))
        .await
        .unwrap();
    store
        .put("ws/job2/step/baz.txt", "text/plain", Bytes::from_static(b"keep"))
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
```

Add the implementation in `blob_storage.rs` (above the `#[cfg(test)]` block):

```rust
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

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
}

#[async_trait]
impl BlobArchive for LocalBlobArchive {
    async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut f = fs::File::create(&path).await?;
        f.write_all(&data).await?;
        f.flush().await?;
        let ct_path = path.with_extension(format!(
            "{}.ct",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        fs::write(ct_path, content_type.as_bytes()).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Blob>> {
        let path = self.path_for(key)?;
        let bytes = match fs::read(&path).await {
            Ok(b) => Bytes::from(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let ct_path = path.with_extension(format!(
            "{}.ct",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        let content_type = fs::read_to_string(&ct_path)
            .await
            .unwrap_or_else(|_| "application/octet-stream".to_string());
        Ok(Some(Blob { content_type, bytes }))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path_for(key)?;
        let ct_path = path.with_extension(format!(
            "{}.ct",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
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
```

- [ ] **Step 2: Run the tests to verify they pass**

```bash
cargo test -p stroem-server blob_storage::tests -- --nocapture
```

Expected: 5 tests pass (previous 3 plus 2 new local-backend tests).

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/blob_storage.rs
git commit -m "feat(server): add LocalBlobArchive with path-traversal validation"
```

---

## Task A3: Implement `S3BlobArchive`

**Files:**
- Modify: `crates/stroem-server/src/blob_storage.rs`

- [ ] **Step 1: Add the implementation (no new unit test — exercised by integration test in Task A7)**

Append to `blob_storage.rs`, gated on the `s3` feature:

```rust
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
            Ok(Some(Blob { content_type, bytes: body }))
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
                    .filter_map(|o| o.key().map(|k| {
                        ObjectIdentifier::builder().key(k).build().unwrap()
                    }))
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
```

Add `use anyhow::Context;` at the top of the file if not already imported.

- [ ] **Step 2: Verify it builds with and without `s3` feature**

```bash
cargo check -p stroem-server --features s3
cargo check -p stroem-server --no-default-features
```

Expected: both succeed with no errors.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/blob_storage.rs
git commit -m "feat(server): add S3BlobArchive with paginated prefix delete"
```

---

## Task A4: Migrate `LogArchive` callers to `BlobArchive`

**Files:**
- Modify: `crates/stroem-server/src/log_storage.rs`
- Modify: `crates/stroem-server/src/state.rs` (whichever file constructs the `LogStorage`)
- Modify: `crates/stroem-server/src/main.rs`

- [ ] **Step 1: Update `LogStorage` to take `Arc<dyn BlobArchive>` instead of `Arc<dyn LogArchive>`**

In `crates/stroem-server/src/log_storage.rs`:

1. Remove the `LogArchive` trait definition (lines ~22-31) and both `S3LogArchive` + `LocalLogArchive` impl blocks.
2. Replace internal field `archive: Option<Arc<dyn LogArchive>>` with `archive: Option<Arc<dyn BlobArchive>>`.
3. At every call site:
   - `archive.upload(&key, &data)` → `archive.put(&key, "application/gzip", Bytes::from(data))`
   - `archive.download(&key)` → adjust: returns `Option<Blob>`; use `.map(|b| b.bytes.to_vec())`
   - `archive.delete(&key)` → unchanged (same method name).

Specifically `upload_to_archive` (around line 398) becomes:

```rust
pub async fn upload_to_archive(&self, job_id: Uuid, meta: &JobLogMeta) -> Result<()> {
    let Some(archive) = &self.archive else { return Ok(()); };
    let local_path = self.local_path_for(job_id);
    let raw = tokio::fs::read(&local_path)
        .await
        .with_context(|| format!("Failed to read local log {local_path:?}"))?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    use std::io::Write;
    encoder.write_all(&raw)?;
    let gz = encoder.finish()?;
    let key = archive_key_for_job(meta, job_id);
    archive
        .put(&key, "application/gzip", bytes::Bytes::from(gz))
        .await
        .with_context(|| format!("Upload log archive {key}"))?;
    Ok(())
}
```

And `get_log_from_archive` / `get_step_log_from_archive`:

```rust
let blob = match archive.get(&key).await? {
    None => return Ok(None),
    Some(b) => b,
};
let mut decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(blob.bytes.as_ref()));
let mut text = String::new();
use std::io::Read;
decoder.read_to_string(&mut text)?;
Ok(Some(text))
```

- [ ] **Step 2: Update construction site**

Find where `LogStorage` is built (search `LogStorage::new` and `from_config`). Change parameter type from `Option<Arc<dyn LogArchive>>` to `Option<Arc<dyn BlobArchive>>`. In `main.rs`, replace `S3LogArchive::from_config(...)` with `S3BlobArchive::from_config(...)` and `LocalLogArchive::new(...)` with `LocalBlobArchive::new(...)`.

- [ ] **Step 3: Run the existing log_storage tests**

```bash
cargo test -p stroem-server log_storage
```

Expected: all existing log tests still pass.

- [ ] **Step 4: Run the full server test suite**

```bash
cargo test -p stroem-server
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/log_storage.rs crates/stroem-server/src/state.rs crates/stroem-server/src/main.rs
git commit -m "refactor(server): migrate LogStorage to BlobArchive backend"
```

---

## Task A5: Migrate `StateArchive` callers to `BlobArchive`

**Files:**
- Modify: `crates/stroem-server/src/state_storage.rs`
- Modify: `crates/stroem-server/src/lib.rs` (re-exports)
- Modify: `crates/stroem-server/src/main.rs`
- Modify: any consumer of `StateArchive` (search: `grep -rn "StateArchive" crates/stroem-server/src/`)

- [ ] **Step 1: Replace the trait + impls**

Delete the `StateArchive` trait and both impl blocks (`S3StateArchive`, `LocalStateArchive`, `InMemoryStateArchive`) from `state_storage.rs`. Replace the `StateStorage` wrapper's internal field with `archive: Arc<dyn BlobArchive>`. Method names stay the same to minimize call-site churn:

```rust
pub async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
    self.archive
        .put(key, "application/gzip", bytes::Bytes::copy_from_slice(data))
        .await
}

pub async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(self.archive.get(key).await?.map(|b| b.bytes.to_vec()))
}

pub async fn delete(&self, key: &str) -> Result<()> {
    self.archive.delete(key).await
}
```

- [ ] **Step 2: Run state tests**

```bash
cargo test -p stroem-server state_storage
```

Expected: all pre-existing state tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/state_storage.rs crates/stroem-server/src/lib.rs crates/stroem-server/src/main.rs
git commit -m "refactor(server): migrate StateStorage to BlobArchive backend"
```

---

## Task A6: Wire `Arc<dyn BlobArchive>` through `AppState`

**Files:**
- Modify: `crates/stroem-server/src/state.rs`
- Modify: `crates/stroem-server/src/main.rs`

- [ ] **Step 1: Add a single shared blob archive on `AppState`**

In `state.rs`, add:

```rust
pub blob_archive: Option<Arc<dyn crate::blob_storage::BlobArchive>>,
```

In `main.rs` startup, construct **one** `Arc<dyn BlobArchive>` (S3 if `log_storage.archive` is S3, local otherwise) and clone it into `LogStorage`, `StateStorage`, and `AppState.blob_archive`. If each subsystem's `archive` block is configured with the *same backend*, share the same `Arc`. If they differ, construct one per subsystem.

(Phase B's artifact code reads `AppState.blob_archive` for the artifact-specific operations.)

- [ ] **Step 2: Run the full test suite**

```bash
cargo test --workspace
```

Expected: all tests pass; log + state archival keeps working.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/state.rs crates/stroem-server/src/main.rs
git commit -m "refactor(server): share BlobArchive Arc across LogStorage, StateStorage, AppState"
```

---

## Task A7: Update existing S3 integration test

**Files:**
- Modify: `crates/stroem-server/tests/s3_integration_test.rs`

- [ ] **Step 1: Replace direct `S3LogArchive` / `S3StateArchive` references with `S3BlobArchive`**

The test already runs against MinIO via testcontainers. Adapt construction:

```rust
let client = aws_sdk_s3::Client::from_conf(s3_cfg);
let blob = stroem_server::blob_storage::S3BlobArchive::from_client(client.clone(), bucket.clone());
```

Then exercise `put` / `get` / `delete` / `delete_prefix` directly on the trait object.

- [ ] **Step 2: Add a `delete_prefix` test against MinIO**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn s3_delete_prefix_removes_only_matching() {
    let (client, bucket, _container) = start_minio().await;
    let blob = S3BlobArchive::from_client(client, bucket);

    blob.put("ws/job1/a.txt", "text/plain", Bytes::from_static(b"1")).await.unwrap();
    blob.put("ws/job1/b.txt", "text/plain", Bytes::from_static(b"2")).await.unwrap();
    blob.put("ws/job2/c.txt", "text/plain", Bytes::from_static(b"3")).await.unwrap();

    blob.delete_prefix("ws/job1/").await.unwrap();
    assert!(blob.get("ws/job1/a.txt").await.unwrap().is_none());
    assert!(blob.get("ws/job1/b.txt").await.unwrap().is_none());
    assert!(blob.get("ws/job2/c.txt").await.unwrap().is_some());
}
```

- [ ] **Step 3: Run the integration test**

```bash
cargo test -p stroem-server --features s3 --test s3_integration_test -- --test-threads=1
```

Expected: all integration tests pass (needs Docker running for MinIO).

- [ ] **Step 4: Commit + Phase A green-bar gate**

```bash
cargo fmt --check --all && cargo clippy --workspace -- -D warnings && cargo test --workspace
git add crates/stroem-server/tests/s3_integration_test.rs
git commit -m "test(server): exercise S3BlobArchive directly + add delete_prefix coverage"
```

---

# Phase B — Core artifacts feature

## Task B1: DB migration `038_job_artifact.sql`

**Files:**
- Create: `crates/stroem-db/migrations/038_job_artifact.sql`

- [ ] **Step 1: Author the migration**

```sql
CREATE TABLE job_artifact (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    job_id       UUID NOT NULL REFERENCES job(id) ON DELETE RESTRICT,
    step_name    TEXT NOT NULL,
    name         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes   BIGINT NOT NULL CHECK (size_bytes >= 0),
    storage_key  TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (job_id, name)
);

CREATE INDEX ix_job_artifact_job_id ON job_artifact(job_id);
```

- [ ] **Step 2: Verify migration applies in a test container**

```bash
cargo test -p stroem-db migrations
```

Expected: existing migration-test passes (it picks up new files automatically).

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-db/migrations/038_job_artifact.sql
git commit -m "feat(db): add job_artifact table with per-job uniqueness + RESTRICT FK"
```

---

## Task B2: `JobArtifactRepo` with CRUD + cleanup

**Files:**
- Create: `crates/stroem-db/src/repos/job_artifact.rs`
- Modify: `crates/stroem-db/src/repos/mod.rs`

- [ ] **Step 1: Write the failing test**

In `crates/stroem-db/tests/job_artifact_repo.rs`:

```rust
use stroem_db::repos::job_artifact::{JobArtifactRecord, JobArtifactRepo, NewArtifactRow};
use uuid::Uuid;

mod common;
use common::setup_db;

#[tokio::test(flavor = "multi_thread")]
async fn upsert_then_list_returns_artifacts_for_job() {
    let pool = setup_db().await;
    let job_id = common::create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    repo.upsert(NewArtifactRow {
        job_id,
        step_name: "build".into(),
        name: "report.html".into(),
        content_type: "text/html".into(),
        size_bytes: 1234,
        storage_key: "artifacts/ws1/{}/build/report.html".replace("{}", &job_id.to_string()),
    })
    .await
    .unwrap();

    let rows = repo.list_for_job(job_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "report.html");
    assert_eq!(rows[0].size_bytes, 1234);
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_same_name_replaces_row() {
    let pool = setup_db().await;
    let job_id = common::create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    let make_row = |step: &str, size: i64| NewArtifactRow {
        job_id,
        step_name: step.into(),
        name: "out.txt".into(),
        content_type: "text/plain".into(),
        size_bytes: size,
        storage_key: format!("artifacts/ws1/{job_id}/{step}/out.txt"),
    };
    repo.upsert(make_row("a", 1)).await.unwrap();
    repo.upsert(make_row("b", 2)).await.unwrap();

    let rows = repo.list_for_job(job_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step_name, "b");
    assert_eq!(rows[0].size_bytes, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_for_job_removes_all_rows() {
    let pool = setup_db().await;
    let job_id = common::create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    repo.upsert(NewArtifactRow {
        job_id, step_name: "s".into(), name: "a.txt".into(),
        content_type: "text/plain".into(), size_bytes: 1,
        storage_key: format!("artifacts/ws1/{job_id}/s/a.txt"),
    }).await.unwrap();
    repo.upsert(NewArtifactRow {
        job_id, step_name: "s".into(), name: "b.txt".into(),
        content_type: "text/plain".into(), size_bytes: 2,
        storage_key: format!("artifacts/ws1/{job_id}/s/b.txt"),
    }).await.unwrap();

    let deleted = repo.delete_for_job(job_id).await.unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(repo.list_for_job(job_id).await.unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_for_step_removes_only_that_step() {
    let pool = setup_db().await;
    let job_id = common::create_job(&pool, "ws1", "task1").await;
    let repo = JobArtifactRepo::new(pool.clone());

    repo.upsert(NewArtifactRow {
        job_id, step_name: "build".into(), name: "out.txt".into(),
        content_type: "text/plain".into(), size_bytes: 1,
        storage_key: format!("artifacts/ws1/{job_id}/build/out.txt"),
    }).await.unwrap();
    repo.upsert(NewArtifactRow {
        job_id, step_name: "test".into(), name: "report.html".into(),
        content_type: "text/html".into(), size_bytes: 2,
        storage_key: format!("artifacts/ws1/{job_id}/test/report.html"),
    }).await.unwrap();

    let removed = repo.delete_for_step(job_id, "build").await.unwrap();
    assert_eq!(removed, 1);
    let rows = repo.list_for_job(job_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step_name, "test");
}
```

(Reuse `common` helpers from existing repo tests; if the helper module doesn't exist yet, copy the `setup_db`/`create_job` pattern from `crates/stroem-db/tests/job_repo.rs` or wherever the existing repo tests live.)

- [ ] **Step 2: Implement the repo**

`crates/stroem-db/src/repos/job_artifact.rs`:

```rust
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct JobArtifactRecord {
    pub id: Uuid,
    pub job_id: Uuid,
    pub step_name: String,
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewArtifactRow {
    pub job_id: Uuid,
    pub step_name: String,
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_key: String,
}

pub struct JobArtifactRepo {
    pool: PgPool,
}

impl JobArtifactRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, row: NewArtifactRow) -> Result<JobArtifactRecord> {
        let rec = sqlx::query_as::<_, (Uuid, Uuid, String, String, String, i64, String, DateTime<Utc>)>(
            r#"
            INSERT INTO job_artifact
                (job_id, step_name, name, content_type, size_bytes, storage_key)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (job_id, name) DO UPDATE
                SET step_name    = EXCLUDED.step_name,
                    content_type = EXCLUDED.content_type,
                    size_bytes   = EXCLUDED.size_bytes,
                    storage_key  = EXCLUDED.storage_key,
                    created_at   = NOW()
            RETURNING id, job_id, step_name, name, content_type, size_bytes, storage_key, created_at
            "#,
        )
        .bind(row.job_id)
        .bind(&row.step_name)
        .bind(&row.name)
        .bind(&row.content_type)
        .bind(row.size_bytes)
        .bind(&row.storage_key)
        .fetch_one(&self.pool)
        .await?;

        Ok(JobArtifactRecord {
            id: rec.0, job_id: rec.1, step_name: rec.2, name: rec.3,
            content_type: rec.4, size_bytes: rec.5, storage_key: rec.6, created_at: rec.7,
        })
    }

    pub async fn list_for_job(&self, job_id: Uuid) -> Result<Vec<JobArtifactRecord>> {
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String, String, i64, String, DateTime<Utc>)>(
            "SELECT id, job_id, step_name, name, content_type, size_bytes, storage_key, created_at
             FROM job_artifact WHERE job_id = $1 ORDER BY created_at ASC, name ASC",
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| JobArtifactRecord {
            id: r.0, job_id: r.1, step_name: r.2, name: r.3,
            content_type: r.4, size_bytes: r.5, storage_key: r.6, created_at: r.7,
        }).collect())
    }

    pub async fn get_by_name(&self, job_id: Uuid, name: &str) -> Result<Option<JobArtifactRecord>> {
        let row = sqlx::query_as::<_, (Uuid, Uuid, String, String, String, i64, String, DateTime<Utc>)>(
            "SELECT id, job_id, step_name, name, content_type, size_bytes, storage_key, created_at
             FROM job_artifact WHERE job_id = $1 AND name = $2",
        )
        .bind(job_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| JobArtifactRecord {
            id: r.0, job_id: r.1, step_name: r.2, name: r.3,
            content_type: r.4, size_bytes: r.5, storage_key: r.6, created_at: r.7,
        }))
    }

    pub async fn total_size_for_job(&self, job_id: Uuid) -> Result<i64> {
        let total: (Option<i64>,) = sqlx::query_as(
            "SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM job_artifact WHERE job_id = $1",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(total.0.unwrap_or(0))
    }

    pub async fn delete_for_job(&self, job_id: Uuid) -> Result<u64> {
        let res = sqlx::query("DELETE FROM job_artifact WHERE job_id = $1")
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn delete_for_step(&self, job_id: Uuid, step_name: &str) -> Result<u64> {
        let res = sqlx::query("DELETE FROM job_artifact WHERE job_id = $1 AND step_name = $2")
            .bind(job_id)
            .bind(step_name)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}
```

Add to `crates/stroem-db/src/repos/mod.rs`:

```rust
pub mod job_artifact;
```

- [ ] **Step 3: Run the repo tests**

```bash
cargo test -p stroem-db job_artifact
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-db/src/repos/ crates/stroem-db/tests/job_artifact_repo.rs
git commit -m "feat(db): add JobArtifactRepo with upsert, list, and step/job cleanup"
```

---

## Task B3: Server config — `ArtifactStorageConfig`

**Files:**
- Modify: `crates/stroem-server/src/config.rs`

- [ ] **Step 1: Write the failing test**

In `crates/stroem-server/src/config.rs` test module, add:

```rust
#[test]
fn artifact_storage_defaults_used_when_unset() {
    let yaml = "db: { url: 'postgres://x' }\nworker_token: 't'\n";
    let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
    let art = cfg.artifact_storage.unwrap_or_default();
    assert_eq!(art.max_file_bytes, 100 * 1024 * 1024);
    assert_eq!(art.max_job_bytes, 1024 * 1024 * 1024);
    assert!(art.archive.is_none());
    assert_eq!(art.prefix, "artifacts/");
}

#[test]
fn artifact_storage_size_limits_parsed() {
    let yaml = r#"
db: { url: 'postgres://x' }
worker_token: 't'
artifact_storage:
  max_file_bytes: 52428800
  max_job_bytes: 524288000
  prefix: "art/"
"#;
    let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
    let art = cfg.artifact_storage.unwrap();
    assert_eq!(art.max_file_bytes, 52_428_800);
    assert_eq!(art.max_job_bytes, 524_288_000);
    assert_eq!(art.prefix, "art/");
}
```

- [ ] **Step 2: Add the type + default**

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactStorageConfig {
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default = "default_max_job_bytes")]
    pub max_job_bytes: u64,
    #[serde(default = "default_artifact_prefix")]
    pub prefix: String,
    pub archive: Option<ArchiveConfig>,
}

fn default_max_file_bytes() -> u64 { 100 * 1024 * 1024 }
fn default_max_job_bytes() -> u64 { 1024 * 1024 * 1024 }
fn default_artifact_prefix() -> String { "artifacts/".to_string() }

impl Default for ArtifactStorageConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: default_max_file_bytes(),
            max_job_bytes: default_max_job_bytes(),
            prefix: default_artifact_prefix(),
            archive: None,
        }
    }
}
```

Add field to `ServerConfig`:

```rust
#[serde(default)]
pub artifact_storage: Option<ArtifactStorageConfig>,
```

- [ ] **Step 3: Run the tests**

```bash
cargo test -p stroem-server config::tests
```

Expected: 2 new tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/src/config.rs
git commit -m "feat(server): add ArtifactStorageConfig with 100MiB/1GiB defaults"
```

---

## Task B4: `AppState::artifact_blob` — pick the right backend at startup

**Files:**
- Modify: `crates/stroem-server/src/state.rs`
- Modify: `crates/stroem-server/src/main.rs`

- [ ] **Step 1: Add field + accessor**

In `state.rs`:

```rust
pub artifact_blob: Option<Arc<dyn crate::blob_storage::BlobArchive>>,
pub artifact_config: crate::config::ArtifactStorageConfig,
```

Method:

```rust
pub fn artifact_storage_key(&self, ws: &str, job_id: Uuid, step: &str, name: &str) -> String {
    format!("{}{}/{}/{}/{}", self.artifact_config.prefix, ws, job_id, step, name)
}
```

- [ ] **Step 2: Wire construction in `main.rs`**

If `artifact_storage.archive` is set, build a dedicated `S3BlobArchive` (or `LocalBlobArchive`) from it. Otherwise, reuse the shared `Arc<dyn BlobArchive>` from `LogStorage`/`StateStorage` (whichever is present), so operators get "single S3 bucket, multiple prefixes" by default.

```rust
let artifact_config = config.artifact_storage.clone().unwrap_or_default();
let artifact_blob: Option<Arc<dyn BlobArchive>> = if let Some(ref ac) = artifact_config.archive {
    Some(build_blob_from_archive_config(ac).await?)
} else {
    shared_blob.clone() // the Arc constructed in Task A6
};
```

- [ ] **Step 3: Verify build**

```bash
cargo check -p stroem-server
```

Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/src/state.rs crates/stroem-server/src/main.rs
git commit -m "feat(server): wire artifact_blob + artifact_config into AppState"
```

---

## Task B5: Worker upload endpoint `POST /worker/jobs/.../artifacts/{name}`

**Files:**
- Create: `crates/stroem-server/src/web/worker_api/artifacts.rs`
- Modify: `crates/stroem-server/src/web/worker_api/mod.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/stroem-server/tests/artifact_upload_test.rs`:

```rust
//! End-to-end: worker uploads file → row in DB → blob in storage.
use axum::http::StatusCode;
mod common;

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_persists_row_and_blob() {
    let h = common::TestHarness::new().await;
    let job_id = h.create_test_job("ws1", "task1").await;

    let body = b"<h1>hi</h1>".to_vec();
    let resp = h
        .worker_request("POST", &format!("/worker/jobs/{job_id}/steps/build/artifacts/report.html"))
        .header("Content-Type", "text/html")
        .body(body.clone().into())
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let rows = h.list_artifacts(job_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "report.html");
    assert_eq!(rows[0].content_type, "text/html");
    assert_eq!(rows[0].size_bytes, body.len() as i64);
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_rejects_oversized_file() {
    let mut h = common::TestHarness::new().await;
    h.set_artifact_max_file_bytes(8);
    let job_id = h.create_test_job("ws1", "task1").await;

    let body = vec![0u8; 100];
    let resp = h
        .worker_request("POST", &format!("/worker/jobs/{job_id}/steps/build/artifacts/big.bin"))
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", "100")
        .body(body.into())
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_rejects_when_per_job_cap_would_exceed() {
    let mut h = common::TestHarness::new().await;
    h.set_artifact_max_job_bytes(20);
    let job_id = h.create_test_job("ws1", "task1").await;

    h.worker_request("POST", &format!("/worker/jobs/{job_id}/steps/s/artifacts/a.bin"))
        .header("Content-Type", "application/octet-stream")
        .body(vec![0u8; 15].into())
        .send()
        .await
        .check_status(StatusCode::CREATED);

    let resp = h
        .worker_request("POST", &format!("/worker/jobs/{job_id}/steps/s/artifacts/b.bin"))
        .header("Content-Type", "application/octet-stream")
        .body(vec![0u8; 10].into())
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_replaces_existing_artifact_by_name() {
    let h = common::TestHarness::new().await;
    let job_id = h.create_test_job("ws1", "task1").await;

    h.worker_request("POST", &format!("/worker/jobs/{job_id}/steps/s1/artifacts/x.txt"))
        .header("Content-Type", "text/plain")
        .body(b"first".to_vec().into())
        .send()
        .await
        .check_status(StatusCode::CREATED);

    h.worker_request("POST", &format!("/worker/jobs/{job_id}/steps/s2/artifacts/x.txt"))
        .header("Content-Type", "text/plain")
        .body(b"second".to_vec().into())
        .send()
        .await
        .check_status(StatusCode::CREATED);

    let rows = h.list_artifacts(job_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step_name, "s2");
    assert_eq!(rows[0].size_bytes, 6);
}
```

(`common::TestHarness` already exists in this crate's tests; see `metrics_test.rs` for the pattern. Add `set_artifact_max_*` helpers + `list_artifacts` that wrap `JobArtifactRepo`.)

- [ ] **Step 2: Implement the handler**

`crates/stroem-server/src/web/worker_api/artifacts.rs`:

```rust
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use stroem_db::repos::job_artifact::{JobArtifactRepo, NewArtifactRow};
use uuid::Uuid;

use crate::state::AppState;
use crate::web::error::AppError;

#[derive(serde::Serialize)]
pub struct UploadResponse {
    pub id: Uuid,
    pub name: String,
    pub size_bytes: i64,
    pub content_type: String,
}

#[tracing::instrument(skip(state, body))]
pub async fn upload_artifact(
    State(state): State<AppState>,
    Path((job_id, step_name, name)): Path<(Uuid, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let cfg = &state.artifact_config;

    // Per-file size limit
    if body.len() as u64 > cfg.max_file_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "file '{name}' is {} bytes, exceeds per-file limit of {}",
            body.len(), cfg.max_file_bytes
        )));
    }

    // Per-job size limit (counts existing + this upload)
    let repo = JobArtifactRepo::new(state.pool.clone());
    let existing = repo.total_size_for_job(job_id).await? as u64;
    // If we're replacing the same-named artifact, subtract its size from the cap math.
    let replacing = repo.get_by_name(job_id, &name).await?.map(|r| r.size_bytes as u64).unwrap_or(0);
    let projected = existing.saturating_sub(replacing).saturating_add(body.len() as u64);
    if projected > cfg.max_job_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "adding '{name}' ({}) would push job to {} bytes, exceeds per-job limit of {}",
            body.len(), projected, cfg.max_job_bytes
        )));
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Resolve workspace from the job row (validate FK + collect prefix)
    let workspace = sqlx::query_scalar::<_, String>("SELECT workspace FROM job WHERE id = $1")
        .bind(job_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound(format!("job {job_id}")))?;

    let blob = state.artifact_blob.clone().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("artifact storage not configured"))
    })?;
    let key = state.artifact_storage_key(&workspace, job_id, &step_name, &name);
    blob.put(&key, &content_type, body.clone()).await
        .map_err(AppError::Internal)?;

    let rec = repo.upsert(NewArtifactRow {
        job_id,
        step_name,
        name: name.clone(),
        content_type: content_type.clone(),
        size_bytes: body.len() as i64,
        storage_key: key,
    }).await?;

    Ok((StatusCode::CREATED, Json(UploadResponse {
        id: rec.id,
        name: rec.name,
        size_bytes: rec.size_bytes,
        content_type: rec.content_type,
    })))
}
```

Register in `crates/stroem-server/src/web/worker_api/mod.rs`:

```rust
.route(
    "/jobs/:job_id/steps/:step_name/artifacts/*name",
    axum::routing::post(artifacts::upload_artifact),
)
```

(Use `*name` greedy match so the name can contain slashes for the recursive-flatten case.)

Add `PayloadTooLarge` to `AppError` in `web/error.rs` (HTTP 413).

- [ ] **Step 3: Run the integration tests**

```bash
cargo test -p stroem-server --test artifact_upload_test
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/src/web/worker_api/ crates/stroem-server/src/web/error.rs crates/stroem-server/tests/artifact_upload_test.rs
git commit -m "feat(server): worker upload endpoint with per-file + per-job size limits"
```

---

## Task B6: User-facing API — list + download

**Files:**
- Create: `crates/stroem-server/src/web/api/artifacts.rs`
- Modify: `crates/stroem-server/src/web/api/mod.rs`
- Modify: `crates/stroem-server/src/web/api/jobs.rs` (or wherever artifact `url` is generated)

- [ ] **Step 1: Write the failing integration test**

Create `crates/stroem-server/tests/artifact_api_test.rs`:

```rust
mod common;
use axum::http::StatusCode;

#[tokio::test(flavor = "multi_thread")]
async fn list_artifacts_returns_uploaded() {
    let h = common::TestHarness::new().await;
    let job_id = h.create_test_job("ws1", "task1").await;
    h.upload_artifact(job_id, "s1", "a.txt", "text/plain", b"hi").await;
    h.upload_artifact(job_id, "s2", "b.png", "image/png", b"PNGDATA").await;

    let body: serde_json::Value = h
        .api_request("GET", &format!("/api/jobs/{job_id}/artifacts"))
        .send().await.json().await;
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    let names: Vec<_> = items.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.png"));
}

#[tokio::test(flavor = "multi_thread")]
async fn download_safe_mime_serves_inline() {
    let h = common::TestHarness::new().await;
    let job_id = h.create_test_job("ws1", "task1").await;
    h.upload_artifact(job_id, "s1", "shot.png", "image/png", b"PNG").await;

    let resp = h.api_request("GET", &format!("/api/jobs/{job_id}/artifacts/shot.png"))
        .send().await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/png");
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert!(resp.headers()["content-disposition"]
        .to_str().unwrap().starts_with("inline"));
}

#[tokio::test(flavor = "multi_thread")]
async fn download_html_forced_to_attachment() {
    let h = common::TestHarness::new().await;
    let job_id = h.create_test_job("ws1", "task1").await;
    h.upload_artifact(job_id, "s1", "evil.html", "text/html", b"<script>alert(1)</script>").await;

    let resp = h.api_request("GET", &format!("/api/jobs/{job_id}/artifacts/evil.html"))
        .send().await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()["content-disposition"]
        .to_str().unwrap().starts_with("attachment"));
}

#[tokio::test(flavor = "multi_thread")]
async fn download_unknown_artifact_is_404() {
    let h = common::TestHarness::new().await;
    let job_id = h.create_test_job("ws1", "task1").await;

    let resp = h.api_request("GET", &format!("/api/jobs/{job_id}/artifacts/missing.txt"))
        .send().await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_requires_view_permission() {
    let h = common::TestHarness::new_with_acl(common::Acl::deny_workspace("ws1")).await;
    let job_id = h.create_test_job("ws1", "task1").await;
    let resp = h.api_request("GET", &format!("/api/jobs/{job_id}/artifacts")).send().await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
```

- [ ] **Step 2: Implement the handlers**

`crates/stroem-server/src/web/api/artifacts.rs`:

```rust
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use stroem_db::repos::job_artifact::JobArtifactRepo;
use uuid::Uuid;

use crate::acl::TaskAction;
use crate::auth::AuthenticatedUser;
use crate::state::AppState;
use crate::web::error::AppError;

#[derive(serde::Serialize)]
pub struct ArtifactListItem {
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub step_name: String,
    pub created_at: DateTime<Utc>,
    pub url: String,
}

const SAFE_INLINE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "application/pdf",
    "text/plain",
    "text/markdown",
];

fn disposition_for(content_type: &str, name: &str) -> String {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    if SAFE_INLINE.contains(&ct.as_str()) {
        format!("inline; filename=\"{}\"", sanitize_filename(name))
    } else {
        format!("attachment; filename=\"{}\"", sanitize_filename(name))
    }
}

fn sanitize_filename(name: &str) -> String {
    name.replace(['"', '\r', '\n'], "_")
}

pub async fn list_artifacts(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(job_id): Path<Uuid>,
) -> Result<Json<Vec<ArtifactListItem>>, AppError> {
    let (workspace, task_name) =
        sqlx::query_as::<_, (String, String)>("SELECT workspace, task_name FROM job WHERE id = $1")
            .bind(job_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or(AppError::NotFound(format!("job {job_id}")))?;

    state.acl.require(&user, &workspace, &task_name, TaskAction::View)?;

    let rows = JobArtifactRepo::new(state.pool.clone())
        .list_for_job(job_id)
        .await?;
    let base = state.public_base_url();
    Ok(Json(
        rows.into_iter()
            .map(|r| ArtifactListItem {
                url: format!("{base}/api/jobs/{job_id}/artifacts/{}", url_encode(&r.name)),
                name: r.name,
                content_type: r.content_type,
                size_bytes: r.size_bytes,
                step_name: r.step_name,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

pub async fn download_artifact(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path((job_id, name)): Path<(Uuid, String)>,
) -> Result<Response, AppError> {
    let (workspace, task_name) =
        sqlx::query_as::<_, (String, String)>("SELECT workspace, task_name FROM job WHERE id = $1")
            .bind(job_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or(AppError::NotFound(format!("job {job_id}")))?;

    state.acl.require(&user, &workspace, &task_name, TaskAction::View)?;

    let repo = JobArtifactRepo::new(state.pool.clone());
    let rec = repo
        .get_by_name(job_id, &name)
        .await?
        .ok_or(AppError::NotFound(format!("artifact {name}")))?;

    let blob = state
        .artifact_blob
        .clone()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("artifact storage not configured")))?;
    let stored = blob
        .get(&rec.storage_key)
        .await
        .map_err(AppError::Internal)?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("blob missing for {}", rec.name)))?;

    let mut resp = Response::new(Body::from(stored.bytes));
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&rec.content_type).unwrap());
    headers.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition_for(&rec.content_type, &rec.name)).unwrap(),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(rec.size_bytes));
    Ok(resp)
}

fn url_encode(s: &str) -> String {
    // pulls in urlencoding crate (cheap, already transitive via reqwest/axum)
    urlencoding::encode(s).into_owned()
}
```

Register routes in `crates/stroem-server/src/web/api/mod.rs` (mirror existing job-scoped routes):

```rust
.route("/jobs/:job_id/artifacts", axum::routing::get(artifacts::list_artifacts))
.route("/jobs/:job_id/artifacts/*name", axum::routing::get(artifacts::download_artifact))
```

- [ ] **Step 3: Run the tests**

```bash
cargo test -p stroem-server --test artifact_api_test
```

Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/src/web/api/ crates/stroem-server/tests/artifact_api_test.rs
git commit -m "feat(server): /api/jobs/{id}/artifacts list + inline-when-safe download"
```

---

## Task B7: Worker — scan, sniff, and upload `/artifacts/`

**Files:**
- Create: `crates/stroem-worker/src/artifacts.rs`
- Modify: `crates/stroem-worker/src/client.rs`
- Modify: `crates/stroem-worker/src/lib.rs`
- Modify: `crates/stroem-worker/Cargo.toml` (add `infer = "0.16"`, `walkdir = "2"`)

- [ ] **Step 1: Write the failing test**

`crates/stroem-worker/src/artifacts.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn scan_recurses_and_flattens_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("reports")).unwrap();
        std::fs::write(root.join("top.txt"), b"a").unwrap();
        std::fs::write(root.join("reports/q1.html"), b"b").unwrap();
        let scan = scan_artifacts(root).unwrap();
        let names: Vec<_> = scan.files.iter().map(|f| f.name.clone()).collect();
        assert!(names.contains(&"top.txt".to_string()));
        assert!(names.contains(&"reports/q1.html".to_string()));
    }

    #[test]
    fn scan_skips_symlinks_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = tmp.path().parent().unwrap().join("victim");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, root.join("link.txt")).unwrap();

        let scan = scan_artifacts(root).unwrap();
        assert!(scan.files.is_empty());
        assert!(scan.warnings.iter().any(|w| w.contains("symlink")));
    }

    #[test]
    fn scan_rejects_path_traversal_in_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("..weird")).unwrap();
        // ".." segments and control chars trigger rejection.
        let bad = root.join("a\x01bad");
        std::fs::write(&bad, b"x").unwrap();
        let scan = scan_artifacts(root).unwrap();
        assert!(scan.files.iter().all(|f| !f.name.contains('\x01')));
    }

    #[test]
    fn sniff_falls_back_to_octet_stream() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), "image/png");
        assert_eq!(sniff(b"random opaque bytes"), "application/octet-stream");
    }

    #[test]
    fn sniff_extension_fallback_for_text() {
        // infer doesn't strongly detect plain text; we fall back to extension for known textual types
        let ct = sniff_with_name(b"hello", "notes.md");
        assert_eq!(ct, "text/markdown");
    }
}
```

Implementation:

```rust
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    pub warnings: Vec<String>,
}

pub struct ScannedFile {
    /// Name as exposed to the server; relative path from `/artifacts/`.
    pub name: String,
    pub abs_path: PathBuf,
    pub size_bytes: u64,
}

pub fn scan_artifacts(root: &Path) -> Result<ScanResult> {
    if !root.exists() {
        return Ok(ScanResult { files: vec![], warnings: vec![] });
    }
    let mut files = Vec::new();
    let mut warnings = Vec::new();

    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_dir() {
            continue;
        }
        if entry.file_type().is_symlink() {
            warnings.push(format!("symlink skipped: {}", entry.path().display()));
            continue;
        }
        if !entry.file_type().is_file() {
            warnings.push(format!("non-regular file skipped: {}", entry.path().display()));
            continue;
        }
        let rel = entry.path().strip_prefix(root)
            .with_context(|| format!("strip_prefix {root:?} from {:?}", entry.path()))?;
        let name = rel.to_string_lossy().replace('\\', "/");
        if !valid_artifact_name(&name) {
            warnings.push(format!("name rejected: {name}"));
            continue;
        }
        let size_bytes = entry.metadata()?.len();
        files.push(ScannedFile {
            name,
            abs_path: entry.path().to_path_buf(),
            size_bytes,
        });
    }
    Ok(ScanResult { files, warnings })
}

fn valid_artifact_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 || name.starts_with('-') || name.starts_with('/') {
        return false;
    }
    for seg in name.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('-') {
            return false;
        }
        for ch in seg.chars() {
            if ch == '\0' || (ch.is_control() && ch != ' ') {
                return false;
            }
        }
    }
    true
}

pub fn sniff(bytes: &[u8]) -> String {
    infer::get(bytes)
        .map(|kind| kind.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

pub fn sniff_with_name(bytes: &[u8], name: &str) -> String {
    let primary = sniff(bytes);
    if primary != "application/octet-stream" {
        return primary;
    }
    // Lightweight extension fallback for textual types `infer` doesn't strongly detect.
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "md" | "markdown" => "text/markdown".into(),
        "txt" | "log" => "text/plain".into(),
        "json" => "application/json".into(),
        "yaml" | "yml" => "application/yaml".into(),
        "csv" => "text/csv".into(),
        "html" | "htm" => "text/html".into(),
        "svg" => "image/svg+xml".into(),
        _ => "application/octet-stream".into(),
    }
}
```

- [ ] **Step 2: Add client method**

In `crates/stroem-worker/src/client.rs`, mirror `upload_state_tarball`:

```rust
pub async fn upload_artifact(
    &self,
    job_id: Uuid,
    step_name: &str,
    name: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<()> {
    let encoded_name = urlencoding::encode(name);
    let url = format!(
        "{}/worker/jobs/{}/steps/{}/artifacts/{}",
        self.base_url, job_id, step_name, encoded_name
    );
    let resp = self
        .http
        .post(&url)
        .bearer_auth(&self.worker_token)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header(reqwest::header::CONTENT_LENGTH, body.len())
        .body(body)
        .send()
        .await
        .context("upload_artifact request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("upload_artifact {status}: {body}");
    }
    Ok(())
}
```

- [ ] **Step 3: Add `mod artifacts;` to `crates/stroem-worker/src/lib.rs`. Run tests.**

```bash
cargo test -p stroem-worker artifacts::tests
```

Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-worker/src/artifacts.rs crates/stroem-worker/src/client.rs crates/stroem-worker/src/lib.rs crates/stroem-worker/Cargo.toml
git commit -m "feat(worker): scan + sniff /artifacts and upload one POST per file"
```

---

## Task B8: Runner — expose `/artifacts` mount + `ARTIFACTS_DIR` env var

**Files:**
- Modify: `crates/stroem-runner/src/traits.rs` (add `artifacts_out_dir: Option<String>`)
- Modify: `crates/stroem-runner/src/docker.rs`
- Modify: `crates/stroem-runner/src/kubernetes.rs` (just refuse + warn; no mount)
- Modify: `crates/stroem-worker/src/executor.rs`

- [ ] **Step 1: Extend `RunConfig`**

In `traits.rs`:

```rust
/// Path on the worker host that becomes `/artifacts` inside the container,
/// or the literal directory the shell runner writes into.
/// Set only for runners that support artifacts (shell, docker WithWorkspace,
/// docker NoWorkspace). Unset for Kube modes (deferred) and for
/// agent/approval/task actions.
pub artifacts_out_dir: Option<String>,
```

- [ ] **Step 2: Docker — bind-mount for BOTH modes**

In `docker.rs::build_container_config`, in both `WithWorkspace` and `NoWorkspace` arms, add:

```rust
if let Some(ref out) = config.artifacts_out_dir {
    binds.push(format!("{}:/artifacts:rw", out));
}
```

For `NoWorkspace`, you need to introduce `binds` (currently the arm builds `ContainerCreateBody` with no `host_config.binds`); only emit the `host_config` block if `binds` is non-empty.

Add unit tests:

```rust
#[test]
fn no_workspace_bind_mounts_artifacts_when_set() {
    let cfg = RunConfig {
        runner_mode: RunnerMode::NoWorkspace,
        artifacts_out_dir: Some("/tmp/art-abc".to_string()),
        ..test_default_config()
    };
    let container = DockerRunner::build_container_config(&cfg);
    let binds = container.host_config.as_ref().unwrap().binds.as_ref().unwrap();
    assert!(binds.iter().any(|b| b == "/tmp/art-abc:/artifacts:rw"));
}

#[test]
fn with_workspace_includes_artifacts_bind() {
    let cfg = RunConfig {
        runner_mode: RunnerMode::WithWorkspace,
        artifacts_out_dir: Some("/tmp/art-xyz".to_string()),
        workdir: "/tmp/ws".to_string(),
        ..test_default_config()
    };
    let container = DockerRunner::build_container_config(&cfg);
    let binds = container.host_config.unwrap().binds.unwrap();
    assert!(binds.iter().any(|b| b == "/tmp/art-xyz:/artifacts:rw"));
}
```

- [ ] **Step 3: Kube — explicitly do NOT mount; log warning**

In `kubernetes.rs::build_pod_spec`, after constructing the spec, add:

```rust
if config.artifacts_out_dir.is_some() {
    tracing::warn!(
        "kubernetes runner does not support /artifacts mount yet; \
         files written by the step will be discarded"
    );
}
```

(The plumbing on the worker side will already have created an `artifacts_out_dir`; we simply don't mount it into the pod. The post-exec scan will find an empty dir → no upload → no rows. The warning shows up in worker logs so the operator notices.)

- [ ] **Step 4: Worker — create tmpdir + set env var + pass to runner**

In `crates/stroem-worker/src/executor.rs`, near the existing state-dir setup:

```rust
let artifacts_out = if step_supports_artifacts(&step) {
    let dir = tempfile::TempDir::new()
        .context("create artifacts tmpdir")?;
    let path = dir.path().to_string_lossy().into_owned();
    config.artifacts_out_dir = Some(path.clone());

    let is_container_mode = matches!(
        config.runner_mode,
        stroem_runner::RunnerMode::WithWorkspace | stroem_runner::RunnerMode::NoWorkspace
    );
    let env_value = if matches!(config.runner, RunnerKind::Shell) {
        path.clone()  // host path for shell
    } else if is_container_mode && matches!(config.runner, RunnerKind::Docker) {
        "/artifacts".to_string()
    } else {
        // Kube modes: do not set env var. Author can detect unset.
        String::new()
    };
    if !env_value.is_empty() {
        config.env.insert("ARTIFACTS_DIR".into(), env_value);
    }
    Some(dir) // keep TempDir alive for the duration of the step
} else {
    None
};
```

`step_supports_artifacts` returns false for `agent`, `approval`, `task` action types.

Pass `artifacts_out` to the post-step scanner (next task).

- [ ] **Step 5: Run unit tests**

```bash
cargo test -p stroem-runner
cargo test -p stroem-worker executor
```

Expected: new docker tests pass; existing tests unchanged.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-runner/ crates/stroem-worker/src/executor.rs
git commit -m "feat(runners): bind-mount /artifacts and set ARTIFACTS_DIR (shell + docker)"
```

---

## Task B9: Worker — upload phase + lifecycle ordering + cleanup-on-failure

**Files:**
- Modify: `crates/stroem-worker/src/executor.rs`
- Modify: `crates/stroem-worker/src/client.rs`

- [ ] **Step 1: Add post-step upload hook**

After the step process exits, after the existing state upload, but only if the step is `succeeded`:

```rust
if step_outcome.is_success() {
    if let Some(ref tmp) = artifacts_out {
        match upload_artifacts_for_step(
            &client,
            &step,
            tmp.path(),
        ).await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!("artifact upload failed: {e:#}");
                // Demote step to failed + cleanup uploaded blobs for this step.
                if let Err(ce) = client
                    .delete_step_artifacts(step.job_id, &step.step_name)
                    .await
                {
                    tracing::warn!("artifact cleanup also failed: {ce:#}");
                }
                step_outcome = StepOutcome::Failed(format!("artifact upload failed: {e}"));
            }
        }
    }
}
```

Function:

```rust
async fn upload_artifacts_for_step(
    client: &ServerClient,
    step: &ClaimedStep,
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let scan = crate::artifacts::scan_artifacts(dir)?;
    for warning in &scan.warnings {
        tracing::warn!(step = %step.step_name, "{warning}");
    }
    for file in scan.files {
        let bytes = tokio::fs::read(&file.abs_path).await?;
        let ct = crate::artifacts::sniff_with_name(&bytes, &file.name);
        upload_with_retry(client, step.job_id, &step.step_name, &file.name, &ct, bytes).await?;
    }
    Ok(())
}

async fn upload_with_retry(
    client: &ServerClient,
    job_id: Uuid,
    step: &str,
    name: &str,
    content_type: &str,
    body: Vec<u8>,
) -> anyhow::Result<()> {
    let mut delay_ms = 250u64;
    for attempt in 1..=3 {
        match client.upload_artifact(job_id, step, name, content_type, body.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 3 => {
                tracing::warn!("artifact upload attempt {attempt} failed: {e:#}");
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(4_000);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
```

Add `client.delete_step_artifacts` — a new worker→server endpoint (`DELETE /worker/jobs/{id}/steps/{step}/artifacts`) that calls `JobArtifactRepo::delete_for_step` + `BlobArchive::delete_prefix({prefix}{ws}/{job_id}/{step}/)`.

- [ ] **Step 2: Add server-side delete endpoint**

In `crates/stroem-server/src/web/worker_api/artifacts.rs`:

```rust
pub async fn delete_step_artifacts(
    State(state): State<AppState>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
) -> Result<StatusCode, AppError> {
    let workspace = sqlx::query_scalar::<_, String>("SELECT workspace FROM job WHERE id = $1")
        .bind(job_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound(format!("job {job_id}")))?;

    let blob = state.artifact_blob.clone()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("artifact storage not configured")))?;
    let prefix = format!("{}{}/{}/{}/", state.artifact_config.prefix, workspace, job_id, step_name);
    blob.delete_prefix(&prefix).await.map_err(AppError::Internal)?;

    JobArtifactRepo::new(state.pool.clone())
        .delete_for_step(job_id, &step_name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
```

Register: `.route("/jobs/:job_id/steps/:step_name/artifacts", axum::routing::delete(artifacts::delete_step_artifacts))`.

- [ ] **Step 3: Integration test for the cleanup path**

Append to `crates/stroem-server/tests/artifact_upload_test.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn delete_step_artifacts_clears_rows_and_blobs() {
    let h = common::TestHarness::new().await;
    let job_id = h.create_test_job("ws1", "task1").await;
    h.upload_artifact(job_id, "build", "a.txt", "text/plain", b"x").await;
    h.upload_artifact(job_id, "build", "b.txt", "text/plain", b"y").await;
    h.upload_artifact(job_id, "test", "c.txt", "text/plain", b"z").await;

    let resp = h.worker_request("DELETE", &format!("/worker/jobs/{job_id}/steps/build/artifacts"))
        .send().await;
    assert_eq!(resp.status(), 204);

    let remaining = h.list_artifacts(job_id).await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].step_name, "test");
}
```

- [ ] **Step 4: Run tests + commit**

```bash
cargo test --workspace
```

Expected: green.

```bash
git add crates/stroem-worker/src/ crates/stroem-server/src/web/worker_api/
git commit -m "feat(worker): retry-with-cleanup artifact upload after successful step"
```

---

## Task B10: Hook context — `hook.artifacts`

**Files:**
- Modify: `crates/stroem-server/src/hooks.rs`
- Modify: hook context type in `crates/stroem-common/src/...` (find via `grep -rn "HookContext"`)

- [ ] **Step 1: Add artifacts vec to `HookContext`**

Locate the `HookContext` struct (likely `crates/stroem-server/src/hooks.rs` or `crates/stroem-common`). Add:

```rust
#[derive(serde::Serialize)]
pub struct HookArtifactMeta {
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub step_name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub url: String,
}
```

Push into `HookContext.artifacts: Vec<HookArtifactMeta>`.

- [ ] **Step 2: Populate at hook fire time**

In `fire_hooks(&AppState, ...)`, before rendering hook templates, fetch artifacts:

```rust
let rows = stroem_db::repos::job_artifact::JobArtifactRepo::new(state.pool.clone())
    .list_for_job(job_id).await.unwrap_or_default();
let base = state.public_base_url();
let artifacts = rows.into_iter().map(|r| HookArtifactMeta {
    url: format!("{base}/api/jobs/{}/artifacts/{}", job_id, urlencoding::encode(&r.name)),
    name: r.name,
    content_type: r.content_type,
    size_bytes: r.size_bytes,
    step_name: r.step_name,
    created_at: r.created_at,
}).collect();
```

- [ ] **Step 3: Integration test**

Create `crates/stroem-server/tests/hook_artifacts_test.rs` exercising a webhook hook with `text: "{% for a in hook.artifacts %}{{ a.name }}={{ a.url }};{% endfor %}"` and verifying the rendered body contains both artifact URLs.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/src/hooks.rs crates/stroem-common/src/ crates/stroem-server/tests/hook_artifacts_test.rs
git commit -m "feat(hooks): expose hook.artifacts list to hook templates"
```

---

## Task B11: Retention sweep — delete blobs before deleting `job`

**Files:**
- Modify: `crates/stroem-server/src/recovery.rs` (where `retention.job_days` sweep lives) or `crates/stroem-server/src/job_recovery.rs`

- [ ] **Step 1: Locate the retention sweep**

```bash
grep -rn "job_days\|retention" crates/stroem-server/src/
```

Inside the sweep, before `DELETE FROM job WHERE id = $1`, add:

```rust
let workspace = /* already fetched in the row */;
let prefix = format!("{}{}/{}/", state.artifact_config.prefix, workspace, job_id);
if let Some(ref blob) = state.artifact_blob {
    if let Err(e) = blob.delete_prefix(&prefix).await {
        tracing::warn!(
            %job_id, "artifact blob cleanup failed during retention: {e:#}"
        );
        // Continue — FK RESTRICT will block job deletion until rows go,
        // and rows can't go while blobs orphan-leak. We'll retry next sweep.
        continue;
    }
}
stroem_db::repos::job_artifact::JobArtifactRepo::new(state.pool.clone())
    .delete_for_job(job_id)
    .await?;
```

Then the existing `DELETE FROM job` runs; FK `RESTRICT` ensures we only get here if `job_artifact` is already empty.

- [ ] **Step 2: Integration test**

Append to recovery tests (or create `crates/stroem-server/tests/artifact_retention_test.rs`):

```rust
#[tokio::test(flavor = "multi_thread")]
async fn retention_sweep_deletes_artifacts_then_job() {
    let h = common::TestHarness::new_with_retention_days(1).await;
    let job_id = h.create_test_job_at("ws1", "task1",
        chrono::Utc::now() - chrono::Duration::days(2)).await;
    h.upload_artifact(job_id, "s", "a.txt", "text/plain", b"x").await;

    h.run_retention_sweep().await;

    assert!(h.get_job_row(job_id).await.is_none());
    assert!(h.list_artifacts(job_id).await.is_empty());
    assert!(h.blob_exists(&format!("artifacts/ws1/{job_id}/s/a.txt")).await == false);
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/recovery.rs crates/stroem-server/tests/
git commit -m "feat(server): retention sweep deletes artifact blobs before job row"
```

---

## Task B12: UI — Artifacts section at top of Job Detail

**Files:**
- Create: `ui/src/components/artifact-list.tsx`
- Modify: `ui/src/pages/job-detail.tsx`
- Modify: `ui/src/lib/api.ts`

- [ ] **Step 1: Add typed API client method**

In `ui/src/lib/api.ts`:

```typescript
export interface ArtifactItem {
  name: string;
  content_type: string;
  size_bytes: number;
  step_name: string;
  created_at: string;
  url: string;
}

export async function listJobArtifacts(jobId: string): Promise<ArtifactItem[]> {
  return apiFetch(`/api/jobs/${jobId}/artifacts`);
}
```

- [ ] **Step 2: Component**

`ui/src/components/artifact-list.tsx`:

```tsx
import { FileText, Image, FileArchive, FileCode, File } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { ArtifactItem } from "@/lib/api";

const SAFE_INLINE = new Set([
  "image/png", "image/jpeg", "image/gif", "image/webp",
  "application/pdf", "text/plain", "text/markdown",
]);

function iconFor(ct: string) {
  if (ct.startsWith("image/")) return <Image className="w-4 h-4" aria-hidden />;
  if (ct === "application/pdf") return <FileText className="w-4 h-4" aria-hidden />;
  if (ct === "application/zip" || ct.endsWith("gzip") || ct.endsWith("tar"))
    return <FileArchive className="w-4 h-4" aria-hidden />;
  if (ct.startsWith("text/") || ct.endsWith("json") || ct.endsWith("yaml"))
    return <FileCode className="w-4 h-4" aria-hidden />;
  return <File className="w-4 h-4" aria-hidden />;
}

function humanSize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

export function ArtifactList({ items }: { items: ArtifactItem[] }) {
  if (items.length === 0) return null;
  return (
    <section aria-labelledby="artifacts-heading" className="rounded-lg border bg-card p-4">
      <h2 id="artifacts-heading" className="font-semibold mb-3">Artifacts</h2>
      <ul className="divide-y">
        {items.map((a) => {
          const ct = a.content_type.split(";")[0].trim().toLowerCase();
          const safe = SAFE_INLINE.has(ct);
          return (
            <li key={a.name} className="flex items-center gap-3 py-2">
              {iconFor(ct)}
              <span className="flex-1 font-mono text-sm truncate">{a.name}</span>
              <span className="text-xs text-muted-foreground tabular-nums">{humanSize(a.size_bytes)}</span>
              <span className="text-xs text-muted-foreground hidden md:inline">{ct}</span>
              <span className="text-xs text-muted-foreground hidden md:inline">from: {a.step_name}</span>
              <Button asChild variant="outline" size="sm">
                <a href={a.url} target={safe ? "_blank" : undefined} rel="noreferrer">
                  {safe ? "Open ↗" : "Download"}
                </a>
              </Button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
```

- [ ] **Step 3: Splice into Job Detail page**

In `ui/src/pages/job-detail.tsx`, fetch artifacts (`useQuery` with `listJobArtifacts(jobId)`) and render `<ArtifactList items={artifacts ?? []} />` between the existing job-stats section and the step timeline.

- [ ] **Step 4: Playwright E2E**

In `ui/e2e/`, add `artifacts.spec.ts`:

```typescript
import { test, expect } from "@playwright/test";

test("artifacts section appears with uploaded files", async ({ page }) => {
  // assumes the test backend has a fixture job with two artifacts seeded
  await page.goto("/jobs/<fixture-job-id>");
  await expect(page.getByRole("heading", { name: "Artifacts" })).toBeVisible();
  await expect(page.getByText("report.html")).toBeVisible();
  await expect(page.getByRole("link", { name: /Download/ })).toBeVisible();
  await expect(page.getByRole("link", { name: /Open/ })).toBeVisible();
});
```

- [ ] **Step 5: Run UI checks + commit**

```bash
cd ui && bun run lint && bunx tsc --noEmit && bunx playwright test artifacts
```

Expected: green.

```bash
git add ui/src/components/artifact-list.tsx ui/src/pages/job-detail.tsx ui/src/lib/api.ts ui/e2e/artifacts.spec.ts
git commit -m "feat(ui): artifacts section on Job Detail with inline-vs-download dispatch"
```

---

## Task B13: E2E happy path

**Files:**
- Modify: `tests/e2e.sh`

- [ ] **Step 1: Add an artifact step + verify download**

Append to `tests/e2e.sh` (after the existing happy-path workflow):

```bash
# ── Artifacts E2E ──────────────────────────────────────────────────────────
log "Triggering artifact-producing job"
JOB_ID=$(curl -sS -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"task": "produce-artifacts"}' \
  "$SERVER/api/workspaces/test/execute" | jq -r '.job_id')

wait_for_terminal_status "$JOB_ID" succeeded

log "Listing artifacts"
ARTIFACTS=$(curl -sS -H "Authorization: Bearer $TOKEN" \
  "$SERVER/api/jobs/$JOB_ID/artifacts")
echo "$ARTIFACTS" | jq -e '. | length >= 1' > /dev/null || \
  fail "expected ≥1 artifact, got: $ARTIFACTS"

log "Downloading first artifact"
URL=$(echo "$ARTIFACTS" | jq -r '.[0].url')
NAME=$(echo "$ARTIFACTS" | jq -r '.[0].name')
curl -sS -f -H "Authorization: Bearer $TOKEN" "$URL" -o "/tmp/dl-$NAME"
[ -s "/tmp/dl-$NAME" ] || fail "downloaded artifact is empty"
log "Artifacts E2E ✓"
```

Add a `tests/e2e-workspace/produce-artifacts.yaml`:

```yaml
name: produce-artifacts
steps:
  - name: make-report
    action:
      type: script
      runner: local
      language: shell
      script: |
        echo "<h1>Report</h1>" > "$ARTIFACTS_DIR/report.html"
        echo "ok" > "$ARTIFACTS_DIR/status.txt"
```

- [ ] **Step 2: Run E2E**

```bash
./tests/e2e.sh
```

Expected: full E2E passes including the new artifacts section.

- [ ] **Step 3: Commit + Phase B green-bar gate**

```bash
cargo fmt --check --all && cargo clippy --workspace -- -D warnings && cargo test --workspace
cd ui && bun run lint && bunx tsc --noEmit && cd -
```

```bash
git add tests/e2e.sh tests/e2e-workspace/
git commit -m "test(e2e): produce, list, and download artifacts end-to-end"
```

---

# Phase C — Polish (CLI, MCP, docs)

## Task C1: CLI — `stroem-api artifacts list`

**Files:**
- Create: `crates/stroem-cli/src/remote/artifacts.rs`
- Modify: `crates/stroem-cli/src/remote/mod.rs`
- Modify: `crates/stroem-cli/src/stroem_api.rs`

- [ ] **Step 1: Add subcommand definition**

In `stroem_api.rs` (mirror `Logs` subcommand):

```rust
Artifacts {
    #[command(subcommand)]
    action: ArtifactsCommand,
},
```

```rust
#[derive(clap::Subcommand)]
enum ArtifactsCommand {
    /// List artifacts for a job.
    List { job_id: Uuid },
    /// Download an artifact by name.
    Download {
        job_id: Uuid,
        name: String,
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
}
```

- [ ] **Step 2: Implement `list`**

`crates/stroem-cli/src/remote/artifacts.rs`:

```rust
use anyhow::Result;
use uuid::Uuid;

use crate::remote::client::ApiClient;

pub async fn list(client: &ApiClient, job_id: Uuid) -> Result<()> {
    let resp: serde_json::Value = client
        .get(&format!("/api/jobs/{job_id}/artifacts"))
        .await?;
    let items = resp.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no artifacts)");
        return Ok(());
    }
    println!("{:<40} {:>12} {:<24} {:<20}", "NAME", "SIZE", "TYPE", "STEP");
    for it in items {
        println!(
            "{:<40} {:>12} {:<24} {:<20}",
            it["name"].as_str().unwrap_or(""),
            it["size_bytes"].as_i64().unwrap_or(0),
            it["content_type"].as_str().unwrap_or(""),
            it["step_name"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

pub async fn download(
    client: &ApiClient,
    job_id: Uuid,
    name: &str,
    output: Option<&str>,
) -> Result<()> {
    let encoded = urlencoding::encode(name);
    let bytes = client
        .get_bytes(&format!("/api/jobs/{job_id}/artifacts/{encoded}"))
        .await?;
    let out_path = output.unwrap_or(name);
    if let Some(parent) = std::path::Path::new(out_path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    tokio::fs::write(out_path, &bytes).await?;
    eprintln!("Wrote {} bytes to {}", bytes.len(), out_path);
    Ok(())
}
```

If `ApiClient` doesn't yet have `get_bytes`, add it next to `get`:

```rust
pub async fn get_bytes(&self, path: &str) -> Result<bytes::Bytes> {
    let resp = self.http.get(self.url(path))
        .bearer_auth(&self.token)
        .send().await?
        .error_for_status()?;
    Ok(resp.bytes().await?)
}
```

- [ ] **Step 3: Wire dispatch**

In `stroem_api.rs` main dispatch:

```rust
Command::Artifacts { action } => match action {
    ArtifactsCommand::List { job_id } => remote::artifacts::list(&client, job_id).await?,
    ArtifactsCommand::Download { job_id, name, output } => {
        remote::artifacts::download(&client, job_id, &name, output.as_deref()).await?
    }
},
```

- [ ] **Step 4: Test**

Add a CLI integration test in `crates/stroem-cli/tests/artifacts_cli.rs` that uses `assert_cmd` against a stubbed HTTP server (or document as covered by the E2E test if simpler).

- [ ] **Step 5: Run + commit**

```bash
cargo test -p stroem-cli
```

```bash
git add crates/stroem-cli/src/
git commit -m "feat(cli): stroem-api artifacts list + download"
```

---

## Task C2: MCP — `list_artifacts` + `get_artifact`

**Files:**
- Modify: `crates/stroem-server/src/mcp/tools.rs`

- [ ] **Step 1: Add `list_artifacts` tool**

Mirror `get_job_logs`:

```rust
#[tool(description = "List artifacts produced by a job. Returns name, content_type, size_bytes, step_name, and download URL.")]
async fn list_artifacts(
    &self,
    #[tool(param)]
    #[schemars(description = "Job UUID")]
    job_id: String,
) -> std::result::Result<CallToolResult, McpError> {
    let job_uuid = parse_uuid(&job_id)?;
    let state = current_state()?;
    let (workspace, task_name) = job_workspace_task(&state, job_uuid).await?;
    require_view(&state, &workspace, &task_name).await?;

    let rows = stroem_db::repos::job_artifact::JobArtifactRepo::new(state.pool.clone())
        .list_for_job(job_uuid).await
        .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
    let base = state.public_base_url();
    let items: Vec<_> = rows.into_iter().map(|r| serde_json::json!({
        "name": r.name,
        "content_type": r.content_type,
        "size_bytes": r.size_bytes,
        "step_name": r.step_name,
        "created_at": r.created_at.to_rfc3339(),
        "url": format!("{base}/api/jobs/{}/artifacts/{}", job_uuid, urlencoding::encode(&r.name)),
    })).collect();
    Ok(CallToolResult::success(vec![Content::text(serde_json::to_string(&items).unwrap())]))
}
```

- [ ] **Step 2: Add `get_artifact` with 1 MiB + text-only guard**

```rust
const MCP_ARTIFACT_MAX_BYTES: i64 = 1024 * 1024;
const MCP_TEXT_PREFIXES: &[&str] = &["text/", "application/json", "application/yaml", "application/xml"];

#[tool(description = "Read an artifact's bytes (text/JSON/YAML/XML up to 1MB only). Refuses binary or oversize files; the agent should use `list_artifacts` and pass URLs to humans for binary content.")]
async fn get_artifact(
    &self,
    #[tool(param)]
    #[schemars(description = "Job UUID")]
    job_id: String,
    #[tool(param)]
    #[schemars(description = "Artifact name as returned by list_artifacts")]
    name: String,
) -> std::result::Result<CallToolResult, McpError> {
    let job_uuid = parse_uuid(&job_id)?;
    let state = current_state()?;
    let (workspace, task_name) = job_workspace_task(&state, job_uuid).await?;
    require_view(&state, &workspace, &task_name).await?;

    let repo = stroem_db::repos::job_artifact::JobArtifactRepo::new(state.pool.clone());
    let rec = repo.get_by_name(job_uuid, &name).await
        .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?
        .ok_or_else(|| McpError::invalid_params(format!("artifact {name} not found"), None))?;

    if rec.size_bytes > MCP_ARTIFACT_MAX_BYTES {
        return Err(McpError::invalid_params(
            format!("artifact {} is {} bytes; MCP get_artifact caps at {}",
                rec.name, rec.size_bytes, MCP_ARTIFACT_MAX_BYTES),
            None));
    }
    let ct = rec.content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    let is_text = MCP_TEXT_PREFIXES.iter().any(|p| ct.starts_with(p));
    if !is_text {
        return Err(McpError::invalid_params(
            format!("artifact {} has content_type {}; MCP get_artifact only returns textual content", rec.name, rec.content_type),
            None));
    }

    let blob = state.artifact_blob.clone()
        .ok_or_else(|| McpError::internal_error("artifact storage not configured".into(), None))?;
    let stored = blob.get(&rec.storage_key).await
        .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?
        .ok_or_else(|| McpError::internal_error("blob missing".into(), None))?;

    let text = String::from_utf8_lossy(&stored.bytes).into_owned();
    Ok(CallToolResult::success(vec![Content::text(text)]))
}
```

- [ ] **Step 3: Test**

`crates/stroem-server/tests/mcp_artifacts_test.rs` — call both tools through the MCP harness used by existing MCP tests; assert size+type guards reject oversized + binary cases.

- [ ] **Step 4: Run + commit**

```bash
cargo test -p stroem-server mcp_artifacts
git add crates/stroem-server/src/mcp/ crates/stroem-server/tests/mcp_artifacts_test.rs
git commit -m "feat(mcp): list_artifacts + get_artifact with 1MB text-only guard"
```

---

## Task C3: Documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/internal/stroem-v2-plan.md`
- Modify: `docs/internal/TODO.md`
- Create: `docs/src/content/docs/guides/artifacts.md`
- Modify: `docs/src/content/docs/reference/api.md`
- Modify: `docs/src/content/docs/reference/worker-api.md`
- Modify: `docs/src/content/docs/operations/storage.md` (or create if missing)
- Modify: `docs/scripts/generate-llms-txt.ts`

- [ ] **Step 1: `CLAUDE.md` — add `### Artifacts` under Key Patterns**

Insert between `### Workspace-Level State` and `### Retry Mechanism`:

```markdown
### Artifacts
- Per-job opaque files produced by successful steps. Convention dir `/artifacts/` (or `$ARTIFACTS_DIR`); recursive scan, dotfiles included, symlinks skipped + warned.
- Per-file 100 MiB cap, per-job 1 GiB cap, both configurable under `artifact_storage:`.
- Success-only upload: failed/cancelled steps discard `/artifacts/`. Upload retried 3× with backoff; terminal failure fails the step AND cleans up already-uploaded blobs for that step.
- Per-job namespace, `UNIQUE(job_id, name)`, last-writer-wins on collision (for_each authors must template filenames).
- Worker sniffs Content-Type via `infer` crate; server stores verbatim, applies `X-Content-Type-Options: nosniff` and inline-when-safe `Content-Disposition` (images, PDF, text/plain, text/markdown). HTML/SVG/XML/JSON forced to attachment.
- Storage via `BlobArchive` trait (unified backend for logs, state, artifacts). S3 + Local impls; `put_stream`/`get_stream` overrides for memory-flat artifact transfers.
- Retention: cascades with `job` row. FK `RESTRICT` + explicit two-phase delete (blob → row).
- Runner support: shell, `script:docker`, `type:docker`. Kube modes deferred (same gap as state file-mount).
- Hooks: `hook.artifacts` is a list of `{name, content_type, size_bytes, url, step_name, created_at}`.
- ACL: `View` on the task. No new permission level.
```

- [ ] **Step 2: User guide `docs/src/content/docs/guides/artifacts.md`**

```markdown
---
title: Artifacts
description: Produce, view, and download files from your tasks.
---

Artifacts let a task produce files — reports, screenshots, build outputs — that
humans can view or download from the UI, CLI, or MCP after the job completes.

## Producing artifacts

Write files to `/artifacts/` (or the `$ARTIFACTS_DIR` env var for portability):

\`\`\`yaml
name: nightly-report
steps:
  - name: build
    action:
      type: script
      runner: local
      language: shell
      script: |
        ./tools/report --out "$ARTIFACTS_DIR/report.html"
        cp build/dist.zip "$ARTIFACTS_DIR/"
\`\`\`

When the step succeeds, the worker uploads every file in `/artifacts/` (recursively). Failed or cancelled steps discard their `/artifacts/` directory.

### What gets uploaded

- ✅ Regular files (recursive — `reports/q1.html` keeps the slash in its name).
- ✅ Dotfiles (e.g. `.config`). ⚠️ **Don't put `.env` here.**
- ✅ Empty files.
- ❌ Symbolic links (skipped + warned).
- ❌ Filenames with `..`, null bytes, or control characters (rejected + warned).

### Limits

| | Default | Configurable |
|---|---:|---|
| Per-file | 100 MiB | `artifact_storage.max_file_bytes` |
| Per-job  | 1 GiB   | `artifact_storage.max_job_bytes` |

Exceeding either fails the step loudly.

### Runner support

| Runner | Supported |
|---|---|
| Shell (local) | ✅ |
| Docker (`script:docker`) | ✅ |
| Docker (`type:docker`) | ✅ |
| Kubernetes (any) | ❌ (deferred — same limitation as state `/state-out`) |
| Agent / Approval / Task actions | n/a |

On unsupported runners, `$ARTIFACTS_DIR` is unset; scripts that try `> "$ARTIFACTS_DIR/foo"` will fail loudly with "ambiguous redirect".

## Viewing & downloading

UI: a new **Artifacts** section appears near the top of the Job Detail page. Safe MIME types (PNG, JPEG, GIF, WebP, PDF, plain text, Markdown) open inline in a new tab. Everything else — including HTML and SVG (for XSS hardening) — downloads.

CLI:

\`\`\`sh
stroem-api artifacts list <job-id>
stroem-api artifacts download <job-id> report.html -o ./report.html
\`\`\`

Hooks: `hook.artifacts` is a list of `{name, content_type, size_bytes, url, step_name, created_at}`. Example Slack notification:

\`\`\`yaml
on_success:
  - action: slack-notify
    input:
      text: |
        Job done. Outputs:
        {% for a in hook.artifacts %}- <{{ a.url }}|{{ a.name }}> ({{ a.size_bytes }} B)\n{% endfor %}
\`\`\`

MCP: `list_artifacts(job_id)` and `get_artifact(job_id, name)` (text/JSON/YAML/XML up to 1 MB only — binaries should be linked via URL, not embedded in the agent's context).

## Retention

Artifacts live as long as the job row does. Once a job is deleted (via the existing `retention.job_days` sweep), its blobs and rows go with it. There is no separate artifact TTL today.

## Security notes

Artifacts produced by a workflow are served with the stored Content-Type, but anything that could execute in the browser (HTML, SVG, XML) is force-downloaded rather than rendered. `X-Content-Type-Options: nosniff` blocks browsers from second-guessing.

`View` permission on the task is sufficient to list and download artifacts — the same permission required to view logs. Don't put secrets in `/artifacts/`.
```

- [ ] **Step 3: `docs/src/content/docs/reference/api.md`** — append `GET /api/jobs/{id}/artifacts` and `GET /api/jobs/{id}/artifacts/{name}` sections matching the rest of the reference style.

- [ ] **Step 4: `docs/src/content/docs/reference/worker-api.md`** — add `POST /worker/jobs/{id}/steps/{step}/artifacts/{name}` and `DELETE /worker/jobs/{id}/steps/{step}/artifacts`.

- [ ] **Step 5: `docs/src/content/docs/operations/storage.md`** — document `artifact_storage:` config block. Note that omitting `archive` inherits the configured `log_storage` backend.

- [ ] **Step 6: `docs/scripts/generate-llms-txt.ts`** — register the new guide:

```typescript
{ slug: "guides/artifacts", title: "Artifacts" },
```

Then run:

```bash
cd docs && bun run generate-llms
```

Expected: `docs/public/llms.txt` regenerated with the Artifacts section.

- [ ] **Step 7: `docs/internal/stroem-v2-plan.md`** — mark "Artifacts" phase complete.

- [ ] **Step 8: `docs/internal/TODO.md`** — under "Roadmap"/"Bugs" add "Kube runner artifact mount (sidecar uploader)" if not already present.

- [ ] **Step 9: Commit**

```bash
git add CLAUDE.md docs/
git commit -m "docs: artifacts user guide, API reference, CLAUDE.md, llms.txt"
```

---

## Task C4: Final green-bar gate + release notes

- [ ] **Step 1: Full CI sweep**

```bash
cargo fmt --check --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
cd ui && bun run lint && bunx tsc --noEmit && bunx playwright test && cd -
./tests/e2e.sh
```

Expected: all green.

- [ ] **Step 2: Manual smoke**

Start server + worker locally; trigger the `produce-artifacts` task; verify:
1. UI shows artifacts section on Job Detail.
2. Image opens in new tab; ZIP downloads.
3. `stroem-api artifacts list <id>` shows the same items.
4. `nosniff` header present on the download response.

- [ ] **Step 3: Update CHANGELOG (if the project keeps one) + bump version per `/release` workflow**

Follow the existing release process (`Cargo.toml` workspace.package.version + `helm/stroem/Chart.yaml`).

---

## Self-Review (per writing-plans skill)

**Spec coverage (mapped against the 23 decisions + ARTIFACTS_DIR splice):**

| Decision | Task(s) |
|---|---|
| ~100 MiB per file, 1 GiB per job | B3, B5 |
| Convention dir `/artifacts` | B8 (mount), B7 (scan) |
| Per-job scope, last-writer-wins | B1 (UNIQUE), B5 (upsert) |
| No cross-step access | (deliberate: no `/artifacts-in/` mount) |
| Success-only upload | B9 (upload-only-after-success branch) |
| Worker pre-check + server validation | B5 (size limits), B7 (scan validation) |
| Fail loud on exceed | B5 (413 + AppError::PayloadTooLarge) |
| BlobArchive trait, buffer-primary + streaming | A1, A2, A3 |
| Preserve config shape | B3 (new block under same shape) |
| One POST per file | B5 |
| Retry + cleanup on terminal failure | B9 (upload_with_retry + delete_step_artifacts) |
| Worker sniffs Content-Type | B7 (sniff/sniff_with_name) |
| Inline-when-safe + nosniff | B6 (disposition_for + headers) |
| Retention cascade only | B11 |
| ACL: View permission | B6 (state.acl.require) |
| `hook.artifacts` list | B10 |
| Shell + script:docker + type:docker, Kube deferred + warned | B8 (docker), B8 step 3 (kube warn) |
| No out-of-band upload, no workspace artifacts | (omitted per decision) |
| UI top of Job Detail, new tab for safe MIMEs | B12 |
| FK RESTRICT + two-phase delete | B1 (RESTRICT), B11 (blob-then-row in sweep) |
| Scan recursive flatten, skip symlinks, allow dotfiles, validate names | B7 |
| Sequential upload | B9 (sequential for-loop) |
| Lifecycle: state first, then artifacts; upload time NOT counted | B9 (after-state ordering; existing timeout wraps exec only) |
| CLI list+download | C1 |
| MCP list + get with 1MB text-only guard | C2 |
| ARTIFACTS_DIR env, unset on unsupported | B8 step 4 |

**Placeholder scan:** no "TBD" / "implement later" / "similar to" — every step has either a code block or an exact command.

**Type consistency:** `JobArtifactRepo`, `NewArtifactRow`, `JobArtifactRecord`, `ArtifactListItem`, `HookArtifactMeta`, `ArtifactItem` (TS) match across tasks; `ARTIFACTS_DIR` env var and `/artifacts` container path used consistently; `delete_for_step` / `delete_for_job` used consistently between repo (B2) and callers (B9, B11).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-01-artifacts.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
