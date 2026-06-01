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
