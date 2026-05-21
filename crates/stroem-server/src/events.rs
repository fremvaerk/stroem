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
/// for the **JSON-encoded** payload string. The wrapper adds ~125 bytes for
/// UUIDs + field names. JSON string escaping expands bytes differently depending
/// on content: `\"` and `\\` each expand 2×, common whitespace escapes (`\n`,
/// `\r`, `\t`) expand 2×, but control bytes below 0x20 (other than `\n`, `\r`,
/// `\t`) encode as `\u00XX` — a 6× expansion. Pathological all-control-byte
/// content could therefore still exceed Postgres' 8 KB hard limit at 3500 raw
/// bytes. In practice, worker stdout/stderr is human-readable text where
/// escaping is dominated by occasional `\"` and newlines (both 2×), so 3500
/// raw bytes stays comfortably under `3500*2 + 125 = 7125` encoded bytes.
///
/// This is also the per-line signal-only threshold: a single jsonl line that
/// exceeds this cap can't be split further, so it degrades to a signal-only
/// NOTIFY (receivers learn new content exists but get no bytes).
pub const NOTIFY_MAX_BYTES: usize = 3500;

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

    /// Forward a log chunk to other replicas. Chunks larger than
    /// [`NOTIFY_MAX_BYTES`] are split on jsonl line boundaries so multi-line
    /// batches still cross the NOTIFY channel intact; a single line that
    /// exceeds the cap on its own falls back to a signal-only event for that
    /// segment (receivers know new content exists but can't render it live).
    ///
    /// **Cancel safety**: the inner segment loop publishes segments one at a
    /// time, in order. This sequential ordering is load-bearing: receivers
    /// depend on segments arriving in the same order they were produced so that
    /// `append_log` writes are stable. Do NOT parallelize the loop (e.g. with
    /// `join_all`) and do NOT place this future in a `select!` arm where it may
    /// be dropped mid-iteration — doing so would leave segments partially
    /// delivered with no recovery path.
    #[tracing::instrument(skip(self, chunk))]
    pub async fn publish_log_chunk(&self, job_id: Uuid, chunk: &str) {
        let Some(pool) = &self.inner.pool else { return };

        if chunk.len() <= NOTIFY_MAX_BYTES {
            self.publish_log_segment(pool, job_id, Some(chunk.to_string()))
                .await;
            return;
        }

        for segment in split_jsonl_chunk(chunk, NOTIFY_MAX_BYTES) {
            match segment {
                ChunkSegment::Inline(bytes) => {
                    self.publish_log_segment(pool, job_id, Some(bytes)).await;
                }
                ChunkSegment::SignalOnly => {
                    self.publish_log_segment(pool, job_id, None).await;
                }
            }
        }
    }

    async fn publish_log_segment(&self, pool: &PgPool, job_id: Uuid, chunk: Option<String>) {
        let payload = JobLogChunkPayload {
            replica_id: self.inner.replica_id,
            job_id,
            chunk,
        };
        if let Err(e) = notify(pool, CHANNEL_JOB_LOG_CHUNK, &payload).await {
            tracing::warn!("publish_log_chunk failed for {}: {:#}", job_id, e);
        }
    }
}

/// One NOTIFY-sized piece of a larger jsonl chunk.
enum ChunkSegment {
    /// Inline content — small enough to ride in a single NOTIFY payload.
    Inline(String),
    /// One jsonl line on its own exceeds the cap. Emitted as `chunk: None`
    /// so receivers know fresh content exists without the bytes.
    SignalOnly,
}

