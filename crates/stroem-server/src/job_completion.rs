use std::collections::HashMap;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

/// Event broadcast when a job reaches terminal state.
#[derive(Debug, Clone)]
pub struct JobCompletionEvent {
    pub job_id: Uuid,
    pub status: String,
    pub output: Option<serde_json::Value>,
}

/// Per-job one-shot completion notifier for sync webhook support.
///
/// Callers subscribe before creating a job, then await the receiver.
/// When the job reaches terminal state, `notify()` broadcasts the event
/// and removes the channel (one-shot semantics).
pub struct JobCompletionNotifier {
    channels: RwLock<HashMap<Uuid, broadcast::Sender<JobCompletionEvent>>>,
}

impl JobCompletionNotifier {
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to completion events for a job. Creates the channel on demand.
    pub async fn subscribe(&self, job_id: Uuid) -> broadcast::Receiver<JobCompletionEvent> {
        let mut channels = self.channels.write().await;
        let sender = channels
            .entry(job_id)
            .or_insert_with(|| broadcast::channel(1).0);
        sender.subscribe()
    }

    /// Broadcast a completion event and remove the channel (one-shot).
    /// No-op if no subscribers exist.
    pub async fn notify(&self, event: JobCompletionEvent) {
        let mut channels = self.channels.write().await;
        if let Some(sender) = channels.remove(&event.job_id) {
            let _ = sender.send(event);
        }
    }
}

impl Default for JobCompletionNotifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_subscribe_and_receive() {
        let notifier = JobCompletionNotifier::new();
        let job_id = Uuid::new_v4();

        let mut rx = notifier.subscribe(job_id).await;

        notifier
            .notify(JobCompletionEvent {
                job_id,
                status: "completed".to_string(),
                output: Some(serde_json::json!({"result": "ok"})),
            })
            .await;

        let event = rx.recv().await.unwrap();
        assert_eq!(event.job_id, job_id);
        assert_eq!(event.status, "completed");
        assert_eq!(event.output.unwrap()["result"], "ok");
    }

    #[tokio::test]
    async fn test_notify_without_subscriber_no_panic() {
        let notifier = JobCompletionNotifier::new();
        let job_id = Uuid::new_v4();

        // Should not panic
        notifier
            .notify(JobCompletionEvent {
                job_id,
                status: "failed".to_string(),
                output: None,
            })
            .await;
    }

    #[tokio::test]
    async fn test_channel_removed_after_notify() {
        let notifier = JobCompletionNotifier::new();
        let job_id = Uuid::new_v4();

        let _rx = notifier.subscribe(job_id).await;

        notifier
            .notify(JobCompletionEvent {
                job_id,
                status: "completed".to_string(),
                output: None,
            })
            .await;

        // Channel should be removed — second subscribe creates a new one
        let channels = notifier.channels.read().await;
        assert!(!channels.contains_key(&job_id));
    }

    #[tokio::test]
    async fn test_multiple_subscribers_same_job() {
        let notifier = JobCompletionNotifier::new();
        let job_id = Uuid::new_v4();

        let mut rx1 = notifier.subscribe(job_id).await;
        let mut rx2 = notifier.subscribe(job_id).await;

        notifier
            .notify(JobCompletionEvent {
                job_id,
                status: "completed".to_string(),
                output: None,
            })
            .await;

        assert_eq!(rx1.recv().await.unwrap().status, "completed");
        assert_eq!(rx2.recv().await.unwrap().status, "completed");
    }

    #[tokio::test]
    async fn test_different_jobs_isolated() {
        let notifier = JobCompletionNotifier::new();
        let job1 = Uuid::new_v4();
        let job2 = Uuid::new_v4();

        let mut rx1 = notifier.subscribe(job1).await;
        let mut rx2 = notifier.subscribe(job2).await;

        notifier
            .notify(JobCompletionEvent {
                job_id: job1,
                status: "completed".to_string(),
                output: None,
            })
            .await;

        // rx1 should receive
        assert_eq!(rx1.recv().await.unwrap().status, "completed");

        // rx2 should not have received anything yet (job2 not notified)
        assert!(rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_timeout_behavior() {
        let notifier = JobCompletionNotifier::new();
        let job_id = Uuid::new_v4();

        let mut rx = notifier.subscribe(job_id).await;

        // Simulate a timeout — no notification sent
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;

        assert!(result.is_err()); // Elapsed
    }
}
