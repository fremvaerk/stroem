//! Streaming core for input-file uploads and downloads (spec § 4.2, § 9):
//! no path holds a whole file in memory.

use crate::blob_storage::BlobArchive;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use sha2::{Digest, Sha256};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

/// Size of one S3 multipart part, and so of the one part buffer an S3
/// upload holds.
pub const S3_PART_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub enum PutError {
    /// The body went over `max` bytes; nothing was kept.
    TooLarge { max: u64 },
    /// The request body itself failed (client went away, bad chunk).
    Body(anyhow::Error),
    /// The storage backend failed.
    Storage(anyhow::Error),
}

/// Fixed, value-free texts: never a key, file name or body content.
impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PutError::TooLarge { max } => write!(f, "file is larger than {max} bytes"),
            PutError::Body(_) => f.write_str("the upload body could not be read"),
            PutError::Storage(_) => f.write_str("the file could not be stored"),
        }
    }
}

impl std::error::Error for PutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PutError::TooLarge { .. } => None,
            PutError::Body(e) | PutError::Storage(e) => Some(e.as_ref()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Stored {
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the stored bytes.
    pub sha256: String,
}

/// Counts and hashes `body` while `archive.put_stream` consumes it; over
/// `max_bytes` the stream errors, the backend discards the partial object,
/// and the result is `Err(TooLarge)`. Never buffers more than one backend
/// part.
pub async fn put_counted(
    archive: &dyn BlobArchive,
    key: &str,
    content_type: &str,
    body: BoxStream<'static, anyhow::Result<Bytes>>,
    max_bytes: u64,
) -> Result<Stored, PutError> {
    let tally = Arc::new(Mutex::new(Tally::default()));
    let counted = Counted {
        inner: body,
        tally: Arc::clone(&tally),
        max: max_bytes,
        done: false,
    };
    let result = archive
        .put_stream(key, content_type, Box::pin(counted))
        .await;

    // Classify from what the counter saw, never from the backend's error
    // text (each backend wraps the stream error in its own context).
    let (size_bytes, hasher, too_large, body_err) = {
        let mut t = lock(&tally);
        (
            t.size,
            std::mem::take(&mut t.hasher),
            t.too_large,
            t.body_err.take(),
        )
    };
    if too_large || body_err.is_some() {
        if result.is_ok() {
            // The backend reported success for a body it should have
            // discarded: do not keep it.
            if let Err(e) = archive.delete(key).await {
                tracing::warn!(key, "discarding an incomplete upload failed: {e:#}");
            }
        }
        return Err(match body_err {
            Some(e) if !too_large => PutError::Body(e),
            _ => PutError::TooLarge { max: max_bytes },
        });
    }
    result.map_err(PutError::Storage)?;
    Ok(Stored {
        size_bytes,
        sha256: hex::encode(hasher.finalize()),
    })
}

/// Response body that streams the blob; `None` when the blob is missing.
pub async fn download_body(
    archive: &dyn BlobArchive,
    key: &str,
) -> anyhow::Result<Option<axum::body::Body>> {
    Ok(archive
        .get_stream(key)
        .await?
        .map(|(_content_type, stream)| axum::body::Body::from_stream(stream)))
}

/// What the counted stream has seen so far.
#[derive(Default)]
struct Tally {
    size: u64,
    hasher: Sha256,
    too_large: bool,
    body_err: Option<anyhow::Error>,
}

fn lock(tally: &Mutex<Tally>) -> MutexGuard<'_, Tally> {
    tally
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The error the counted stream yields in place of an over-limit chunk or
/// a failed body chunk. `put_counted` classifies from the `Tally`, never
/// from this.
#[derive(Debug)]
struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the upload body stopped")
    }
}

impl std::error::Error for Stopped {}

/// `inner`, counted and hashed chunk by chunk. The lock is taken only
/// inside a synchronous `poll_next`, never across an await. After the
/// first error it ends and never polls `inner` again.
struct Counted {
    inner: BoxStream<'static, anyhow::Result<Bytes>>,
    tally: Arc<Mutex<Tally>>,
    max: u64,
    done: bool,
}