/// Split a jsonl chunk into NOTIFY-sized segments on `\n` boundaries.
///
/// Each `Inline` segment is at most `max_bytes` and ends at a jsonl line
/// boundary. A line that is itself larger than `max_bytes` is emitted as
/// `SignalOnly` — the receiver can fan out the "new content" signal but
/// can't write the bytes locally for that line.
fn split_jsonl_chunk(chunk: &str, max_bytes: usize) -> Vec<ChunkSegment> {
    let mut result = Vec::new();
    let mut current = String::new();

    for line in chunk.split_inclusive('\n') {
        if line.len() > max_bytes {
            if !current.is_empty() {
                result.push(ChunkSegment::Inline(std::mem::take(&mut current)));
            }
            result.push(ChunkSegment::SignalOnly);
            continue;
        }
        if current.len() + line.len() > max_bytes {
            result.push(ChunkSegment::Inline(std::mem::take(&mut current)));
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        result.push(ChunkSegment::Inline(current));
    }
    result
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
                // Self-emitted; local disk write + broadcast already happened
                // on the publisher path. Mirroring or re-broadcasting here
                // would duplicate lines for any client connected to this
                // replica.
            }
            Ok(p) => {
                if let Some(chunk) = p.chunk {
                    // Mirror the chunk into this replica's local jsonl file so
                    // REST log reads served from this pod see the same content
                    // a viewer would. Without this, GET /api/jobs/:id/logs
                    // returns empty whenever the load balancer routes to a
                    // replica that didn't receive the worker's push.
                    if let Err(e) = state.log_storage.append_log(p.job_id, &chunk).await {
                        tracing::warn!(
                            "Cross-replica log mirror failed for job {}: {:#}",
                            p.job_id,
                            e
                        );
                    }
                    state.log_broadcast.broadcast(p.job_id, chunk).await;
                } else {
                    // Oversize single jsonl line: NOTIFY carried no bytes so we
                    // can't mirror or fan out live. The canonical bytes live on
                    // the publishing replica until the archive upload at job
                    // terminal.
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

    fn jsonl_line(i: usize) -> String {
        format!(
            "{{\"ts\":\"2026-01-01T00:00:00Z\",\"stream\":\"stdout\",\"step\":\"s\",\"line\":\"{i}\"}}\n"
        )
    }

    #[test]
    fn split_jsonl_chunk_small_returns_single_segment() {
        // A chunk under the cap should be left whole.
        let chunk = format!("{}{}{}", jsonl_line(1), jsonl_line(2), jsonl_line(3));
        let segments = split_jsonl_chunk(&chunk, NOTIFY_MAX_BYTES);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            ChunkSegment::Inline(bytes) => assert_eq!(bytes, &chunk),
            ChunkSegment::SignalOnly => panic!("small chunk must not signal-only"),
        }
    }

    #[test]
    fn split_jsonl_chunk_splits_multi_line_on_boundary() {
        // 200 jsonl lines well over the cap — every segment must end in '\n'
        // and concatenate back to the original bytes.
        let mut chunk = String::new();
        for i in 0..200 {
            chunk.push_str(&jsonl_line(i));
        }
        assert!(chunk.len() > NOTIFY_MAX_BYTES);

        let segments = split_jsonl_chunk(&chunk, NOTIFY_MAX_BYTES);
        assert!(segments.len() > 1, "must produce multiple segments");

        let mut reassembled = String::new();
        for seg in &segments {
            match seg {
                ChunkSegment::Inline(bytes) => {
                    assert!(bytes.len() <= NOTIFY_MAX_BYTES);
                    assert!(
                        bytes.ends_with('\n'),
                        "every inline segment must end at a line boundary"
                    );
                    reassembled.push_str(bytes);
                }
                ChunkSegment::SignalOnly => panic!("no line in this chunk is oversize"),
            }
        }
        assert_eq!(reassembled, chunk);
    }

    #[test]
    fn split_jsonl_chunk_single_huge_line_emits_signal_only() {
        // One jsonl line larger than the cap can't be split — emit signal-only
        // for that line, but keep the surrounding small lines as inline.
        let small_before = jsonl_line(0);
        let huge = format!("{}\n", "x".repeat(NOTIFY_MAX_BYTES + 1));
        let small_after = jsonl_line(1);
        let chunk = format!("{}{}{}", small_before, huge, small_after);

        let segments = split_jsonl_chunk(&chunk, NOTIFY_MAX_BYTES);

        // Expected pattern: Inline(small_before), SignalOnly, Inline(small_after)
        assert_eq!(segments.len(), 3);
        match &segments[0] {
            ChunkSegment::Inline(b) => assert_eq!(b, &small_before),
            ChunkSegment::SignalOnly => panic!("segment 0 should be inline"),
        }
        assert!(matches!(segments[1], ChunkSegment::SignalOnly));
        match &segments[2] {
            ChunkSegment::Inline(b) => assert_eq!(b, &small_after),
            ChunkSegment::SignalOnly => panic!("segment 2 should be inline"),
        }
    }

    #[test]
    fn split_jsonl_chunk_handles_no_trailing_newline() {
        // Defensive: input without a trailing '\n' still emits the tail as
        // a single inline segment (no data lost).
        let chunk = "a\nb\nc";
        let segments = split_jsonl_chunk(chunk, NOTIFY_MAX_BYTES);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            ChunkSegment::Inline(b) => assert_eq!(b, chunk),
            ChunkSegment::SignalOnly => panic!(),
        }
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

    // ── Splitter edge cases ────────────────────────────────────────────────

    #[test]
    fn split_jsonl_chunk_empty_input_returns_empty_vec() {
        let segments = split_jsonl_chunk("", NOTIFY_MAX_BYTES);
        assert!(segments.is_empty(), "empty input must produce no segments");
    }

    #[test]
    fn split_jsonl_chunk_bare_newline_returns_single_inline() {
        let segments = split_jsonl_chunk("\n", NOTIFY_MAX_BYTES);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            ChunkSegment::Inline(b) => assert_eq!(b, "\n"),
            ChunkSegment::SignalOnly => panic!("bare newline is tiny — must be Inline"),
        }
    }

    #[test]
    fn split_jsonl_chunk_single_oversize_no_newline_emits_signal_only() {
        // A line that exceeds the cap but has no trailing '\n' must still
        // degrade to SignalOnly (split_inclusive emits it as a single token).
        let big = "x".repeat(NOTIFY_MAX_BYTES + 1);
        let segments = split_jsonl_chunk(&big, NOTIFY_MAX_BYTES);
        assert_eq!(segments.len(), 1);
        assert!(
            matches!(segments[0], ChunkSegment::SignalOnly),
            "oversize line with no trailing newline must emit SignalOnly"
        );
    }

    #[test]
    fn split_jsonl_chunk_two_consecutive_oversize_lines_each_signal_only() {
        // Two consecutive lines each larger than the cap must become two
        // separate SignalOnly segments — they must not be merged into one.
        let big_line = format!("{}\n", "x".repeat(NOTIFY_MAX_BYTES + 1));
        let chunk = format!("{}{}", big_line, big_line);
        let segments = split_jsonl_chunk(&chunk, NOTIFY_MAX_BYTES);
        assert_eq!(
            segments.len(),
            2,
            "two oversize lines must produce exactly two SignalOnly segments"
        );
        assert!(matches!(segments[0], ChunkSegment::SignalOnly));
        assert!(matches!(segments[1], ChunkSegment::SignalOnly));
    }

    #[test]
    fn split_jsonl_chunk_zero_max_bytes_every_line_signals_only() {
        // With max_bytes = 0, every line — even a bare '\n' (len 1) — exceeds
        // the cap and must become SignalOnly.
        let segments = split_jsonl_chunk("a\nb\n", 0);
        assert_eq!(
            segments.len(),
            2,
            "each of the two lines must become its own SignalOnly segment"
        );
        assert!(matches!(segments[0], ChunkSegment::SignalOnly));
        assert!(matches!(segments[1], ChunkSegment::SignalOnly));
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
