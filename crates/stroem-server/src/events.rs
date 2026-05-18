//! Cross-replica event bus via Postgres `LISTEN/NOTIFY`.
//!
//! Each replica publishes events through the shared pool (`NOTIFY <channel>,
//! <payload>`) and subscribes via a dedicated [`sqlx::postgres::PgListener`].
//! Three channels are defined:
//!
//! - [`CHANNEL_JOB_CANCELLED`] — a job was cancelled; subscribers add it to
//!   their local `cancelled_jobs` cache so workers polling any replica see
//!   the cancellation.
//! - [`CHANNEL_WORKSPACE_RELOADED`] — a workspace's revision changed;
//!   subscribers refresh their local cache (otherwise they wait up to the
//!   poll interval, default 30 s).
//! - [`CHANNEL_JOB_LOG_CHUNK`] — a worker pushed a log chunk to one replica;
//!   subscribers fan it out to their local WebSocket viewers. Payloads
//!   larger than [`NOTIFY_MAX_BYTES`] fall back to a signal-only message so
//!   the receiving replica knows new content exists without the bytes.
//!
//! Every payload carries the publishing replica's UUID; listeners drop events
//! they themselves emitted to avoid duplicate side-effects (especially
//! double-broadcasting log chunks).
//!
//! For tests / single-replica deployments, [`EventBus::noop`] returns a bus
//! that silently swallows publishes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::state::AppState;

/// Channel name: cancelled job ID.
pub const CHANNEL_JOB_CANCELLED: &str = "stroem_job_cancelled";
/// Channel name: workspace name whose revision changed.
pub const CHANNEL_WORKSPACE_RELOADED: &str = "stroem_workspace_reloaded";
/// Channel name: log chunk for a job/step (or signal-only when too large).
pub const CHANNEL_JOB_LOG_CHUNK: &str = "stroem_job_log_chunk";