impl Stream for Counted {
    type Item = anyhow::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let item = match this.inner.as_mut().poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(item) => item,
        };
        let mut tally = lock(&this.tally);
        match item {
            None => {
                this.done = true;
                Poll::Ready(None)
            }
            Some(Ok(chunk)) => {
                tally.size = tally.size.saturating_add(chunk.len() as u64);
                if tally.size > this.max {
                    tally.too_large = true;
                    this.done = true;
                    return Poll::Ready(Some(Err(Stopped.into())));
                }
                tally.hasher.update(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Some(Err(e)) => {
                tally.body_err = Some(e);
                this.done = true;
                Poll::Ready(Some(Err(Stopped.into())))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_storage::{Blob, LocalBlobArchive};
    use anyhow::Result;
    use async_trait::async_trait;
    use futures_util::StreamExt;
    use std::path::Path;

    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;

    fn chunks(parts: Vec<Result<Bytes>>) -> BoxStream<'static, Result<Bytes>> {
        Box::pin(futures_util::stream::iter(parts))
    }

    fn temp_entries(dir: &Path) -> Vec<String> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.path().display().to_string())
            .filter(|p| p.contains(".tmp."))
            .collect()
    }

    #[tokio::test]
    async fn put_counted_reports_size_and_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = LocalBlobArchive::new(tmp.path().to_path_buf());
        let body = chunks(vec![
            Ok(Bytes::from_static(b"a")),
            Ok(Bytes::from_static(b"bc")),
        ]);
        let stored = put_counted(&archive, "inputs/ws/file_abc", "text/plain", body, 1024)
            .await
            .unwrap();
        assert_eq!(stored.size_bytes, 3);
        assert_eq!(
            stored.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let blob = archive.get("inputs/ws/file_abc").await.unwrap().unwrap();
        assert_eq!(&blob.bytes[..], b"abc");
        assert_eq!(blob.content_type, "text/plain");
    }

    #[tokio::test]
    async fn put_counted_accepts_a_body_of_exactly_max_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = LocalBlobArchive::new(tmp.path().to_path_buf());
        let body = chunks(vec![Ok(Bytes::from_static(b"abc"))]);
        let stored = put_counted(&archive, "inputs/ws/file_max", "text/plain", body, 3)
            .await
            .unwrap();
        assert_eq!(stored.size_bytes, 3);
    }

    #[tokio::test]
    async fn put_counted_of_an_empty_body_is_the_empty_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = LocalBlobArchive::new(tmp.path().to_path_buf());
        let stored = put_counted(
            &archive,
            "inputs/ws/file_empty",
            "text/plain",
            chunks(vec![]),
            3,
        )
        .await
        .unwrap();
        assert_eq!(stored.size_bytes, 0);
        assert_eq!(
            stored.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let blob = archive.get("inputs/ws/file_empty").await.unwrap().unwrap();
        assert!(blob.bytes.is_empty());
    }

    #[tokio::test]
    async fn put_counted_over_limit_is_too_large_and_keeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = LocalBlobArchive::new(tmp.path().to_path_buf());
        // A chunked request: no Content-Length, so only the stream can tell.
        let body = chunks(
            (0..3)
                .map(|_| Ok(Bytes::from(vec![b'x'; 400 * KIB])))
                .collect(),
        );
        let err = put_counted(
            &archive,
            "inputs/ws/file_big",
            "application/octet-stream",
            body,
            MIB as u64,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, PutError::TooLarge { max } if max == MIB as u64),
            "unexpected: {err:?}"
        );
        assert_eq!(err.to_string(), "file is larger than 1048576 bytes");
        assert!(archive.get("inputs/ws/file_big").await.unwrap().is_none());
        assert!(temp_entries(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn put_counted_body_error_is_body_and_keeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = LocalBlobArchive::new(tmp.path().to_path_buf());
        let body = chunks(vec![
            Ok(Bytes::from_static(b"partial")),
            Err(anyhow::anyhow!("connection reset by peer")),
        ]);
        let err = put_counted(&archive, "inputs/ws/file_cut", "text/plain", body, 1024)
            .await
            .unwrap_err();
        let PutError::Body(e) = err else {
            panic!("expected Body, got {err:?}");
        };
        assert_eq!(e.to_string(), "connection reset by peer");
        assert!(archive.get("inputs/ws/file_cut").await.unwrap().is_none());
        assert!(temp_entries(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn put_counted_backend_error_is_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = LocalBlobArchive::new(tmp.path().to_path_buf());
        let body = chunks(vec![Ok(Bytes::from_static(b"abc"))]);
        let err = put_counted(&archive, "../escape", "text/plain", body, 1024)
            .await
            .unwrap_err();
        assert!(matches!(err, PutError::Storage(_)), "unexpected: {err:?}");
    }

    /// A backend that swallows a stream error and stores what it got: the
    /// over-limit object must still not be kept.
    struct Lenient(LocalBlobArchive);

    #[async_trait]
    impl BlobArchive for Lenient {
        async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
            self.0.put(key, content_type, data).await
        }
        async fn get(&self, key: &str) -> Result<Option<Blob>> {
            self.0.get(key).await
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.0.delete(key).await
        }
        async fn delete_prefix(&self, prefix: &str) -> Result<()> {
            self.0.delete_prefix(prefix).await
        }
        async fn put_stream(
            &self,
            key: &str,
            content_type: &str,
            mut body: BoxStream<'static, Result<Bytes>>,
        ) -> Result<()> {
            let mut kept = Vec::new();
            while let Some(Ok(chunk)) = body.next().await {
                kept.extend_from_slice(&chunk);
            }
            self.0.put(key, content_type, kept.into()).await
        }
    }

    #[tokio::test]
    async fn put_counted_deletes_what_a_lenient_backend_kept_over_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = Lenient(LocalBlobArchive::new(tmp.path().to_path_buf()));
        let body = chunks(vec![
            Ok(Bytes::from_static(b"12345")),
            Ok(Bytes::from_static(b"67890")),
        ]);
        let err = put_counted(&archive, "inputs/ws/file_lenient", "text/plain", body, 8)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PutError::TooLarge { max: 8 }),
            "unexpected: {err:?}"
        );
        assert!(archive
            .get("inputs/ws/file_lenient")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn download_body_streams_the_blob_or_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = LocalBlobArchive::new(tmp.path().to_path_buf());
        let data: Vec<u8> = (0..300 * KIB).map(|i| (i % 251) as u8).collect();
        archive
            .put(
                "inputs/ws/file_dl",
                "application/octet-stream",
                data.clone().into(),
            )
            .await
            .unwrap();
        let body = download_body(&archive, "inputs/ws/file_dl")
            .await
            .unwrap()
            .expect("blob exists");
        let mut frames = body.into_data_stream();
        let mut got = Vec::new();
        let mut n = 0;
        while let Some(frame) = frames.next().await {
            got.extend_from_slice(&frame.unwrap());
            n += 1;
        }
        assert_eq!(got, data);
        assert!(n >= 2, "a 300 KiB blob streams in 64 KiB frames, got {n}");
        assert!(download_body(&archive, "inputs/ws/file_missing")
            .await
            .unwrap()
            .is_none());
    }
}
