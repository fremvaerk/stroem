use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

/// A retrieved blob plus its content type.
#[derive(Debug, Clone)]
pub struct Blob {
    pub content_type: String,
    pub bytes: Bytes,
}

/// An archive object opened for ranged reads, pinned to the version seen
/// at open: a replaced object is an error, never a silent mix.
#[derive(Debug)]
pub struct ArchiveObject {
    pub key: String,
    pub size: u64,
    pub version: ArchiveVersion,
}

#[derive(Debug)]
pub enum ArchiveVersion {
    /// The whole object, held by the default `open` (test and in-memory
    /// backends only).
    Snapshot(Bytes),
    /// An open descriptor: reads see the opened file even after the path
    /// is atomically replaced.
    File(tokio::sync::Mutex<fs::File>),
    /// An S3(-compatible) object's version as seen by `HEAD`: an `ETag`
    /// sent as `If-Match` on every ranged GET when present, else the
    /// object's `Last-Modified` (already formatted as an HTTP-date) sent as
    /// `If-Unmodified-Since` -- some S3-compatible endpoints omit the
    /// `ETag` header. Both `None` means the endpoint returned neither, and
    /// ranged reads of that object are not version-pinned at all (a
    /// `warn!` at `open` names this, see `S3BlobArchive::open`).
    S3 {
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// Unified pluggable storage for logs, state snapshots, and artifacts.
///
/// Keys are opaque slash-separated paths; each consumer owns its key namespace.
/// `put`/`get` are the primary API; streaming overrides exist for callers that
/// need to avoid buffering large payloads (artifacts up to 100 MiB).
// async_trait desugars this to a function returning a `Pin<Box<dyn Future>>`,
// already `#[must_use]`, and redundantly marks the method itself `#[must_use]`
// too — newer clippy's `double_must_use` flags that redundancy.
#[allow(clippy::double_must_use)]
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

    /// Open `key` for ranged reads; `None` when it does not exist. The
    /// default holds the whole object in memory, which is acceptable only
    /// for test and in-memory backends; the local and S3 backends override.
    async fn open(&self, key: &str) -> Result<Option<ArchiveObject>> {
        Ok(self.get(key).await?.map(|blob| ArchiveObject {
            key: key.to_string(),
            size: blob.bytes.len() as u64,
            version: ArchiveVersion::Snapshot(blob.bytes),
        }))
    }

    /// Append `[offset, min(offset + len, size))` of the opened object to
    /// `into`: never more than `len` bytes, nothing when `offset >= size`.
    /// Callers preallocate `into`; implementations must not grow it beyond
    /// `len` bytes of new content.
    async fn read_range(
        &self,
        obj: &ArchiveObject,
        offset: u64,
        len: u64,
        into: &mut Vec<u8>,
    ) -> Result<()> {
        let ArchiveVersion::Snapshot(bytes) = &obj.version else {
            anyhow::bail!(
                "read_range: object {} was not opened by this backend",
                obj.key
            );
        };
        let size = bytes.len() as u64;
        if offset >= size {
            return Ok(());
        }
        let end = offset.saturating_add(len).min(size);
        into.extend_from_slice(&bytes[offset as usize..end as usize]);
        Ok(())
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

/// Read size of `LocalBlobArchive::get_stream`: each chunk it yields, and
/// the open file's own buffer, are at most this big.
const LOCAL_STREAM_CHUNK: usize = 64 * 1024;

/// The temp files of an unfinished local write. If the write's future is
/// dropped (a cancelled request, mid-body) they are removed here,
/// synchronously: `Drop` cannot await, and an unlink is cheap. Every
/// finished write disarms it first.
struct TempFiles {
    paths: [PathBuf; 2],
    armed: bool,
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        if self.armed {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[async_trait]
impl BlobArchive for LocalBlobArchive {
    async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
        let body = futures_util::stream::once(async move { Ok(data) });
        self.put_stream(key, content_type, Box::pin(body)).await
    }

    /// Writes the body chunk by chunk to a temp file next to the target,
    /// fsyncs it, then renames it into place (data first, then the `.ct`
    /// sidecar). A body or write error removes the temp files and leaves
    /// any previous blob untouched.
    async fn put_stream(
        &self,
        key: &str,
        content_type: &str,
        body: BoxStream<'static, Result<Bytes>>,
    ) -> Result<()> {
        use futures_util::StreamExt;
        let mut body = body;
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
        let mut guard = TempFiles {
            paths: [tmp_data.clone(), tmp_ct.clone()],
            armed: true,
        };

        let write_result = async {
            // Data temp file: write chunk by chunk, flush, fsync. A body
            // error ends the write here, with no context of its own: the
            // caller's stream error is what matters.
            let mut f = fs::File::create(&tmp_data)
                .await
                .with_context(|| format!("create temp {}", tmp_data.display()))?;
            while let Some(chunk) = body.next().await {
                f.write_all(&chunk?)
                    .await
                    .with_context(|| format!("write temp {}", tmp_data.display()))?;
            }
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
        guard.armed = false;
        write_result
    }

    async fn get_stream(
        &self,
        key: &str,
    ) -> Result<Option<(String, BoxStream<'static, Result<Bytes>>)>> {
        use futures_util::StreamExt;
        let path = self.path_for(key)?;
        let mut file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        // Keep tokio's own file buffer at one chunk too.
        file.set_max_buf_size(LOCAL_STREAM_CHUNK);
        let content_type = fs::read_to_string(Self::ct_path_for(&path))
            .await
            .unwrap_or_else(|_| "application/octet-stream".to_string());
        let key = key.to_string();
        let stream = tokio_util::io::ReaderStream::with_capacity(file, LOCAL_STREAM_CHUNK)
            .map(move |chunk| chunk.with_context(|| format!("read {key}")));
        Ok(Some((content_type, Box::pin(stream))))
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

    async fn open(&self, key: &str) -> Result<Option<ArchiveObject>> {
        let path = self.path_for(key)?;
        let mut file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let size = file
            .metadata()
            .await
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        // tokio's `File` sizes its internal read buffer to the largest
        // request it has served (up to `max_buf_size`) and keeps it for the
        // handle's lifetime. `read_range` issues `RANGE` (1 MiB) requests,
        // which would otherwise leave a second `RANGE`-sized buffer alive
        // alongside the one spec § 3.4 already counts. Cap it at `CHUNK` (64
        // KiB) so the held file contributes only the one buffer the formula
        // names.
        file.set_max_buf_size(crate::log_read::CHUNK);
        Ok(Some(ArchiveObject {
            key: key.to_string(),
            size,
            version: ArchiveVersion::File(tokio::sync::Mutex::new(file)),
        }))
    }

    async fn read_range(
        &self,
        obj: &ArchiveObject,
        offset: u64,
        len: u64,
        into: &mut Vec<u8>,
    ) -> Result<()> {
        let ArchiveVersion::File(file) = &obj.version else {
            anyhow::bail!(
                "LocalBlobArchive::read_range: {} was not opened by this backend",
                obj.key
            );
        };
        if offset >= obj.size {
            return Ok(());
        }
        let len = len.min(obj.size - offset);
        let mut file = file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        (&mut *file)
            .take(len)
            .read_to_end(into)
            .await
            .with_context(|| format!("read {} bytes of {} at {offset}", len, obj.key))?;
        Ok(())
    }
}

#[cfg(feature = "s3")]
pub use s3_blob_archive::S3BlobArchive;

#[cfg(feature = "s3")]
mod s3_blob_archive {
    use super::*;
    use crate::config::ArchiveConfig;
    use crate::input_files::stream::S3_PART_BYTES;
    use aws_sdk_s3::error::DisplayErrorContext;
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_sdk_s3::operation::head_object::HeadObjectError;
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::types::{
        ChecksumAlgorithm, CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier,
    };

    /// Which precondition header pins a ranged GET to the version `open`
    /// saw. Pure and independent of the SDK's own `DateTime` type so it's
    /// trivially unit-testable.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Precondition {
        /// `If-Match: <etag>`.
        IfMatch(String),
        /// `If-Unmodified-Since: <http-date>`, used only when the endpoint
        /// returned no `ETag`.
        IfUnmodifiedSince(String),
        /// The endpoint returned neither header at `open`: nothing pins
        /// this read to a version (`open` already warned about it once).
        None,
    }

    /// `ETag` wins when present (S3 and every compatible store worth using
    /// gives one); `Last-Modified` is the fallback for the ETag-less
    /// endpoints C8 exists for.
    fn range_precondition(etag: Option<&str>, last_modified: Option<&str>) -> Precondition {
        match (etag, last_modified) {
            (Some(etag), _) => Precondition::IfMatch(etag.to_string()),
            (None, Some(last_modified)) => {
                Precondition::IfUnmodifiedSince(last_modified.to_string())
            }
            (None, None) => Precondition::None,
        }
    }

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

        async fn create_upload(&self, key: &str, content_type: &str) -> Result<OpenUpload> {
            let out = self
                .client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .content_type(content_type)
                .checksum_algorithm(ChecksumAlgorithm::Crc32)
                .send()
                .await
                .with_context(|| format!("S3 create multipart upload {key}"))?;
            let upload_id = out
                .upload_id()
                .with_context(|| format!("S3 create multipart upload {key}: no upload id"))?
                .to_string();
            Ok(OpenUpload {
                client: self.client.clone(),
                bucket: self.bucket.clone(),
                key: key.to_string(),
                upload_id,
                parts: Vec::new(),
                armed: true,
            })
        }

        /// The body of `put_stream`; on error the caller aborts `upload`.
        async fn put_parts(
            &self,
            key: &str,
            content_type: &str,
            mut body: BoxStream<'static, Result<Bytes>>,
            upload: &mut Option<OpenUpload>,
        ) -> Result<()> {
            use futures_util::StreamExt;
            // Grows to one part; see `push_capped`.
            let mut part: Vec<u8> = Vec::new();
            while let Some(chunk) = body.next().await {
                let mut chunk = chunk?;
                while !chunk.is_empty() {
                    if part.len() == S3_PART_BYTES {
                        // A full part, and more data follows: send it.
                        let full = Bytes::from(std::mem::take(&mut part));
                        let open = match upload {
                            Some(open) => open,
                            None => upload.insert(self.create_upload(key, content_type).await?),
                        };
                        open.send_part(full).await?;
                        part = Vec::with_capacity(S3_PART_BYTES);
                    }
                    let take = chunk.len().min(S3_PART_BYTES - part.len());
                    push_capped(&mut part, &chunk[..take]);
                    chunk = chunk.slice(take..);
                }
            }
            match upload {
                // The body ended inside the first part: one plain PUT.
                None => {
                    self.client
                        .put_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .content_type(content_type)
                        .body(ByteStream::from(Bytes::from(part)))
                        .send()
                        .await
                        .with_context(|| format!("S3 PUT {key}"))?;
                    Ok(())
                }
                Some(open) => {
                    // Non-empty: a part is sent only when more data follows.
                    open.send_part(Bytes::from(part)).await?;
                    open.complete().await
                }
            }
        }
    }

    /// Append `data` to `part`, doubling its capacity as needed but never
    /// past one part: a small body costs about its own size, and the
    /// buffer never grows beyond `S3_PART_BYTES`. The caller keeps
    /// `part.len() + data.len() <= S3_PART_BYTES`.
    fn push_capped(part: &mut Vec<u8>, data: &[u8]) {
        let need = part.len() + data.len();
        if need > part.capacity() {
            let target = (part.capacity() * 2).max(need).min(S3_PART_BYTES);
            part.reserve_exact(target - part.len());
        }
        part.extend_from_slice(data);
    }

    /// An open multipart upload. Aborted inline on an error (`abort`), or
    /// in the background if it is dropped while still armed -- the upload
    /// future was cancelled mid-body (a client disconnect) -- so no
    /// incomplete upload is left to accrue storage.
    struct OpenUpload {
        client: aws_sdk_s3::Client,
        bucket: String,
        key: String,
        upload_id: String,
        parts: Vec<CompletedPart>,
        armed: bool,
    }

    impl OpenUpload {
        async fn send_part(&mut self, data: Bytes) -> Result<()> {
            let number = i32::try_from(self.parts.len() + 1)
                .with_context(|| format!("S3 upload of {}: too many parts", self.key))?;
            let out = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(&self.key)
                .upload_id(&self.upload_id)
                .part_number(number)
                .checksum_algorithm(ChecksumAlgorithm::Crc32)
                .body(ByteStream::from(data))
                .send()
                .await
                .with_context(|| format!("S3 upload part {number} of {}", self.key))?;
            // The upload was created with CRC32, so completing it needs each
            // part's checksum as well as its ETag.
            self.parts.push(
                CompletedPart::builder()
                    .part_number(number)
                    .set_e_tag(out.e_tag().map(str::to_string))
                    .set_checksum_crc32(out.checksum_crc32().map(str::to_string))
                    .build(),
            );
            Ok(())
        }

        async fn complete(&mut self) -> Result<()> {
            let parts = CompletedMultipartUpload::builder()
                .set_parts(Some(std::mem::take(&mut self.parts)))
                .build();
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&self.key)
                .upload_id(&self.upload_id)
                .multipart_upload(parts)
                .send()
                .await
                .with_context(|| format!("S3 complete multipart upload {}", self.key))?;
            self.armed = false;
            Ok(())
        }

        /// Best effort: a failure is logged, never returned (the caller
        /// already has the error that made it abort).
        async fn abort(mut self) {
            self.armed = false;
            abort_upload(&self.client, &self.bucket, &self.key, &self.upload_id).await;
        }
    }

    impl Drop for OpenUpload {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                tracing::warn!(
                    key = %self.key,
                    "S3 multipart upload dropped outside a runtime; it is not aborted"
                );
                return;
            };
            let client = self.client.clone();
            let bucket = std::mem::take(&mut self.bucket);
            let key = std::mem::take(&mut self.key);
            let upload_id = std::mem::take(&mut self.upload_id);
            runtime.spawn(async move { abort_upload(&client, &bucket, &key, &upload_id).await });
        }
    }

    async fn abort_upload(client: &aws_sdk_s3::Client, bucket: &str, key: &str, upload_id: &str) {
        if let Err(e) = client
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
        {
            tracing::warn!(
                key,
                "S3 abort multipart upload failed: {}",
                DisplayErrorContext(&e)
            );
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

        /// Up to one part in memory: the body fills one `S3_PART_BYTES`
        /// buffer; a body that ends inside it is one `PutObject`, anything
        /// longer a multipart upload, aborted on any error.
        async fn put_stream(
            &self,
            key: &str,
            content_type: &str,
            body: BoxStream<'static, Result<Bytes>>,
        ) -> Result<()> {
            let mut upload = None;
            let result = self.put_parts(key, content_type, body, &mut upload).await;
            if result.is_err() {
                if let Some(open) = upload.take() {
                    open.abort().await;
                }
            }
            result
        }

        /// The object's body chunks as the SDK receives them; never
        /// collected.
        async fn get_stream(
            &self,
            key: &str,
        ) -> Result<Option<(String, BoxStream<'static, Result<Bytes>>)>> {
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
            let state = (Some(out.body), key.to_string());
            let stream = futures_util::stream::unfold(state, |(body, key)| async move {
                let mut body = body?;
                match body.try_next().await {
                    Ok(Some(chunk)) => Some((Ok(chunk), (Some(body), key))),
                    Ok(None) => None,
                    // End the stream after the error: never re-poll the body.
                    Err(e) => {
                        let err = anyhow::Error::new(e).context(format!("S3 read body {key}"));
                        Some((Err(err), (None, key)))
                    }
                }
            });
            Ok(Some((content_type, Box::pin(stream))))
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
                    let del_resp = self
                        .client
                        .delete_objects()
                        .bucket(&self.bucket)
                        .delete(delete)
                        .send()
                        .await
                        .with_context(|| format!("S3 BATCH DELETE {prefix}"))?;

                    // S3 batch DeleteObjects returns 200 OK even when individual
                    // keys fail (access denied, internal error, etc). The failures
                    // surface only in the response's per-object `Errors` array, so
                    // a successful HTTP call above does NOT mean every key was
                    // removed. If we ignored these the caller would believe the
                    // prefix was wiped and (in retention sweeps) drop the DB rows
                    // that point at the still-present blobs, orphaning them.
                    let errs = del_resp.errors();
                    if !errs.is_empty() {
                        let total = errs.len();
                        let mut sample: Vec<String> = errs
                            .iter()
                            .take(10)
                            .map(|e| {
                                format!(
                                    "{}: {}{}",
                                    e.key().unwrap_or("<no-key>"),
                                    e.code().unwrap_or("<no-code>"),
                                    e.message().map(|m| format!(" ({m})")).unwrap_or_default(),
                                )
                            })
                            .collect();
                        if total > sample.len() {
                            sample.push(format!("... and {} more", total - sample.len()));
                        }
                        anyhow::bail!(
                            "S3 BATCH DELETE {prefix}: {total} object(s) failed: [{}]",
                            sample.join(", ")
                        );
                    }
                }
                if resp.is_truncated().unwrap_or(false) {
                    continuation = resp.next_continuation_token().map(|s| s.to_string());
                } else {
                    break;
                }
            }
            Ok(())
        }

        async fn open(&self, key: &str) -> Result<Option<ArchiveObject>> {
            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(out) => {
                    let etag = out.e_tag().map(str::to_string);
                    // The builder's `if_unmodified_since` wants the SDK's own
                    // `DateTime`, but `ArchiveVersion` (feature-agnostic) can
                    // only hold a `String` -- format it once here with the
                    // SDK's own HTTP-date formatter, parsed back with the
                    // same formatter in `read_range`. Lossless: HTTP-date
                    // has second precision, which is all `Last-Modified`
                    // ever carries.
                    let last_modified = out.last_modified().and_then(|dt| {
                        dt.fmt(aws_sdk_s3::primitives::DateTimeFormat::HttpDate)
                            .inspect_err(|e| {
                                tracing::warn!(
                                    "formatting Last-Modified for {key} failed, \
                                     ranged reads will not be version-pinned by it: {e}"
                                )
                            })
                            .ok()
                    });
                    if etag.is_none() && last_modified.is_none() {
                        tracing::warn!(
                            "archive endpoint returned no ETag or Last-Modified; \
                             ranged reads of {key} are not version-pinned"
                        );
                    }
                    Ok(Some(ArchiveObject {
                        key: key.to_string(),
                        size: u64::try_from(out.content_length().unwrap_or(0)).unwrap_or(0),
                        version: ArchiveVersion::S3 {
                            etag,
                            last_modified,
                        },
                    }))
                }
                Err(e) => {
                    if matches!(e.as_service_error(), Some(HeadObjectError::NotFound(_))) {
                        return Ok(None);
                    }
                    Err(anyhow::anyhow!(e)).context(format!("S3 HEAD {key}"))
                }
            }
        }