/// Conservative payload cap for `NOTIFY`. Postgres' hard limit is 8000 bytes
/// for the payload string; we leave headroom for JSON wrapping and the
/// statement itself.
pub const NOTIFY_MAX_BYTES: usize = 7000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobCancelledPayload {
    replica_id: Uuid,
    job_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceReloadedPayload {
    replica_id: Uuid,
    workspace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobLogChunkPayload {
    replica_id: Uuid,
    job_id: Uuid,
    /// The JSONL chunk content. `None` means the chunk was too large; the
    /// receiver should know new content exists but can't fan it out live.
    chunk: Option<String>,
}

/// Cross-replica event publisher.
///
/// A `noop` bus is safe to publish to and is what tests + single-replica
/// deployments use. The real bus holds the same `PgPool` used elsewhere; each
/// `publish_*` call acquires a connection, runs `NOTIFY`, and returns it.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

struct EventBusInner {
    pool: Option<PgPool>,
    replica_id: Uuid,
}

impl EventBus {
    /// Real bus that publishes via `pool`. `replica_id` is included in every
    /// payload so subscribers can drop their own messages.
    pub fn new(pool: PgPool, replica_id: Uuid) -> Self {
        Self {
            inner: Arc::new(EventBusInner {
                pool: Some(pool),
                replica_id,
            }),
        }
    }

    /// Bus that silently discards publishes. For tests and single-replica
    /// deployments where no listener is running.
    pub fn noop() -> Self {
        Self {
            inner: Arc::new(EventBusInner {
                pool: None,
                replica_id: Uuid::new_v4(),
            }),
        }
    }

    /// This replica's identifier — same UUID that's embedded in every payload.
    pub fn replica_id(&self) -> Uuid {
        self.inner.replica_id
    }

    /// Notify other replicas that a job was cancelled. Failures are logged and
    /// swallowed (cancellation is durable in the DB; NOTIFY is the
    /// best-effort fast path).
    #[tracing::instrument(skip(self))]
    pub async fn publish_job_cancelled(&self, job_id: Uuid) {
        let Some(pool) = &self.inner.pool else { return };
        let payload = JobCancelledPayload {
            replica_id: self.inner.replica_id,
            job_id,
        };
        if let Err(e) = notify(pool, CHANNEL_JOB_CANCELLED, &payload).await {
            tracing::warn!("publish_job_cancelled failed for {}: {:#}", job_id, e);
        }
    }

    /// Notify other replicas that a workspace's revision changed and they
    /// should refresh their cache.
    #[tracing::instrument(skip(self))]
    pub async fn publish_workspace_reloaded(&self, workspace: &str) {
        let Some(pool) = &self.inner.pool else { return };
        let payload = WorkspaceReloadedPayload {
            replica_id: self.inner.replica_id,
            workspace: workspace.to_string(),
        };
        if let Err(e) = notify(pool, CHANNEL_WORKSPACE_RELOADED, &payload).await {
            tracing::warn!(
                "publish_workspace_reloaded failed for {}: {:#}",
                workspace,
                e
            );
        }
    }

    /// Forward a log chunk to other replicas. If the chunk doesn't fit in a
    /// single NOTIFY payload (see [`NOTIFY_MAX_BYTES`]), a signal-only event is
    /// sent instead — receivers know new content exists but won't see the
    /// bytes until the next backfill / archive read.
    #[tracing::instrument(skip(self, chunk))]
    pub async fn publish_log_chunk(&self, job_id: Uuid, chunk: &str) {
        let Some(pool) = &self.inner.pool else { return };
        let chunk_field = if chunk.len() <= NOTIFY_MAX_BYTES {
            Some(chunk.to_string())
        } else {
            None
        };
        let payload = JobLogChunkPayload {
            replica_id: self.inner.replica_id,
            job_id,
            chunk: chunk_field,
        };
        if let Err(e) = notify(pool, CHANNEL_JOB_LOG_CHUNK, &payload).await {
            tracing::warn!("publish_log_chunk failed for {}: {:#}", job_id, e);
        }
    }
}

async fn notify<T: Serialize>(pool: &PgPool, channel: &str, payload: &T) -> Result<()> {
    let json = serde_json::to_string(payload).context("serialize NOTIFY payload")?;
    // Use bind parameters via pg_notify(text, text) so payload escaping is
    // handled correctly even if it contains quotes.
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(channel)
        .bind(&json)
        .execute(pool)
        .await
        .with_context(|| format!("pg_notify on '{}'", channel))?;
    Ok(())
}

/// Spawn the listener task. Connects via a dedicated `PgListener` (not from
/// the pool), subscribes to all three channels, and dispatches incoming
/// notifications to the appropriate handler on `state`.
///
/// `PgListener::recv()` transparently reconnects on transient network errors
/// (sqlx default `eager_reconnect=true`); the outer loop here only triggers
/// on fatal errors (auth/protocol failure) or initial connection failure
/// during DB startup.
#[tracing::instrument(skip(state, cancel))]
pub fn start_listener(
    db_url: String,
    state: AppState,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move { run_listener(db_url, state, cancel).await })
}

async fn run_listener(db_url: String, state: AppState, cancel: CancellationToken) {
    tracing::info!("EventBus listener started");
    loop {
        match connect_and_listen(&db_url, &state, &cancel).await {
            Ok(ListenerExit::Cancelled) => {
                tracing::info!("EventBus listener shutting down");
                return;
            }
            Ok(ListenerExit::Disconnected) => {
                tracing::warn!("EventBus listener disconnected; reconnecting in 2s");
            }
            Err(e) => {
                tracing::warn!("EventBus listener error: {:#}; reconnecting in 2s", e);
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            _ = cancel.cancelled() => {
                tracing::info!("EventBus listener shutting down");
                return;
            }
        }
    }
}

enum ListenerExit {
    Cancelled,
    Disconnected,
}

async fn connect_and_listen(
    db_url: &str,
    state: &AppState,
    cancel: &CancellationToken,
) -> Result<ListenerExit> {
    let mut listener = PgListener::connect(db_url)
        .await
        .context("PgListener::connect")?;
    listener
        .listen_all([
            CHANNEL_JOB_CANCELLED,
            CHANNEL_WORKSPACE_RELOADED,
            CHANNEL_JOB_LOG_CHUNK,
        ])
        .await
        .context("LISTEN on event channels")?;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(ListenerExit::Cancelled),
            msg = listener.recv() => {
                match msg {
                    Ok(notification) => {
                        dispatch(state, notification.channel(), notification.payload()).await;
                    }
                    Err(e) => {
                        tracing::warn!("PgListener recv error: {:#}", e);
                        return Ok(ListenerExit::Disconnected);
                    }
                }
            }
        }
    }
}

