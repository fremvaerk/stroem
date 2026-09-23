//! Terminal-job archive reads: ranged gzip decoding, tails, streams, merge.

use super::{RANGE, READ_SLACK};
use crate::blob_storage::{ArchiveObject, BlobArchive};
use anyhow::Context as _;
use futures_util::future::BoxFuture;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

/// `AsyncBufRead` over an opened archive object, fetched in ranges of
/// `range` bytes into ONE buffer that moves into each fetch and back, so a
/// finished range never coexists with the next.
pub(crate) struct ArchiveRangeReader {
    archive: Arc<dyn BlobArchive>,
    obj: Arc<ArchiveObject>,
    range: u64,
    pos: u64,
    buf: Vec<u8>,
    consumed: usize,
    fetch: Option<BoxFuture<'static, anyhow::Result<Vec<u8>>>>,
}

impl ArchiveRangeReader {
    pub(crate) fn new(archive: Arc<dyn BlobArchive>, obj: Arc<ArchiveObject>) -> Self {
        Self::with_range(archive, obj, RANGE)
    }

    pub(crate) fn with_range(
        archive: Arc<dyn BlobArchive>,
        obj: Arc<ArchiveObject>,
        range: u64,
    ) -> Self {
        Self {
            archive,
            obj,
            range,
            pos: 0,
            buf: Vec::with_capacity(range as usize + READ_SLACK),
            consumed: 0,
            fetch: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn range_buffer_capacity(&self) -> usize {
        self.buf.capacity()
    }
}

impl AsyncBufRead for ArchiveRangeReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.consumed < this.buf.len() {
            return Poll::Ready(Ok(&this.buf[this.consumed..]));
        }
        if this.fetch.is_none() {
            if this.pos >= this.obj.size {
                return Poll::Ready(Ok(&[]));
            }
            let mut buf = std::mem::take(&mut this.buf);
            buf.clear();
            this.consumed = 0;
            let (archive, obj, pos, range) = (
                Arc::clone(&this.archive),
                Arc::clone(&this.obj),
                this.pos,
                this.range,
            );
            this.fetch = Some(Box::pin(async move {
                archive.read_range(&obj, pos, range, &mut buf).await?;
                Ok(buf)
            }));
        }
        let fetch = this.fetch.as_mut().expect("fetch was just set");
        let result = ready!(fetch.as_mut().poll(cx));
        this.fetch = None;
        match result {
            Ok(buf) if buf.is_empty() => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("archive object {} is shorter than its size", this.obj.key),
            ))),
            Ok(buf) => {
                this.pos += buf.len() as u64;
                this.buf = buf;
                Poll::Ready(Ok(&this.buf[..]))
            }
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        this.consumed = (this.consumed + amt).min(this.buf.len());
    }
}

impl AsyncRead for ArchiveRangeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let avail = ready!(Pin::new(&mut *this).poll_fill_buf(cx))?;
        let n = avail.len().min(out.remaining());
        out.put_slice(&avail[..n]);
        this.consumed += n;
        Poll::Ready(Ok(()))
    }
}

/// The decompressed length from a gzip member's trailer (ISIZE, mod 2^32).
/// Our uploads are one member (`GzEncoder` … `finish()`), so this is exact
/// under 4 GiB; callers still bound every decompression with `take`.
pub(crate) async fn gzip_isize(
    archive: &dyn BlobArchive,
    obj: &ArchiveObject,
) -> anyhow::Result<u64> {
    anyhow::ensure!(
        obj.size >= 18,
        "archive object {} is {} bytes, too short to be gzip",
        obj.key,
        obj.size
    );
    let mut trailer = Vec::with_capacity(4 + READ_SLACK);
    archive
        .read_range(obj, obj.size - 4, 4, &mut trailer)
        .await?;
    let bytes: [u8; 4] = trailer
        .as_slice()
        .try_into()
        .with_context(|| format!("short gzip trailer read from {}", obj.key))?;
    Ok(u64::from(u32::from_le_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_storage::LocalBlobArchive;
    use tokio::io::AsyncReadExt;

    pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    pub(crate) async fn put(
        dir: &tempfile::TempDir,
        bytes: Vec<u8>,
    ) -> (Arc<dyn BlobArchive>, Arc<ArchiveObject>) {
        let store: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(dir.path().to_path_buf()));
        store
            .put("ws/t/obj.jsonl.gz", "application/gzip", bytes.into())
            .await
            .unwrap();
        let obj = store.open("ws/t/obj.jsonl.gz").await.unwrap().unwrap();
        (store, Arc::new(obj))
    }

    #[tokio::test]
    async fn range_reader_reads_the_whole_object_in_ranges() {
        for data in [&b"0123456789"[..], &b"012345678"[..], &b""[..]] {
            let dir = tempfile::TempDir::new().unwrap();
            let (store, obj) = put(&dir, data.to_vec()).await;
            let mut reader = ArchiveRangeReader::with_range(store, obj, 3);
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, data);
            assert_eq!(
                reader.range_buffer_capacity(),
                3 + READ_SLACK,
                "range buffer must be reused"
            );
        }
    }

    #[tokio::test]
    async fn gzip_isize_reads_the_trailer() {
        let dir = tempfile::TempDir::new().unwrap();
        let (store, obj) = put(&dir, gzip(b"hello world")).await;
        assert_eq!(gzip_isize(store.as_ref(), &obj).await.unwrap(), 11);

        let dir2 = tempfile::TempDir::new().unwrap();
        let (store2, obj2) = put(&dir2, b"short".to_vec()).await;
        assert!(gzip_isize(store2.as_ref(), &obj2).await.is_err());
    }
}