        async fn read_range(
            &self,
            obj: &ArchiveObject,
            offset: u64,
            len: u64,
            into: &mut Vec<u8>,
        ) -> Result<()> {
            let ArchiveVersion::S3 {
                etag,
                last_modified,
            } = &obj.version
            else {
                anyhow::bail!(
                    "S3BlobArchive::read_range: {} was not opened by this backend",
                    obj.key
                );
            };
            if offset >= obj.size || len == 0 {
                return Ok(());
            }
            let want = len.min(obj.size - offset);
            let mut req = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&obj.key)
                .range(format!("bytes={offset}-{}", offset + want - 1));
            match range_precondition(etag.as_deref(), last_modified.as_deref()) {
                Precondition::IfMatch(etag) => req = req.if_match(etag),
                Precondition::IfUnmodifiedSince(http_date) => {
                    let dt = aws_sdk_s3::primitives::DateTime::from_str(
                        &http_date,
                        aws_sdk_s3::primitives::DateTimeFormat::HttpDate,
                    )
                    .with_context(|| format!("parse stored Last-Modified for {}", obj.key))?;
                    req = req.if_unmodified_since(dt);
                }
                Precondition::None => {}
            }
            let out = match req.send().await {
                Ok(out) => out,
                Err(e) => {
                    match e.raw_response().map(|r| r.status().as_u16()) {
                        Some(416) => return Ok(()),
                        Some(412) => anyhow::bail!(
                            "S3 object {} changed since it was opened (If-Match/If-Unmodified-Since failed)",
                            obj.key
                        ),
                        _ => {}
                    }
                    return Err(anyhow::anyhow!(e)).context(format!("S3 ranged GET {}", obj.key));
                }
            };
            // Read through the SDK's async adapter into the caller's buffer:
            // no `collect()`, so no segment list and no contiguous copy.
            let reader = tokio::io::AsyncReadExt::take(out.body.into_async_read(), want);
            let mut reader = std::pin::pin!(reader);
            tokio::io::AsyncReadExt::read_to_end(&mut reader, into)
                .await
                .with_context(|| format!("S3 read range of {}", obj.key))?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod precondition_tests {
        use super::*;

        #[test]
        fn etag_wins_when_present() {
            assert_eq!(
                range_precondition(Some("\"abc\""), Some("Mon, 01 Jan 2024 00:00:00 GMT")),
                Precondition::IfMatch("\"abc\"".to_string())
            );
            assert_eq!(
                range_precondition(Some("\"abc\""), None),
                Precondition::IfMatch("\"abc\"".to_string())
            );
        }

        #[test]
        fn last_modified_is_the_fallback_without_an_etag() {
            assert_eq!(
                range_precondition(None, Some("Mon, 01 Jan 2024 00:00:00 GMT")),
                Precondition::IfUnmodifiedSince("Mon, 01 Jan 2024 00:00:00 GMT".to_string())
            );
        }

        #[test]
        fn neither_header_pins_nothing() {
            assert_eq!(range_precondition(None, None), Precondition::None);
        }
    }