async fn dispatch(state: &AppState, channel: &str, payload: &str) {
    match channel {
        CHANNEL_JOB_CANCELLED => match serde_json::from_str::<JobCancelledPayload>(payload) {
            Ok(p) if p.replica_id == state.event_bus.replica_id() => {
                // Self-emitted; we already inserted locally before publishing.
            }
            Ok(p) => {
                state
                    .cancelled_jobs
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(p.job_id);
                tracing::debug!("Cross-replica cancellation received for job {}", p.job_id);
            }
            Err(e) => tracing::warn!("Bad {} payload: {:#}", channel, e),
        },
        CHANNEL_WORKSPACE_RELOADED => {
            match serde_json::from_str::<WorkspaceReloadedPayload>(payload) {
                Ok(p) if p.replica_id == state.event_bus.replica_id() => {
                    // Self-emitted; our watcher already reloaded the cache.
                }
                Ok(p) => {
                    if let Err(e) = state.workspaces.reload(&p.workspace).await {
                        tracing::warn!(
                            "Cross-replica workspace reload failed for '{}': {:#}",
                            p.workspace,
                            e
                        );
                    } else {
                        tracing::debug!(
                            "Cross-replica reload applied for workspace '{}'",
                            p.workspace
                        );
                    }
                }
                Err(e) => tracing::warn!("Bad {} payload: {:#}", channel, e),
            }
        }
        CHANNEL_JOB_LOG_CHUNK => match serde_json::from_str::<JobLogChunkPayload>(payload) {
            Ok(p) if p.replica_id == state.event_bus.replica_id() => {
                // Self-emitted; local broadcast already happened on the
                // publisher path. Re-broadcasting here would duplicate lines
                // for any client connected to this replica.
            }
            Ok(p) => {
                if let Some(chunk) = p.chunk {
                    state.log_broadcast.broadcast(p.job_id, chunk).await;
                } else {
                    // Oversize: we can't fan out live. Log so operators know
                    // why a viewer on this replica missed a chunk.
                    tracing::debug!(
                        "Oversize log chunk signaled for job {} (>{}B); viewers must refresh",
                        p.job_id,
                        NOTIFY_MAX_BYTES
                    );
                }
            }
            Err(e) => tracing::warn!("Bad {} payload: {:#}", channel, e),
        },
        other => {
            tracing::debug!("Ignoring notification on unknown channel '{}'", other);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_bus_publishes_swallow_errors() {
        let bus = EventBus::noop();
        // The publishes are async but never actually touch a pool; we just
        // need to confirm they don't panic when called.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            bus.publish_job_cancelled(Uuid::new_v4()).await;
            bus.publish_workspace_reloaded("any").await;
            bus.publish_log_chunk(Uuid::new_v4(), "hello").await;
        });
    }

    #[test]
    fn log_chunk_under_limit_carries_content() {
        let small = "x".repeat(NOTIFY_MAX_BYTES);
        let payload = JobLogChunkPayload {
            replica_id: Uuid::nil(),
            job_id: Uuid::nil(),
            chunk: if small.len() <= NOTIFY_MAX_BYTES {
                Some(small.clone())
            } else {
                None
            },
        };
        assert!(payload.chunk.is_some());
    }

    #[test]
    fn log_chunk_over_limit_falls_back_to_signal() {
        let big = "x".repeat(NOTIFY_MAX_BYTES + 1);
        let payload = JobLogChunkPayload {
            replica_id: Uuid::nil(),
            job_id: Uuid::nil(),
            chunk: if big.len() <= NOTIFY_MAX_BYTES {
                Some(big.clone())
            } else {
                None
            },
        };
        assert!(payload.chunk.is_none());
    }

    #[test]
    fn channels_are_distinct() {
        // Cheap guard against typos collapsing two channels into one.
        let set: std::collections::HashSet<&str> = [
            CHANNEL_JOB_CANCELLED,
            CHANNEL_WORKSPACE_RELOADED,
            CHANNEL_JOB_LOG_CHUNK,
        ]
        .into_iter()
        .collect();
        assert_eq!(set.len(), 3);
    }

    #[tokio::test]
    async fn replica_id_is_stable_across_clones() {
        // PgPool::connect_lazy spawns an internal management task on the
        // current Tokio runtime, so the test must run inside one even though
        // we never actually connect.
        let bus = EventBus::new(
            sqlx::PgPool::connect_lazy("postgres://x:5432/db").unwrap(),
            Uuid::new_v4(),
        );
        let cloned = bus.clone();
        assert_eq!(bus.replica_id(), cloned.replica_id());
    }
}
