use std::collections::HashMap;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

const CHANNEL_CAPACITY: usize = 256;

/// Per-job broadcast channels for real-time log streaming
pub struct LogBroadcast {
    channels: RwLock<HashMap<Uuid, broadcast::Sender<String>>>,
}

impl LogBroadcast {
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to log messages for a job. Creates the channel on first subscribe.
    pub async fn subscribe(&self, job_id: Uuid) -> broadcast::Receiver<String> {
        let mut channels = self.channels.write().await;
        let sender = channels
            .entry(job_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0);
        sender.subscribe()
    }

    /// Broadcast a log chunk to all subscribers for a job. No-op if no subscribers.
    pub async fn broadcast(&self, job_id: Uuid, chunk: String) {
        let channels = self.channels.read().await;
        if let Some(sender) = channels.get(&job_id) {
            // Ignore send errors (no active receivers)
            let _ = sender.send(chunk);
        }
    }

    /// Remove a channel when no subscribers remain.
    pub async fn remove_channel(&self, job_id: Uuid) {
        let mut channels = self.channels.write().await;
        if let Some(sender) = channels.get(&job_id) {
            if sender.receiver_count() == 0 {
                channels.remove(&job_id);
            }
        }
    }
}

impl Default for LogBroadcast {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_subscribe_and_receive() {
        let lb = LogBroadcast::new();
        let job_id = Uuid::new_v4();

        let mut rx = lb.subscribe(job_id).await;
        lb.broadcast(job_id, "hello\n".to_string()).await;

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg, "hello\n");
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let lb = LogBroadcast::new();
        let job_id = Uuid::new_v4();

        let mut rx1 = lb.subscribe(job_id).await;
        let mut rx2 = lb.subscribe(job_id).await;

        lb.broadcast(job_id, "line1\n".to_string()).await;

        assert_eq!(rx1.recv().await.unwrap(), "line1\n");
        assert_eq!(rx2.recv().await.unwrap(), "line1\n");
    }

    #[tokio::test]
    async fn test_broadcast_no_subscribers() {
        let lb = LogBroadcast::new();
        let job_id = Uuid::new_v4();
        // Should not panic
        lb.broadcast(job_id, "nobody listening\n".to_string()).await;
    }

    #[tokio::test]
    async fn test_cleanup_after_drop() {
        let lb = LogBroadcast::new();
        let job_id = Uuid::new_v4();

        let rx = lb.subscribe(job_id).await;
        drop(rx);

        lb.remove_channel(job_id).await;

        let channels = lb.channels.read().await;
        assert!(!channels.contains_key(&job_id));
    }

    #[tokio::test]
    async fn test_different_jobs_isolated() {
        let lb = LogBroadcast::new();
        let job1 = Uuid::new_v4();
        let job2 = Uuid::new_v4();

        let mut rx1 = lb.subscribe(job1).await;
        let mut rx2 = lb.subscribe(job2).await;

        lb.broadcast(job1, "job1-msg\n".to_string()).await;
        lb.broadcast(job2, "job2-msg\n".to_string()).await;

        assert_eq!(rx1.recv().await.unwrap(), "job1-msg\n");
        assert_eq!(rx2.recv().await.unwrap(), "job2-msg\n");
    }
}