    #[cfg(test)]
    mod part_buffer_tests {
        use super::*;

        #[test]
        fn push_capped_grows_with_the_body_and_stops_at_one_part() {
            let mut part = Vec::new();
            push_capped(&mut part, &[1u8; 3]);
            assert!(part.capacity() < 64, "a tiny body stays tiny");

            // Odd-sized chunks would double past a part without the cap.
            let chunk = vec![7u8; 100_000];
            while part.len() < S3_PART_BYTES {
                let take = chunk.len().min(S3_PART_BYTES - part.len());
                push_capped(&mut part, &chunk[..take]);
                assert!(part.capacity() <= S3_PART_BYTES, "{}", part.capacity());
            }
            assert_eq!(part.len(), S3_PART_BYTES);
            assert_eq!(part.capacity(), S3_PART_BYTES);
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

    /// Regression: when the `.ct` sidecar is absent (e.g. left over from a
    /// pre-sidecar release, externally-rsync'd backup, or partial recovery),
    /// `get` must still return the blob with `application/octet-stream` rather
    /// than fail or return None. We simulate the missing-sidecar case by
    /// writing the data file directly via `tokio::fs::write`, bypassing
    /// `put`.
    #[tokio::test]
    async fn local_blob_get_without_sidecar_falls_back_to_octet_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());

        // Plant a data file directly — no `.ct` sidecar.
        let key = "ws/job/step/manual.bin";
        let path = tmp.path().join(key);
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(&path, b"manually written").await.unwrap();

        // Sanity check: no sidecar file exists for this key.
        let sidecar = path.with_file_name(format!(
            "{}.ct",
            path.file_name().unwrap().to_string_lossy()
        ));
        assert!(
            !sidecar.exists(),
            "test setup: sidecar must not exist for this case"
        );

        let got = store.get(key).await.unwrap().expect("blob should be found");
        assert_eq!(
            got.content_type, "application/octet-stream",
            "missing sidecar must fall back to octet-stream"
        );
        assert_eq!(&got.bytes[..], b"manually written");
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

    #[tokio::test]
    async fn default_open_and_read_range_slice_a_snapshot() {
        let store = InMemoryBlob::new();
        assert!(store.open("missing").await.unwrap().is_none());
        store
            .put("k", "t", Bytes::from_static(b"0123456789"))
            .await
            .unwrap();
        let obj = store.open("k").await.unwrap().unwrap();
        assert_eq!((obj.key.as_str(), obj.size), ("k", 10));
        let mut v = Vec::new();
        store.read_range(&obj, 2, 3, &mut v).await.unwrap();
        assert_eq!(v, b"234");
        v.clear();
        store.read_range(&obj, 8, 10, &mut v).await.unwrap();
        assert_eq!(v, b"89");
        v.clear();
        store.read_range(&obj, 10, 5, &mut v).await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn local_open_and_read_range() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        assert!(store.open("a/missing.gz").await.unwrap().is_none());
        store
            .put(
                "a/obj.gz",
                "application/gzip",
                Bytes::from_static(b"0123456789"),
            )
            .await
            .unwrap();
        let obj = store.open("a/obj.gz").await.unwrap().unwrap();
        assert_eq!(obj.size, 10);
        let mut v = Vec::with_capacity(64);
        store.read_range(&obj, 2, 3, &mut v).await.unwrap();
        assert_eq!(v, b"234");
        v.clear();
        store.read_range(&obj, 8, 10, &mut v).await.unwrap();
        assert_eq!(v, b"89");
        v.clear();
        store.read_range(&obj, 12, 5, &mut v).await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn local_read_range_serves_the_opened_version_after_the_path_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        store
            .put("a/obj.gz", "t", Bytes::from_static(b"old-content"))
            .await
            .unwrap();
        let obj = store.open("a/obj.gz").await.unwrap().unwrap();
        store
            .put("a/obj.gz", "t", Bytes::from_static(b"NEW-CONTENT!!"))
            .await
            .unwrap();
        let mut v = Vec::new();
        store.read_range(&obj, 0, 100, &mut v).await.unwrap();
        assert_eq!(v, b"old-content");
    }

    fn chunks(parts: Vec<Result<Bytes>>) -> BoxStream<'static, Result<Bytes>> {
        Box::pin(futures_util::stream::iter(parts))
    }

    /// Every leftover temp file (`<name>.tmp.<uuid>`, `<name>.ct.tmp.<uuid>`)
    /// under `dir`.
    fn temp_entries(dir: &Path) -> Vec<String> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.path().display().to_string())
            .filter(|p| p.contains(".tmp."))
            .collect()
    }

    #[tokio::test]
    async fn local_put_stream_roundtrip_and_no_temp_left() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let body = chunks(vec![
            Ok(Bytes::from_static(b"hello ")),
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"streamed ")),
            Ok(Bytes::from_static(b"world")),
        ]);
        store
            .put_stream("inputs/ws/file_1", "text/plain", body)
            .await
            .unwrap();
        let got = store.get("inputs/ws/file_1").await.unwrap().unwrap();
        assert_eq!(got.content_type, "text/plain");
        assert_eq!(&got.bytes[..], b"hello streamed world");

        // Overwrite replaces both the bytes and the content type.
        store
            .put_stream(
                "inputs/ws/file_1",
                "application/json",
                chunks(vec![Ok(Bytes::from_static(b"{}"))]),
            )
            .await
            .unwrap();
        let got = store.get("inputs/ws/file_1").await.unwrap().unwrap();
        assert_eq!(got.content_type, "application/json");
        assert_eq!(&got.bytes[..], b"{}");

        // An empty body is an empty blob, not a missing one.
        store
            .put_stream("inputs/ws/file_2", "text/plain", chunks(vec![]))
            .await
            .unwrap();
        let got = store.get("inputs/ws/file_2").await.unwrap().unwrap();
        assert!(got.bytes.is_empty());

        assert!(
            temp_entries(tmp.path()).is_empty(),
            "temp files left: {:?}",
            temp_entries(tmp.path())
        );
    }

    #[tokio::test]
    async fn local_put_stream_error_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let err = store
            .put_stream(
                "inputs/ws/file_new",
                "text/plain",
                chunks(vec![
                    Ok(Bytes::from_static(b"first ")),
                    Ok(Bytes::from_static(b"second")),
                    Err(anyhow::anyhow!("client went away")),
                ]),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("client went away"), "{err:#}");
        assert!(store.get("inputs/ws/file_new").await.unwrap().is_none());
        assert!(
            temp_entries(tmp.path()).is_empty(),
            "temp files left: {:?}",
            temp_entries(tmp.path())
        );

        // A failed overwrite keeps the previous blob and its content type.
        store
            .put(
                "inputs/ws/file_kept",
                "text/plain",
                Bytes::from_static(b"v1"),
            )
            .await
            .unwrap();
        store
            .put_stream(
                "inputs/ws/file_kept",
                "application/x-new",
                chunks(vec![
                    Ok(Bytes::from_static(b"v2")),
                    Err(anyhow::anyhow!("client went away")),
                ]),
            )
            .await
            .unwrap_err();
        let got = store.get("inputs/ws/file_kept").await.unwrap().unwrap();
        assert_eq!(&got.bytes[..], b"v1");
        assert_eq!(got.content_type, "text/plain");
        assert!(temp_entries(tmp.path()).is_empty());
    }

    /// The first chunk reaches the temp file on disk before the second one
    /// is even asked for: nothing is buffered up to the end of the body.
    #[tokio::test]
    async fn local_put_stream_writes_chunk_by_chunk() {
        use futures_util::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let dir = tmp.path().join("inputs/ws");
        let body = futures_util::stream::iter([0u8, 1]).then(move |i| {
            let dir = dir.clone();
            async move {
                if i == 0 {
                    return Ok(Bytes::from(vec![b'x'; 4096]));
                }
                // The temp data file holds the first chunk (allow the
                // blocking write a moment to land).
                for _ in 0..200 {
                    let on_disk: u64 = std::fs::read_dir(&dir)
                        .map(|rd| {
                            rd.filter_map(|e| e.ok())
                                .filter(|e| {
                                    let n = e.file_name().to_string_lossy().into_owned();
                                    n.contains(".tmp.") && !n.contains(".ct.")
                                })
                                .filter_map(|e| e.metadata().ok())
                                .map(|m| m.len())
                                .sum()
                        })
                        .unwrap_or(0);
                    if on_disk == 4096 {
                        return Ok(Bytes::from_static(b"tail"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                anyhow::bail!("the first chunk never reached the temp file")
            }
        });
        store
            .put_stream("inputs/ws/file_cbc", "text/plain", Box::pin(body))
            .await
            .unwrap();
        let got = store.get("inputs/ws/file_cbc").await.unwrap().unwrap();
        assert_eq!(got.bytes.len(), 4096 + 4);
    }

    /// A dropped upload (the request future is cancelled mid-body) leaves no
    /// temp file behind.
    #[tokio::test]
    async fn local_put_stream_dropped_mid_body_leaves_no_temp() {
        use futures_util::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(LocalBlobArchive::new(tmp.path().to_path_buf()));
        let body = futures_util::stream::iter([Ok(Bytes::from_static(b"first"))])
            .chain(futures_util::stream::pending());
        let task = {
            let store = std::sync::Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .put_stream("inputs/ws/file_drop", "text/plain", Box::pin(body))
                    .await
            })
        };
        for _ in 0..200 {
            if !temp_entries(tmp.path()).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !temp_entries(tmp.path()).is_empty(),
            "the upload never started writing"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            temp_entries(tmp.path()).is_empty(),
            "temp files left: {:?}",
            temp_entries(tmp.path())
        );
        assert!(store.get("inputs/ws/file_drop").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn local_get_stream_yields_multiple_chunks() {
        use futures_util::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        store
            .put(
                "inputs/ws/file_big",
                "application/x-big",
                Bytes::from(data.clone()),
            )
            .await
            .unwrap();
        let (ct, mut stream) = store
            .get_stream("inputs/ws/file_big")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ct, "application/x-big");
        let mut got = Vec::new();
        let mut n = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            assert!(chunk.len() <= 64 * 1024, "chunk of {} bytes", chunk.len());
            got.extend_from_slice(&chunk);
            n += 1;
        }
        assert!(
            n >= 2,
            "a 1 MiB blob must stream in several chunks, got {n}"
        );
        assert_eq!(got, data);
        assert!(store
            .get_stream("inputs/ws/file_missing")
            .await
            .unwrap()
            .is_none());
        assert!(store.get_stream("../escape").await.is_err());
    }

    #[tokio::test]
    async fn local_get_stream_without_sidecar_falls_back_to_octet_stream() {
        use futures_util::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let path = tmp.path().join("inputs/ws/file_raw");
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(&path, b"raw").await.unwrap();
        let (ct, mut stream) = store
            .get_stream("inputs/ws/file_raw")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ct, "application/octet-stream");
        assert_eq!(&stream.next().await.unwrap().unwrap()[..], b"raw");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn read_range_rejects_an_object_opened_by_another_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalBlobArchive::new(tmp.path().to_path_buf());
        let foreign = ArchiveObject {
            key: "k".into(),
            size: 3,
            version: ArchiveVersion::Snapshot(Bytes::from_static(b"abc")),
        };
        assert!(store
            .read_range(&foreign, 0, 3, &mut Vec::new())
            .await
            .is_err());
    }
}
