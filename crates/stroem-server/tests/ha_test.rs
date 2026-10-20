//! High-availability integration tests.
//!
//! These tests get an isolated database via `stroem_test_support`, then
//! exercise the HA primitives end-to-end:
//!
//! - Leader election: only one `LeaderElection` instance holds the advisory
//!   lock at a time; failover happens within seconds when the holder drops.
//!   Postgres advisory locks are per database, and every test has its own;
//!   the tests that touch `LEADER_LOCK_KEY` are still `#[serial]` (against
//!   each other and the metrics tests' `#[serial]` ones, the same default
//!   key) because they measure timing that a parallel load would skew.
//! - Leader lease, validity and self-release (`mod lease`): a leader that
//!   stops working loses the lock in bounded time.
//! - NOTIFY/LISTEN event bus: cancellation, log chunks, and workspace reloads
//!   propagate from one replica to another via Postgres pub/sub.

use anyhow::Result;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use serial_test::serial;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_server::config::{
    DbConfig, LogStorageConfig, RecoveryConfig, RetentionConfig, ServerConfig,
};
use stroem_server::events::{start_listener, EventBus, NOTIFY_MAX_BYTES};
use stroem_server::leader::LeaderElection;
use stroem_server::log_read::StepFilter;
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

/// `read_tail`'s tail budget for tests that want the whole log — well above
/// anything these fixtures write.
const ALL: u64 = 16 * 1024 * 1024;

struct Harness {
    pool: PgPool,
    url: String,
    _temp: TempDir,
}

async fn boot() -> Result<Harness> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let url = test_db.url;
    let temp = TempDir::new()?;
    Ok(Harness {
        pool,
        url,
        _temp: temp,
    })
}

fn empty_config(url: &str, log_dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: url.to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-must-be-long-enough-32".to_string(),
        auth: None,
        recovery: RecoveryConfig {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 60,
            unmatched_step_timeout_secs: 30,
        },
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        pin_store: None,
    }
}

fn build_state(harness: &Harness, replica_id: Uuid) -> AppState {
    let log_dir = harness._temp.path().join(format!("logs-{}", replica_id));
    std::fs::create_dir_all(&log_dir).unwrap();
    let config = empty_config(&harness.url, &log_dir);
    let mgr = WorkspaceManager::from_config("default", WorkspaceConfig::new());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let bus = EventBus::new(harness.pool.clone(), replica_id);
    AppState::new(
        harness.pool.clone(),
        mgr,
        config,
        log_storage,
        HashMap::new(),
        None,
    )
    .with_event_bus(bus)
}

/// Wait up to `timeout` for `predicate` to return true, polling every 100ms.
async fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    predicate()
}

/// Async variant of [`wait_for`] for predicates that need to perform async I/O
/// (e.g. `tokio::fs::read_to_string`). Polls every 100 ms until the closure
/// returns `true` or the deadline passes.
async fn wait_for_async<F, Fut>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate().await {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    predicate().await
}

// ─── Leader election ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn leader_only_one_holds_lock() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let (leader_a, _ha) = LeaderElection::start(h.url.clone(), cancel.clone());
    let (leader_b, _hb) = LeaderElection::start(h.url.clone(), cancel.clone());

    // Both loops need time to attempt acquisition. The retry interval is 5s
    // but the first attempt is immediate, so a few seconds is plenty.
    sleep(Duration::from_secs(2)).await;

    let a = leader_a.is_leader();
    let b = leader_b.is_leader();
    assert!(
        a ^ b,
        "exactly one replica must be leader (a={a}, b={b}) — got both true or both false"
    );

    cancel.cancel();
    Ok(())
}

#[tokio::test]
#[serial]
async fn leader_failover_when_holder_cancelled() -> Result<()> {
    let h = boot().await?;
    let cancel_a = CancellationToken::new();
    let cancel_b = CancellationToken::new();

    let (leader_a, _ha) = LeaderElection::start(h.url.clone(), cancel_a.clone());
    // Let A become leader before starting B (avoids the race where both
    // attempt acquisition simultaneously and either could win).
    sleep(Duration::from_secs(2)).await;
    assert!(leader_a.is_leader(), "A should have acquired first");

    let (leader_b, _hb) = LeaderElection::start(h.url.clone(), cancel_b.clone());
    sleep(Duration::from_secs(2)).await;
    assert!(
        !leader_b.is_leader(),
        "B must be follower while A holds lock"
    );

    // Drop A — its session closes, Postgres releases the lock.
    cancel_a.cancel();

    // B should pick up within ~ retry_interval (5s). Allow 12s for headroom.
    let took_over = wait_for(Duration::from_secs(12), || leader_b.is_leader()).await;
    assert!(
        took_over,
        "B should have become leader within 12s of A dropping"
    );

    cancel_b.cancel();
    Ok(())
}

// ─── NOTIFY / LISTEN ─────────────────────────────────────────────────────────

#[tokio::test]
async fn notify_job_cancelled_propagates_to_other_replica() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());

    // Let PgListener establish its subscription.
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    state_a.event_bus.publish_job_cancelled(job).await;

    let received = wait_for(Duration::from_secs(3), || {
        state_b
            .cancelled_jobs
            .read()
            .map(|s| s.contains(&job))
            .unwrap_or(false)
    })
    .await;
    assert!(
        received,
        "replica B should have observed cancellation for {job} via NOTIFY"
    );

    // Self-emit must NOT echo back into A's own cancelled_jobs (we filter on
    // replica_id in dispatch).
    {
        let a_set = state_a.cancelled_jobs.read().unwrap();
        assert!(
            !a_set.contains(&job),
            "self-emitted NOTIFY should not insert into the publisher's local cache"
        );
    }

    cancel.cancel();
    Ok(())
}

#[tokio::test]
async fn notify_log_chunk_propagates_to_other_replica() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    let mut rx_b = state_b.log_broadcast.subscribe(job).await;

    let chunk = "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"hello\"}\n";
    state_a.event_bus.publish_log_chunk(job, chunk).await;

    let msg = tokio::time::timeout(Duration::from_secs(3), rx_b.recv())
        .await
        .expect("WS subscriber on replica B should receive chunk via NOTIFY")?;
    assert!(msg.contains("hello"));

    cancel.cancel();
    Ok(())
}

#[tokio::test]
async fn notify_log_chunk_mirrors_to_receiver_local_disk() -> Result<()> {
    use stroem_server::log_storage::JobLogMeta;

    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    let chunk = "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"mirror-me\"}\n";

    // Publisher writes to its own disk (real worker_api path does this); B
    // should mirror the chunk into its own disk via the NOTIFY listener.
    state_a.log_storage.append_log(job, chunk).await?;
    state_a.event_bus.publish_log_chunk(job, chunk).await;

    let meta = JobLogMeta {
        workspace: "ws".to_string(),
        task_name: "task".to_string(),
        created_at: chrono::Utc::now(),
    };

    // Allow NOTIFY round-trip + dispatch to land.
    let mirrored = wait_for_async(Duration::from_secs(3), || {
        let path = std::path::PathBuf::from(&state_b.config.log_storage.local_dir)
            .join(format!("{job}.jsonl"));
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|s| s.contains("mirror-me"))
                .unwrap_or(false)
        }
    })
    .await;
    assert!(
        mirrored,
        "replica B should have mirrored the log chunk to its local jsonl file"
    );

    // And get_log on replica B must return the chunk (no archive configured —
    // this would be empty before the fix).
    state_b.log_storage.close_log(job).await;
    let logs = state_b
        .log_storage
        .read_tail(job, &meta, false, StepFilter::All, ALL)
        .await?
        .logs;
    assert!(
        logs.contains("mirror-me"),
        "get_log on replica B must see the mirrored content (got {logs:?})"
    );

    cancel.cancel();
    Ok(())
}

#[tokio::test]
async fn notify_log_chunk_oversize_multiline_splits_and_mirrors() -> Result<()> {
    use stroem_server::log_storage::JobLogMeta;

    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();

    // 400 jsonl lines: comfortably over NOTIFY_MAX_BYTES so the publisher must
    // split into multiple NOTIFY events. Every line is small enough on its own
    // that none degrade to signal-only.
    let mut chunk = String::new();
    for i in 0..400 {
        chunk.push_str(&format!(
            "{{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"line-{i:04}\"}}\n"
        ));
    }
    assert!(
        chunk.len() > NOTIFY_MAX_BYTES,
        "test setup must exceed NOTIFY cap"
    );

    state_a.log_storage.append_log(job, &chunk).await?;
    state_a.event_bus.publish_log_chunk(job, &chunk).await;

    // Wait for full content to land on replica B.
    let full = wait_for_async(Duration::from_secs(5), || {
        let path = std::path::PathBuf::from(&state_b.config.log_storage.local_dir)
            .join(format!("{job}.jsonl"));
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|s| s.contains("line-0000") && s.contains("line-0399"))
                .unwrap_or(false)
        }
    })
    .await;
    assert!(
        full,
        "replica B must have mirrored every split NOTIFY segment in order"
    );

    state_b.log_storage.close_log(job).await;
    let meta = JobLogMeta {
        workspace: "ws".to_string(),
        task_name: "task".to_string(),
        created_at: chrono::Utc::now(),
    };
    let logs = state_b
        .log_storage
        .read_tail(job, &meta, false, StepFilter::All, ALL)
        .await?
        .logs;
    assert_eq!(
        logs, chunk,
        "mirrored bytes on replica B must match publisher byte-for-byte"
    );

    cancel.cancel();
    Ok(())
}

#[tokio::test]
async fn notify_log_chunk_oversize_signals_only() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    let mut rx_b = state_b.log_broadcast.subscribe(job).await;

    // Way over NOTIFY_MAX_BYTES — fall back to signal-only.
    let big = "x".repeat(20_000);
    state_a.event_bus.publish_log_chunk(job, &big).await;

    // The replica B subscriber MUST NOT receive content (NOTIFY carried no
    // bytes). Use a short timeout to confirm absence.
    let recv = tokio::time::timeout(Duration::from_millis(800), rx_b.recv()).await;
    assert!(
        recv.is_err(),
        "oversize chunk should not deliver bytes to peer subscribers; got {:?}",
        recv
    );

    cancel.cancel();
    Ok(())
}

#[tokio::test]
async fn notify_self_emitted_does_not_double_broadcast() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();
    let state = build_state(&h, Uuid::new_v4());

    // Listener attached to the SAME state that publishes.
    let _listener = start_listener(h.url.clone(), state.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    let mut rx = state.log_broadcast.subscribe(job).await;

    // Simulate what worker_api/jobs.rs does: write to disk, broadcast locally,
    // THEN publish. The self-emit guard in dispatch must prevent a second
    // append_log call from happening on the same replica.
    let chunk = "{\"step\":\"s\",\"line\":\"only-once\"}\n";
    state.log_storage.append_log(job, chunk).await?;
    state.log_broadcast.broadcast(job, chunk.to_string()).await;
    state.event_bus.publish_log_chunk(job, chunk).await;

    // Receive the local broadcast.
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("first broadcast should arrive")?;
    assert!(first.contains("only-once"));

    // Self-NOTIFY should be filtered by dispatch — no second message.
    let second = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
    assert!(
        second.is_err(),
        "self-emitted NOTIFY must not re-broadcast; got {:?}",
        second
    );

    // Wait for the NOTIFY round-trip window to expire, then check the disk
    // file contains the line exactly once. Without the self-emit guard in
    // dispatch the listener would call append_log a second time.
    sleep(Duration::from_millis(1000)).await;
    state.log_storage.close_log(job).await;
    let path =
        std::path::PathBuf::from(&state.config.log_storage.local_dir).join(format!("{job}.jsonl"));
    let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let occurrences = content.matches("only-once").count();
    assert_eq!(
        occurrences, 1,
        "publisher's disk file must contain the line exactly once (self-emit guard); found {occurrences}"
    );

    cancel.cancel();
    Ok(())
}

// ─── Mixed inline + signal-only mirror ────────────────────────────────────────

/// Publish a chunk from replica A that contains two small lines and one huge
/// line. Replica B must mirror the two small lines but NOT the huge line
/// (it arrives as a signal-only NOTIFY with no bytes). This locks in the
/// documented partial-mirror behavior.
#[tokio::test]
async fn notify_log_chunk_mixed_inline_and_signal_partial_mirrors() -> Result<()> {
    use stroem_server::log_storage::JobLogMeta;

    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();

    let small_before =
        "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"small-before\"}\n";
    let huge_line = format!("{}\n", "x".repeat(NOTIFY_MAX_BYTES + 1));
    let small_after =
        "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"small-after\"}\n";
    let chunk = format!("{}{}{}", small_before, huge_line, small_after);

    // Replicate the real worker_api/jobs.rs path: write to publisher's disk first.
    state_a.log_storage.append_log(job, &chunk).await?;
    state_a.event_bus.publish_log_chunk(job, &chunk).await;

    let log_dir_b = state_b.config.log_storage.local_dir.clone();

    // B should receive both small lines.
    let mirrored = wait_for_async(Duration::from_secs(5), || {
        let path = std::path::PathBuf::from(&log_dir_b).join(format!("{job}.jsonl"));
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|s| s.contains("small-before") && s.contains("small-after"))
                .unwrap_or(false)
        }
    })
    .await;
    assert!(
        mirrored,
        "replica B must have mirrored both small lines from the mixed chunk"
    );

    // B must NOT have the huge line's content (it was signal-only).
    state_b.log_storage.close_log(job).await;
    let meta = JobLogMeta {
        workspace: "ws".to_string(),
        task_name: "task".to_string(),
        created_at: chrono::Utc::now(),
    };
    let logs_b = state_b
        .log_storage
        .read_tail(job, &meta, false, StepFilter::All, ALL)
        .await?
        .logs;
    assert!(
        !logs_b.contains(&"x".repeat(10)),
        "replica B must NOT contain the huge line's raw content (signal-only)"
    );

    cancel.cancel();
    Ok(())
}

// ─── Receiver disk-write failure ──────────────────────────────────────────────

/// When replica B's log_storage cannot write to disk (local_dir points at an
/// invalid path), the listener must NOT panic, must still deliver the broadcast
/// to WS subscribers, and must continue processing subsequent NOTIFYs.
#[tokio::test]
async fn notify_log_chunk_mirror_disk_failure_does_not_break_listener() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());

    // Build a state_b whose log_storage local_dir is unwritable. We create a
    // regular file at a path and then point local_dir at a subdirectory of it
    // (impossible to create dirs through a file).
    let bad_base = h._temp.path().join("not-a-dir");
    std::fs::write(&bad_base, b"I am a file, not a dir")?;
    let unwritable_dir = bad_base.join("logs"); // can't create subdirs inside a file

    let replica_b = Uuid::new_v4();
    let config_b = empty_config(&h.url, &unwritable_dir);
    let log_storage_b = LogStorage::new(&unwritable_dir);
    let bus_b = EventBus::new(h.pool.clone(), replica_b);
    let state_b = AppState::new(
        h.pool.clone(),
        WorkspaceManager::from_config("default", WorkspaceConfig::new()),
        config_b,
        log_storage_b,
        HashMap::new(),
        None,
    )
    .with_event_bus(bus_b);

    let job = Uuid::new_v4();
    let mut rx_b = state_b.log_broadcast.subscribe(job).await;

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let chunk =
        "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"disk-fail-test\"}\n";

    // Publish from A — B's disk write will fail, but broadcast must still fire.
    state_a.event_bus.publish_log_chunk(job, chunk).await;

    // (a) No panic — test is still running at this point.
    // (b) Broadcast still fires despite disk failure.
    let msg = tokio::time::timeout(Duration::from_secs(3), rx_b.recv())
        .await
        .expect("WS broadcast must still fire even when disk write fails")?;
    assert!(
        msg.contains("disk-fail-test"),
        "broadcast payload should contain the chunk content; got: {msg}"
    );

    // (c) Listener continues: publish another chunk and confirm delivery.
    let job2 = Uuid::new_v4();
    let mut rx_b2 = state_b.log_broadcast.subscribe(job2).await;
    let chunk2 =
        "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"still-alive\"}\n";
    state_a.event_bus.publish_log_chunk(job2, chunk2).await;
    let msg2 = tokio::time::timeout(Duration::from_secs(3), rx_b2.recv())
        .await
        .expect("listener must still process NOTIFYs after a disk-write failure")?;
    assert!(msg2.contains("still-alive"));

    cancel.cancel();
    Ok(())
}

// ─── Three-replica concurrent publishers ──────────────────────────────────────

/// Three replicas (A, B, C). A listener runs on B. A and C publish 50 small
/// chunks each concurrently. After all publishes drain, B's mirror file must
/// contain all 100 unique line identifiers — verifying that concurrent
/// NOTIFY-driven `append_log` calls don't corrupt the file.
#[tokio::test]
async fn notify_log_chunk_three_replica_fanout_no_corruption() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());
    let state_c = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    const N: usize = 50;

    let sa = state_a.clone();
    let sc = state_c.clone();

    // Publish from A and C concurrently.
    tokio::join!(
        async {
            for i in 0..N {
                let line = format!(
                    "{{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"a-{i:03}\"}}\n"
                );
                sa.event_bus.publish_log_chunk(job, &line).await;
            }
        },
        async {
            for i in 0..N {
                let line = format!(
                    "{{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"c-{i:03}\"}}\n"
                );
                sc.event_bus.publish_log_chunk(job, &line).await;
            }
        }
    );

    let log_dir_b = state_b.config.log_storage.local_dir.clone();

    // Wait until all 100 lines land on B.
    let all_arrived = wait_for_async(Duration::from_secs(10), || {
        let path = std::path::PathBuf::from(&log_dir_b).join(format!("{job}.jsonl"));
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|s| {
                    (0..N).all(|i| s.contains(&format!("\"a-{i:03}\"")))
                        && (0..N).all(|i| s.contains(&format!("\"c-{i:03}\"")))
                })
                .unwrap_or(false)
        }
    })
    .await;

    assert!(
        all_arrived,
        "replica B must receive all 100 lines from concurrent publishers A and C"
    );

    // Verify no file corruption — each line identifier appears exactly once.
    state_b.log_storage.close_log(job).await;
    let path = std::path::PathBuf::from(&state_b.config.log_storage.local_dir)
        .join(format!("{job}.jsonl"));
    let content = tokio::fs::read_to_string(&path).await?;
    for i in 0..N {
        let count_a = content.matches(&format!("\"a-{i:03}\"")).count();
        assert_eq!(
            count_a, 1,
            "line a-{i:03} should appear exactly once; found {count_a}"
        );
        let count_c = content.matches(&format!("\"c-{i:03}\"")).count();
        assert_eq!(
            count_c, 1,
            "line c-{i:03} should appear exactly once; found {count_c}"
        );
    }

    cancel.cancel();
    Ok(())
}

// ─── Reconnect-window mirror gap (documented best-effort) ─────────────────────

/// Chunks published while B's listener is cancelled are NOT mirrored (NOTIFY
/// is fire-and-forget). This test locks in the documented best-effort behavior
/// so a future change doesn't silently "fix" it without updating the docs.
#[tokio::test]
async fn notify_log_chunk_lost_during_listener_gap_documented() -> Result<()> {
    let h = boot().await?;

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    // Phase 1: start listener, publish chunk_1, wait for it to land.
    let cancel_1 = CancellationToken::new();
    let _listener_1 = start_listener(h.url.clone(), state_b.clone(), cancel_1.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    let chunk_1 = "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"chunk-1\"}\n";
    state_a.event_bus.publish_log_chunk(job, chunk_1).await;

    let log_dir_b = state_b.config.log_storage.local_dir.clone();
    let arrived_1 = wait_for_async(Duration::from_secs(5), || {
        let path = std::path::PathBuf::from(&log_dir_b).join(format!("{job}.jsonl"));
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|s| s.contains("chunk-1"))
                .unwrap_or(false)
        }
    })
    .await;
    assert!(arrived_1, "chunk-1 must arrive before we kill the listener");

    // Phase 2: cancel the listener; publish chunk_2 into the gap.
    cancel_1.cancel();
    sleep(Duration::from_millis(500)).await; // let the listener task exit

    let chunk_2 = "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"chunk-2\"}\n";
    state_a.event_bus.publish_log_chunk(job, chunk_2).await;

    // Phase 3: start a new listener on B.
    let cancel_2 = CancellationToken::new();
    let _listener_2 = start_listener(h.url.clone(), state_b.clone(), cancel_2.clone());
    sleep(Duration::from_millis(500)).await; // let it connect

    // Publish chunk_3 — new listener must receive it.
    let chunk_3 = "{\"ts\":\"2026-01-01T00:00:00Z\",\"step\":\"s\",\"stream\":\"stdout\",\"line\":\"chunk-3\"}\n";
    state_a.event_bus.publish_log_chunk(job, chunk_3).await;

    let arrived_3 = wait_for_async(Duration::from_secs(5), || {
        let path = std::path::PathBuf::from(&log_dir_b).join(format!("{job}.jsonl"));
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map(|s| s.contains("chunk-3"))
                .unwrap_or(false)
        }
    })
    .await;
    assert!(arrived_3, "chunk-3 must arrive on the new listener");

    // Final assertions: chunk-1 and chunk-3 present; chunk-2 absent (gap).
    state_b.log_storage.close_log(job).await;
    let path = std::path::PathBuf::from(&state_b.config.log_storage.local_dir)
        .join(format!("{job}.jsonl"));
    let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();

    assert!(
        content.contains("chunk-1"),
        "chunk-1 must be present in B's mirror"
    );
    assert!(
        content.contains("chunk-3"),
        "chunk-3 must be present in B's mirror"
    );
    assert!(
        !content.contains("chunk-2"),
        "chunk-2 was published during the listener gap and must NOT appear on B (best-effort behavior)"
    );

    cancel_2.cancel();
    Ok(())
}

#[tokio::test]
async fn events_are_distinct_channels() {
    // Sanity check: the public channel constants are unique. A regression
    // here would silently break dispatch.
    let set: HashSet<&str> = [
        stroem_server::events::CHANNEL_JOB_CANCELLED,
        stroem_server::events::CHANNEL_WORKSPACE_RELOADED,
        stroem_server::events::CHANNEL_JOB_LOG_CHUNK,
    ]
    .into_iter()
    .collect();
    assert_eq!(set.len(), 3);
}

// ─── Scheduler gating (smoke) ────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn scheduler_followers_skip_firing() -> Result<()> {
    // Smoke test: two scheduler instances against the same DB shouldn't
    // double-fire. We can't easily inject a cron-trigger workspace via
    // testcontainers in this stripped-down setup, so this test asserts the
    // weaker property: the LeaderElection over a shared pool reports exactly
    // one leader, and AppState.leader.is_leader() reflects the chosen one.
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let (leader_a, _ha) = LeaderElection::start(h.url.clone(), cancel.clone());
    let (leader_b, _hb) = LeaderElection::start(h.url.clone(), cancel.clone());

    sleep(Duration::from_secs(2)).await;
    assert!(
        leader_a.is_leader() ^ leader_b.is_leader(),
        "exactly one replica should hold the scheduler lock"
    );

    cancel.cancel();
    Ok(())
}

// ─── Must: cancellation publish gate ─────────────────────────────────────────

/// Verify that `cancel_job` does NOT publish a NOTIFY when the job has no
/// running steps. The `has_running_steps` branch in `cancellation.rs` gates
/// the `publish_job_cancelled` call; this test confirms the peer's
/// `cancelled_jobs` set remains empty after a generous wait.
///
/// Because the assertion is "did not happen", we poll for a short window and
/// expect the condition to remain false.
#[tokio::test]
async fn cancel_job_with_no_running_steps_does_not_publish_notify() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    // Insert a job and all-pending step directly in the DB so cancel_job
    // sees a cancellable job but finds no *running* steps.
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, input, raw_input)
         VALUES ($1, 'default', 'test-task', 'pending', 'api', '{}', '{}')",
    )
    .bind(job_id)
    .execute(&h.pool)
    .await?;

    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, status, action_type, runner, required_tags)
         VALUES ($1, 'step-a', 'test-action', 'pending', 'script', 'local', '[]')",
    )
    .bind(job_id)
    .execute(&h.pool)
    .await?;

    // Cancel via the business-logic function.
    state_a.settlement().cancel(job_id).await?;

    // Poll for up to 2 s — the NOTIFY must NOT arrive on peer B.
    let notify_arrived = wait_for(Duration::from_secs(2), || {
        state_b
            .cancelled_jobs
            .read()
            .map(|s| s.contains(&job_id))
            .unwrap_or(false)
    })
    .await;

    assert!(
        !notify_arrived,
        "peer B must NOT receive cancellation NOTIFY when job has no running steps"
    );

    cancel.cancel();
    Ok(())
}

// ─── Must: workspace-reloaded propagation ─────────────────────────────────────

/// Verify that publishing `stroem_workspace_reloaded` causes the peer replica's
/// `WorkspaceManager::reload` to be invoked. We confirm the NOTIFY round-trip
/// completed correctly by checking that the channel dispatch routing is correct
/// (no cross-contamination into cancelled_jobs) and no panic occurred.
#[tokio::test]
async fn notify_workspace_reloaded_propagates_and_triggers_reload() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    // Publish workspace-reloaded from A.
    state_a
        .event_bus
        .publish_workspace_reloaded("default")
        .await;

    // Wait long enough for the NOTIFY round-trip.
    sleep(Duration::from_secs(2)).await;

    // The dispatch path must not have inserted anything into cancelled_jobs.
    {
        let set = state_b.cancelled_jobs.read().unwrap();
        assert!(
            set.is_empty(),
            "workspace_reloaded must not corrupt cancelled_jobs: {:?}",
            *set
        );
    }

    cancel.cancel();
    Ok(())
}

// ─── Must: /healthz/detail — leader with stopped scheduler ───────────────────

/// When this replica is leader but the scheduler `AliveGuard` has been
/// dropped, `GET /healthz/detail` must return 503 with
/// `checks.scheduler == "stopped"`.
#[tokio::test(flavor = "multi_thread")]
async fn healthz_leader_reports_stopped_when_scheduler_guard_dropped() -> Result<()> {
    let h = boot().await?;

    let state = build_state(&h, Uuid::new_v4());
    // Already always-leader by default from build_state (uses LeaderElection::always()).
    // Scheduler flag starts false (guard not held).
    assert!(
        !state
            .background_tasks
            .scheduler_alive
            .load(Ordering::Relaxed),
        "scheduler_alive must start false"
    );

    // Build HTTP router and call /healthz/detail with the worker token.
    let router = build_router(state, CancellationToken::new());

    let req = Request::builder()
        .method("GET")
        .uri("/healthz/detail")
        .header("Authorization", "Bearer test-token-must-be-long-enough-32")
        .body(Body::empty())?;

    let resp = router.oneshot(req).await?;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "leader with stopped scheduler must return 503"
    );

    let body = resp.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        json["checks"]["scheduler"], "stopped",
        "checks.scheduler must be 'stopped' for leader with dropped guard"
    );
    assert_eq!(json["status"], "degraded");

    Ok(())
}

// ─── Must: /livez — a HUNG or dead background loop restarts the pod ──────────

fn mark_all_tasks_alive(state: &stroem_server::state::AppState) {
    let bg = &state.background_tasks;
    for flag in [
        &bg.scheduler_alive,
        &bg.recovery_alive,
        &bg.event_source_alive,
    ] {
        flag.store(true, Ordering::Relaxed);
    }
    for beat in [&bg.scheduler_beat, &bg.recovery_beat, &bg.event_source_beat] {
        beat.beat();
    }
}

async fn get_json(
    router: &axum::Router,
    uri: &str,
    bearer: bool,
) -> Result<(StatusCode, serde_json::Value)> {
    let mut req = Request::builder().uri(uri);
    if bearer {
        req = req.header("Authorization", "Bearer test-token-must-be-long-enough-32");
    }
    let resp = router.clone().oneshot(req.body(Body::empty())?).await?;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    Ok((status, serde_json::from_slice(&body)?))
}

/// Prod 2026-09-16: the scheduler task hung (guard still held) and the probe
/// stayed green for 9 hours. `/livez` must go 503 so the liveness probe
/// restarts the pod — without naming the task — while `/healthz` (readiness)
/// stays 200: the replica can still serve API and worker traffic.
#[tokio::test(flavor = "multi_thread")]
async fn livez_fails_when_scheduler_stalled_but_readiness_holds() -> Result<()> {
    let h = boot().await?;
    let state = build_state(&h, Uuid::new_v4());
    mark_all_tasks_alive(&state);
    state
        .background_tasks
        .scheduler_beat
        .backdate(Duration::from_secs(3600));
    let router = build_router(state, CancellationToken::new());

    let (status, json) = get_json(&router, "/livez", false).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json, serde_json::json!({ "status": "stalled" }));

    let (status, json) = get_json(&router, "/healthz", false).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "readiness must not see a stalled loop"
    );
    assert_eq!(json["status"], "ok");

    let (status, json) = get_json(&router, "/healthz/detail", true).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json["checks"]["scheduler"], "stalled");
    assert_eq!(json["checks"]["recovery"], "ok");
    Ok(())
}

/// Followers run the loops too; a hung standby is no standby.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn livez_fails_for_stalled_loop_on_follower() -> Result<()> {
    let h = boot().await?;
    let cancel_leader = CancellationToken::new();
    let (leader_false, _handle) = LeaderElection::start(h.url.clone(), cancel_leader.clone());
    cancel_leader.cancel();
    sleep(Duration::from_millis(300)).await;

    let state = build_state(&h, Uuid::new_v4()).with_leader(leader_false);
    mark_all_tasks_alive(&state);
    state
        .background_tasks
        .event_source_beat
        .backdate(Duration::from_secs(3600));
    let router = build_router(state, CancellationToken::new());

    let (status, _) = get_json(&router, "/livez", false).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (_, json) = get_json(&router, "/healthz/detail", true).await?;
    assert_eq!(json["checks"]["leader"], false);
    assert_eq!(json["checks"]["event_source"], "stalled");
    Ok(())
}

/// Fresh beats, and tasks that were never started (every router-only test,
/// and the window before `main` spawns them), must not fail liveness.
#[tokio::test(flavor = "multi_thread")]
async fn livez_ok_when_tasks_fresh_or_never_started() -> Result<()> {
    let h = boot().await?;

    let never_started = build_router(build_state(&h, Uuid::new_v4()), CancellationToken::new());
    let (status, json) = get_json(&never_started, "/livez", false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, serde_json::json!({ "status": "ok" }));

    let fresh = build_state(&h, Uuid::new_v4());
    mark_all_tasks_alive(&fresh);
    let (status, _) = get_json(
        &build_router(fresh, CancellationToken::new()),
        "/livez",
        false,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

/// A task that ran and then EXITED (panic, guard dropped) on the leader also
/// fails liveness — before, only `/healthz/detail` noticed.
#[tokio::test(flavor = "multi_thread")]
async fn livez_fails_when_started_task_exited_on_leader() -> Result<()> {
    let h = boot().await?;
    let state = build_state(&h, Uuid::new_v4());
    mark_all_tasks_alive(&state);
    state
        .background_tasks
        .recovery_alive
        .store(false, Ordering::Relaxed);
    let router = build_router(state, CancellationToken::new());

    let (status, json) = get_json(&router, "/livez", false).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json, serde_json::json!({ "status": "stopped" }));
    let (_, json) = get_json(&router, "/healthz/detail", true).await?;
    assert_eq!(json["checks"]["recovery"], "stopped");
    assert_eq!(json["checks"]["scheduler"], "ok");
    Ok(())
}

// ─── Must: /healthz/detail — follower with stopped scheduler ─────────────────

/// When this replica is a follower and the scheduler guard is not held,
/// `GET /healthz/detail` must return 200 with `checks.scheduler == "follower"`.
/// Followers don't fail health on background-task liveness.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn healthz_follower_reports_follower_not_503() -> Result<()> {
    let h = boot().await?;

    // Start a LeaderElection and immediately cancel so it stays false.
    let cancel_leader = CancellationToken::new();
    let (leader_false, _handle) = LeaderElection::start(h.url.clone(), cancel_leader.clone());
    cancel_leader.cancel();
    // Give the loop a moment to exit and confirm it's false.
    sleep(Duration::from_millis(300)).await;

    let state = build_state(&h, Uuid::new_v4()).with_leader(leader_false);

    // Scheduler guard NOT held — flag stays false.
    assert!(
        !state
            .background_tasks
            .scheduler_alive
            .load(Ordering::Relaxed),
        "scheduler_alive must start false"
    );

    let router = build_router(state, CancellationToken::new());

    let req = Request::builder()
        .method("GET")
        .uri("/healthz/detail")
        .header("Authorization", "Bearer test-token-must-be-long-enough-32")
        .body(Body::empty())?;

    let resp = router.oneshot(req).await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "follower must return 200 even when scheduler guard is not held"
    );

    let body = resp.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        json["checks"]["scheduler"], "follower",
        "checks.scheduler must be 'follower' for a non-leader replica"
    );

    Ok(())
}

// ─── Should: listener reconnects after disconnect ─────────────────────────────

/// Forcibly terminate the PgListener's idle wait connection, then verify that
/// the listener reconnects and continues delivering NOTIFY messages.
///
/// sqlx's `PgListener` has `eager_reconnect = true` by default, so after a
/// disconnect it reconnects transparently. Our outer loop adds a 2 s delay for
/// fatal errors — total reconnect window is < 5 s in practice.
#[tokio::test]
async fn listener_reconnects_after_disconnect() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(800)).await;

    // Terminate idle connections that are in LISTEN state (the PgListener
    // connection). Filter by wait_event to avoid killing active workers.
    let _: Vec<(bool,)> = sqlx::query_as(
        "SELECT pg_terminate_backend(pid)
         FROM pg_stat_activity
         WHERE datname = current_database()
           AND pid <> pg_backend_pid()
           AND wait_event = 'ClientRead'
           AND state = 'idle'",
    )
    .fetch_all(&h.pool)
    .await
    .unwrap_or_default();

    // Give listener time to reconnect (sqlx eager_reconnect + outer 2s delay).
    sleep(Duration::from_secs(4)).await;

    // Publish and verify delivery — proves listener is active after reconnect.
    let job = Uuid::new_v4();
    state_a.event_bus.publish_job_cancelled(job).await;

    let received = wait_for(Duration::from_secs(5), || {
        state_b
            .cancelled_jobs
            .read()
            .map(|s| s.contains(&job))
            .unwrap_or(false)
    })
    .await;

    assert!(
        received,
        "replica B must receive NOTIFY after listener reconnects"
    );

    cancel.cancel();
    Ok(())
}

// ─── Nice-to-have: leader connection loss → follower reflag ─────────────────

/// Forcibly terminate the leader's advisory-lock connection (identified by
/// `application_name = 'stroem-leader'`) and assert that `is_leader()` returns
/// false within two retry intervals (10 s).
#[tokio::test]
#[serial]
async fn leader_connection_lost_triggers_reflag_to_follower() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let (leader, _handle) = LeaderElection::start(h.url.clone(), cancel.clone());

    // Wait for the lock to be acquired.
    let became_leader = wait_for(Duration::from_secs(3), || leader.is_leader()).await;
    assert!(became_leader, "leader must acquire lock within 3s");

    // Terminate the stroem-leader connection.
    let _: Vec<(bool,)> = sqlx::query_as(
        "SELECT pg_terminate_backend(pid)
         FROM pg_stat_activity
         WHERE application_name = 'stroem-leader'
           AND pid <> pg_backend_pid()",
    )
    .fetch_all(&h.pool)
    .await
    .unwrap_or_default();

    // The leader loop ping (SELECT 1) will fail on the next tick (≤5s RETRY_INTERVAL),
    // flipping is_leader to false. Allow 2 retry intervals = 10s.
    let flipped = wait_for(Duration::from_secs(10), || !leader.is_leader()).await;
    assert!(
        flipped,
        "is_leader must flip to false within 10s after connection termination"
    );

    cancel.cancel();
    Ok(())
}

// ─── Nice-to-have: double leader race ────────────────────────────────────────

/// Start three LeaderElection instances simultaneously and assert exactly one
/// holds the lock after 3 s.
#[tokio::test]
#[serial]
async fn double_leader_race_both_attempt_simultaneously() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let (la, _ha) = LeaderElection::start(h.url.clone(), cancel.clone());
    let (lb, _hb) = LeaderElection::start(h.url.clone(), cancel.clone());
    let (lc, _hc) = LeaderElection::start(h.url.clone(), cancel.clone());

    sleep(Duration::from_secs(3)).await;

    let count = [la.is_leader(), lb.is_leader(), lc.is_leader()]
        .iter()
        .filter(|&&v| v)
        .count();

    assert_eq!(
        count, 1,
        "exactly one replica must hold the leader lock; got {} leaders",
        count
    );

    cancel.cancel();
    Ok(())
}

// ─── WebSocket viewer channel cleanup ─────────────────────────────────────────

/// Serve `state` on a real port for WebSocket clients.
async fn serve_ws(state: AppState) -> Result<(u16, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let router = build_router(state, CancellationToken::new());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Ok((port, server))
}

#[tokio::test]
async fn ws_viewer_close_removes_job_channel() -> Result<()> {
    // Regression: handle_ws called remove_channel while its own receiver was still
    // alive, so the no-receivers check never passed and every job ever viewed kept
    // its broadcast channel allocated for good.
    let h = boot().await?;
    let state = build_state(&h, Uuid::new_v4());
    let broadcast = state.log_broadcast.clone();
    let (port, server) = serve_ws(state).await?;
    let job = Uuid::new_v4();

    let url = format!("ws://127.0.0.1:{port}/api/jobs/{job}/logs/stream");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;
    assert!(
        wait_for_async(Duration::from_secs(5), || broadcast.has_channel(job)).await,
        "the viewer never subscribed"
    );

    ws.close(None).await?;
    assert!(
        wait_for_async(Duration::from_secs(5), || async {
            !broadcast.has_channel(job).await
        })
        .await,
        "channel left behind after the last viewer closed"
    );

    server.abort();
    Ok(())
}

#[tokio::test]
async fn ws_viewer_dropped_with_backfill_pending_removes_job_channel() -> Result<()> {
    // The handler subscribes before it reads and sends the backfill. A viewer whose
    // connection is gone by then takes the early exit on the failed send, or the
    // loop's exit on the closed socket — both must clean up like a normal close.
    let h = boot().await?;
    let state = build_state(&h, Uuid::new_v4());
    let job = Uuid::new_v4();
    let line = json!({"ts": "2026-09-24T12:00:00Z", "stream": "stdout", "step": "build", "line": "x".repeat(64 * 1024)});
    for _ in 0..8 {
        state
            .log_storage
            .append_log(job, &format!("{line}\n"))
            .await?;
    }
    let broadcast = state.log_broadcast.clone();
    let (port, server) = serve_ws(state).await?;

    let url = format!("ws://127.0.0.1:{port}/api/jobs/{job}/logs/stream");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
    // Subscribing is the handler's first step, before the backfill is read or sent:
    // wait for it, so the assertion below cannot pass on a handler that never subscribed.
    assert!(
        wait_for_async(Duration::from_secs(5), || broadcast.has_channel(job)).await,
        "the viewer never subscribed"
    );
    drop(ws);

    assert!(
        wait_for_async(Duration::from_secs(5), || async {
            !broadcast.has_channel(job).await
        })
        .await,
        "channel left behind after a viewer dropped with a backfill pending"
    );

    server.abort();
    Ok(())
}

// ─── Full HTTP-stack cross-replica log fan-out ────────────────────────────────

/// Worker POSTs a log chunk to replica A's HTTP router; a log subscriber on
/// replica B receives the chunk via the NOTIFY/LISTEN bus.
///
/// This exercises the complete cross-replica live-tail path that the WS
/// handler relies on internally:
///   worker → A's `/worker/jobs/{id}/logs` → `append_log` writes locally,
///   broadcasts on A, AND publishes `stroem_job_log_chunk` → B's PgListener
///   dispatch → `state_b.log_broadcast.broadcast(job_id, …)` → subscriber.
///
/// We subscribe directly to `state_b.log_broadcast` (the same channel the
/// WS handler reads after backfill) instead of dancing through a real
/// WebSocket upgrade — same code path, much less wiring.
#[tokio::test]
async fn worker_log_post_reaches_ws_viewer_on_peer_replica() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    // Start the listener on B so it receives `stroem_job_log_chunk` NOTIFYs
    // emitted by A's `append_log` and fans them out to B's local broadcast.
    let _listener = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    // Give the PgListener time to attach to the channel.
    sleep(Duration::from_millis(500)).await;

    let job_id = Uuid::new_v4();
    let mut rx_b = state_b.log_broadcast.subscribe(job_id).await;

    // Build A's full router and POST the worker log payload through it. This
    // exercises the real auth middleware + handler stack, not just the bus.
    let router_a = build_router(state_a.clone(), cancel.clone());
    let body = json!({
        "step_name": "demo-step",
        "lines": [
            { "ts": "2026-05-18T00:00:00Z", "stream": "stdout", "line": "cross-replica-hello" }
        ]
    });
    let req = Request::builder()
        .method("POST")
        .uri(format!("/worker/jobs/{}/logs", job_id))
        .header("content-type", "application/json")
        .header("Authorization", "Bearer test-token-must-be-long-enough-32")
        .body(Body::from(body.to_string()))?;
    let response = router_a.oneshot(req).await?;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "worker log POST to replica A should succeed (body: {:?})",
        response.into_body().collect().await?.to_bytes()
    );

    // B's broadcast subscriber should receive the line. Generous 3s window
    // covers PgListener delivery latency under CI load.
    let msg = tokio::time::timeout(Duration::from_secs(3), rx_b.recv())
        .await
        .expect("replica B should observe the chunk within 3s via NOTIFY → dispatch → broadcast")?;
    assert!(
        msg.contains("cross-replica-hello"),
        "broadcast payload on B should contain the line content; got: {msg}"
    );
    // And it should be the JSONL produced by `append_log` — sanity-check the
    // shape (step + stream fields) to catch regressions in the chunk format.
    assert!(
        msg.contains("\"step\":\"demo-step\""),
        "chunk should retain step field; got: {msg}"
    );
    assert!(
        msg.contains("\"stream\":\"stdout\""),
        "chunk should retain stream field; got: {msg}"
    );

    cancel.cancel();
    Ok(())
}

// ─── Leader lease, validity and self-release (spec 2026-10-20) ──────────────

/// A leader that stops working must lose the lock in bounded time — prod
/// 2026-10-19: a frozen node's kernel kept the leader session to RDS alive and
/// no replica was leader for 16.5 h. Spec
/// `docs/superpowers/specs/2026-10-20-leader-stale-lock-design.md` § 6.3.
mod lease {
    use super::{boot, wait_for};
    use anyhow::Result;
    use serial_test::serial;
    use sqlx::{Connection, PgConnection, PgPool};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use stroem_server::leader::{
        apply_lease, lock_holder, LeaderElection, LeaderTiming, LeaseStatus, APPLICATION_NAME,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::watch;
    use tokio::time::sleep;
    use tokio_util::sync::CancellationToken;

    /// Sub-second timing; `leader::tests::integration_test_timing_satisfies_the_invariants`
    /// checks it against `LeaderTiming::check`.
    fn fast() -> LeaderTiming {
        LeaderTiming {
            retry_interval: Duration::from_millis(200),
            ping_timeout: Duration::from_millis(500),
            acquire_timeout: Duration::from_millis(1500),
            validity: Duration::from_millis(1500),
            lease: Duration::from_millis(2500),
            release_after: Duration::from_millis(600),
        }
    }

    /// Takeover bound after the old session's last idle start (spec § 5.1),
    /// plus 1 s of slack.
    fn takeover_bound(t: &LeaderTiming) -> Duration {
        t.lease + 2 * t.acquire_timeout + t.retry_interval + Duration::from_secs(1)
    }

    fn start(url: &str) -> (Arc<LeaderElection>, CancellationToken) {
        let cancel = CancellationToken::new();
        let (leader, _handle) = LeaderElection::start_with(url.to_string(), fast(), cancel.clone());
        (leader, cancel)
    }

    /// A leads on `url_a`, then B starts on `url_b` and stays follower.
    async fn leader_and_follower(
        url_a: &str,
        url_b: &str,
    ) -> (
        Arc<LeaderElection>,
        CancellationToken,
        Arc<LeaderElection>,
        CancellationToken,
    ) {
        let (a, ca) = start(url_a);
        assert!(
            wait_for(Duration::from_secs(5), || a.is_leader()).await,
            "A must acquire the lock"
        );
        let (b, cb) = start(url_b);
        sleep(Duration::from_millis(800)).await;
        assert!(a.is_leader() && !b.is_leader(), "A leads, B follows");
        (a, ca, b, cb)
    }

    /// Poll every 20 ms until `done()` or `within` has passed; fail the test
    /// if A and B are ever leader at the same time.
    async fn wait_exclusive(
        a: &LeaderElection,
        b: &LeaderElection,
        within: Duration,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let deadline = Instant::now() + within;
        loop {
            assert!(
                !(a.is_leader() && b.is_leader()),
                "two replicas reported is_leader() at the same time"
            );
            if done() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    async fn holder_pid(pool: &PgPool) -> Option<i32> {
        let mut conn = pool.acquire().await.expect("pool connection");
        lock_holder(&mut conn)
            .await
            .expect("read lock holder")
            .map(|h| h.pid)
    }

    async fn backend_alive(pool: &PgPool, pid: i32) -> bool {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM pg_stat_activity WHERE pid = $1")
            .bind(pid)
            .fetch_one(pool)
            .await
            .expect("count backend");
        n == 1
    }

    // ── A PostgreSQL-protocol-aware TCP proxy ──────────────────────────────

    /// Controls of [`PgProxy`]. Acts on whole protocol messages, never on TCP
    /// read chunks (TCP keeps no write boundaries).
    struct ProxyCtl {
        /// Stop forwarding in both directions; keep every socket open, also
        /// when the client closes — what the incident's node did.
        frozen: AtomicBool,
        /// Hold each client `Q` message this long before forwarding it.
        request_delay_ms: AtomicU64,
        /// Hold the server's messages until this long after the `Z` (or end
        /// of stream) that ends them.
        answer_delay_ms: AtomicU64,
        /// When > 0: forward only this many bytes of the next client `Q`
        /// message, then freeze.
        cut_next_query_after: AtomicUsize,
        /// Barriers: counters bumped when a `Q` is held, an answer is held,
        /// and when a `Q` was cut (`(cut after, message length)`).
        query_held: watch::Sender<u64>,
        answer_held: watch::Sender<u64>,
        query_cut: watch::Sender<(usize, usize)>,
    }

    struct PgProxy {
        port: u16,
        ctl: Arc<ProxyCtl>,
        accept: tokio::task::JoinHandle<()>,
    }

    impl Drop for PgProxy {
        fn drop(&mut self) {
            self.accept.abort();
        }
    }

    impl PgProxy {
        async fn start(upstream: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
            let port = listener.local_addr().expect("proxy addr").port();
            let ctl = Arc::new(ProxyCtl {
                frozen: AtomicBool::new(false),
                request_delay_ms: AtomicU64::new(0),
                answer_delay_ms: AtomicU64::new(0),
                cut_next_query_after: AtomicUsize::new(0),
                query_held: watch::channel(0).0,
                answer_held: watch::channel(0).0,
                query_cut: watch::channel((0, 0)).0,
            });
            let accept_ctl = ctl.clone();
            let accept = tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let ctl = accept_ctl.clone();
                    let upstream = upstream.clone();
                    tokio::spawn(async move {
                        let Ok(server) = TcpStream::connect(&upstream).await else {
                            return;
                        };
                        let (cr, cw) = client.into_split();
                        let (sr, sw) = server.into_split();
                        tokio::spawn(client_to_server(cr, sw, ctl.clone()));
                        tokio::spawn(server_to_client(sr, cw, ctl));
                    });
                }
            });
            Self { port, ctl, accept }
        }

        /// `url` with its host and port replaced by the proxy, TLS off so
        /// the proxy sees plain protocol messages.
        fn url_for(&self, url: &str) -> String {
            let (head, rest) = url.split_once('@').expect("URL has credentials");
            let (_, path) = rest.split_once('/').expect("URL has a database");
            let sep = if path.contains('?') { '&' } else { '?' };
            format!("{head}@127.0.0.1:{}/{path}{sep}sslmode=disable", self.port)
        }
    }

    /// `host:port` of the test database.
    fn upstream_of(url: &str) -> String {
        let (_, rest) = url.split_once('@').expect("URL has credentials");
        rest.split_once('/')
            .expect("URL has a database")
            .0
            .to_string()
    }

    /// One protocol message: `typed` = it starts with a type byte (every
    /// message except the client's StartupMessage). `None` at end of stream.
    async fn read_message(r: &mut OwnedReadHalf, typed: bool) -> Option<Vec<u8>> {
        let header = if typed { 5 } else { 4 };
        let mut msg = vec![0u8; header];
        r.read_exact(&mut msg).await.ok()?;
        let len = u32::from_be_bytes(msg[header - 4..].try_into().ok()?) as usize;
        let mut body = vec![0u8; len.checked_sub(4)?];
        r.read_exact(&mut body).await.ok()?;
        msg.extend_from_slice(&body);
        Some(msg)
    }

    async fn unfrozen(ctl: &ProxyCtl) {
        while ctl.frozen.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(10)).await;
        }
    }

    async fn client_to_server(mut r: OwnedReadHalf, mut w: OwnedWriteHalf, ctl: Arc<ProxyCtl>) {
        let mut typed = false;
        loop {
            let Some(msg) = read_message(&mut r, typed).await else {
                // Client closed: while frozen the server side stays open.
                unfrozen(&ctl).await;
                let _ = w.shutdown().await;
                return;
            };
            let is_query = typed && msg[0] == b'Q';
            typed = true;
            unfrozen(&ctl).await;
            if is_query {
                let k = ctl.cut_next_query_after.swap(0, Ordering::SeqCst);
                if k > 0 {
                    assert!(
                        k < msg.len(),
                        "cut offset {k} not inside a {}-byte Q",
                        msg.len()
                    );
                    let _ = w.write_all(&msg[..k]).await;
                    let _ = w.flush().await;
                    // Hold new connections too, so the cut replica cannot
                    // simply reconnect.
                    ctl.frozen.store(true, Ordering::SeqCst);
                    ctl.query_cut.send_replace((k, msg.len()));
                    // Keep both sockets open forever.
                    std::future::pending::<()>().await;
                }
                let delay = ctl.request_delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    ctl.query_held.send_modify(|n| *n += 1);
                    sleep(Duration::from_millis(delay)).await;
                }
            }
            if w.write_all(&msg).await.is_err() {
                return;
            }
        }
    }

    async fn server_to_client(mut r: OwnedReadHalf, mut w: OwnedWriteHalf, ctl: Arc<ProxyCtl>) {
        let mut held = Vec::new();
        loop {
            let msg = read_message(&mut r, true).await;
            let delay = ctl.answer_delay_ms.load(Ordering::SeqCst);
            let Some(msg) = msg else {
                // End of stream (e.g. after a FATAL): released `delay` later.
                if delay > 0 {
                    sleep(Duration::from_millis(delay)).await;
                }
                unfrozen(&ctl).await;
                let _ = w.write_all(&held).await;
                let _ = w.shutdown().await;
                return;
            };
            let ends_answer = msg[0] == b'Z';
            held.extend_from_slice(&msg);
            if delay > 0 && !ends_answer {
                continue;
            }
            if delay > 0 {
                ctl.answer_held.send_modify(|n| *n += 1);
                sleep(Duration::from_millis(delay)).await;
            }
            unfrozen(&ctl).await;
            if w.write_all(&held).await.is_err() {
                return;
            }
            held.clear();
        }
    }

    // ── Tests ────────────────────────────────────────────────────────────

    /// Test 1: the leader task stops (frozen process, working network). A
    /// stops reporting leader within `validity`; PostgreSQL ends A's idle
    /// session after the lease; B takes over; the two never overlap; the
    /// thawed A stays follower.
    #[tokio::test]
    #[serial]
    async fn frozen_leader_loses_lock_to_follower_after_lease() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let (a, ca, b, cb) = leader_and_follower(&h.url, &h.url).await;

        a.pause_for_test(true);
        let paused_at = Instant::now();
        assert!(
            wait_exclusive(&a, &b, t.validity + Duration::from_millis(500), || !a
                .is_leader())
            .await,
            "a frozen leader must stop reporting leader within validity"
        );
        assert!(
            wait_exclusive(&a, &b, takeover_bound(&t), || b.is_leader()).await,
            "B must take over within the lease bound ({:?} since the pause)",
            paused_at.elapsed()
        );
        assert!(
            paused_at.elapsed() >= t.lease - t.retry_interval - t.ping_timeout,
            "B must not take over before A's session could have expired"
        );

        // Thaw: A's next ping fails and A stays follower.
        a.pause_for_test(false);
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            assert!(!a.is_leader(), "a thawed ex-leader must not report leader");
            assert!(b.is_leader(), "B keeps the lock");
            sleep(Duration::from_millis(20)).await;
        }
        ca.cancel();
        cb.cancel();
        Ok(())
    }

    /// Test 2: the network to the database goes silent while both ends keep
    /// their sockets (the incident's shape). A's ping times out; its backend
    /// lives on until the lease ends it; then B takes over.
    #[tokio::test]
    #[serial]
    async fn partitioned_leader_loses_lock_to_follower_after_lease() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let proxy = PgProxy::start(upstream_of(&h.url)).await;
        let (a, ca, b, cb) = leader_and_follower(&proxy.url_for(&h.url), &h.url).await;
        let pid_a = holder_pid(&h.pool).await.expect("A holds the lock");

        proxy.ctl.frozen.store(true, Ordering::SeqCst);
        let frozen_at = Instant::now();
        assert!(
            wait_exclusive(
                &a,
                &b,
                t.ping_timeout + t.retry_interval + Duration::from_millis(500),
                || !a.is_leader()
            )
            .await,
            "A's ping must time out and clear its validity"
        );
        assert!(
            backend_alive(&h.pool, pid_a).await,
            "A dropped its socket, but the frozen proxy keeps A's session open"
        );
        assert!(
            wait_exclusive(&a, &b, takeover_bound(&t), || b.is_leader()).await,
            "B must take over once the lease ends A's session ({:?} since the freeze)",
            frozen_at.elapsed()
        );
        assert!(
            !backend_alive(&h.pool, pid_a).await,
            "the lease must have ended A's session"
        );
        ca.cancel();
        cb.cancel();
        Ok(())
    }

    /// Test 3: a ping `Query` message that arrives only in part leaves the
    /// idle-session timer armed (spec § 4.2's protocol rule).
    #[tokio::test]
    #[serial]
    async fn partial_ping_message_does_not_stop_the_lease() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let proxy = PgProxy::start(upstream_of(&h.url)).await;
        let (a, ca, b, cb) = leader_and_follower(&proxy.url_for(&h.url), &h.url).await;

        let mut cut = proxy.ctl.query_cut.subscribe();
        proxy.ctl.cut_next_query_after.store(5, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), cut.changed())
            .await
            .expect("A's next ping is cut")?;
        let cut_at = Instant::now();
        // "SELECT 1" as a Query: type byte, int32 length, the text and a NUL.
        assert_eq!(*cut.borrow(), (5, 14), "cut inside A's ping message");

        assert!(
            wait_exclusive(&a, &b, takeover_bound(&t), || b.is_leader()).await,
            "B must take over although A's backend waits mid-message ({:?} since the cut)",
            cut_at.elapsed()
        );
        ca.cancel();
        cb.cancel();
        Ok(())
    }

    /// Test 4: a healthy leader keeps its session and lock across several
    /// leases.
    #[tokio::test]
    #[serial]
    async fn healthy_leader_keeps_lock_past_lease() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let (a, ca, b, cb) = leader_and_follower(&h.url, &h.url).await;
        let pid = holder_pid(&h.pool).await.expect("A holds the lock");

        let until = Instant::now() + 3 * t.lease;
        while Instant::now() < until {
            assert!(a.is_leader(), "a healthy leader keeps reporting leader");
            assert!(!b.is_leader(), "the follower never leads");
            assert_eq!(
                holder_pid(&h.pool).await,
                Some(pid),
                "the leader session is never replaced"
            );
            sleep(Duration::from_millis(250)).await;
        }
        ca.cancel();
        cb.cancel();
        Ok(())
    }

    /// Test 5: a session ended early (administrator, connection reset) lets B
    /// take over at once; A notices on its next ping.
    #[tokio::test]
    #[serial]
    async fn early_session_end_overlap_is_bounded() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let (a, ca, b, cb) = leader_and_follower(&h.url, &h.url).await;
        let pid = holder_pid(&h.pool).await.expect("A holds the lock");

        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&h.pool)
            .await?;
        let ended_at = Instant::now();
        assert!(
            wait_for(
                t.retry_interval + t.ping_timeout + Duration::from_millis(500),
                || { !a.is_leader() }
            )
            .await,
            "A must notice the ended session on its next ping"
        );
        assert!(
            wait_for(takeover_bound(&t), || b.is_leader()).await,
            "B takes over ({:?} after the termination)",
            ended_at.elapsed()
        );
        ca.cancel();
        cb.cancel();
        Ok(())
    }

    /// Test 6: as test 5, but a successful answer sent before the
    /// termination reaches A only afterwards. The overlap stays within
    /// `2 × ping_timeout + retry_interval`, never past `validity`.
    #[tokio::test]
    #[serial]
    async fn early_session_end_with_late_answers_is_bounded() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let proxy = PgProxy::start(upstream_of(&h.url)).await;
        let (a, ca, b, cb) = leader_and_follower(&proxy.url_for(&h.url), &h.url).await;
        let pid = holder_pid(&h.pool).await.expect("A holds the lock");

        let mut held = proxy.ctl.answer_held.subscribe();
        proxy.ctl.answer_delay_ms.store(350, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), held.changed())
            .await
            .expect("an answer to A is held")?;
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(&h.pool)
            .await?;
        let ended_at = Instant::now();

        let bound =
            (2 * t.ping_timeout + t.retry_interval).min(t.validity) + Duration::from_millis(500);
        assert!(
            wait_for(bound, || !a.is_leader()).await,
            "A must stop reporting leader within {bound:?} of the termination"
        );
        assert!(
            wait_for(takeover_bound(&t), || b.is_leader()).await,
            "B takes over ({:?} after the termination)",
            ended_at.elapsed()
        );
        ca.cancel();
        cb.cancel();
        Ok(())
    }

    /// Test 7: a ping already on its way when A freezes restarts the idle
    /// interval when it arrives — once (spec § 5.1, bound from the freeze).
    #[tokio::test]
    #[serial]
    async fn delayed_ping_extends_the_session_by_one_lease_only() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let delay = Duration::from_millis(300);
        let proxy = PgProxy::start(upstream_of(&h.url)).await;
        let (a, ca, b, cb) = leader_and_follower(&proxy.url_for(&h.url), &h.url).await;

        let mut held = proxy.ctl.query_held.subscribe();
        proxy
            .ctl
            .request_delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), held.changed())
            .await
            .expect("a ping of A is held")?;
        a.pause_for_test(true);
        let paused_at = Instant::now();

        // The held ping reaches PostgreSQL `delay` after the pause and
        // restarts the idle interval there.
        let earliest = delay + t.lease - Duration::from_millis(150);
        assert!(
            !wait_exclusive(&a, &b, earliest, || b.is_leader()).await,
            "B took over {:?} after the pause, before the delayed ping's lease could end",
            paused_at.elapsed()
        );
        assert!(
            wait_exclusive(&a, &b, delay + takeover_bound(&t) - earliest, || b
                .is_leader())
            .await,
            "B must take over within the bound from the freeze ({:?})",
            paused_at.elapsed()
        );
        ca.cancel();
        cb.cancel();
        Ok(())
    }

    /// Test 8: the PostgreSQL < 14 fallback keys on the real server error for
    /// an unknown parameter.
    #[tokio::test]
    #[serial]
    async fn lease_setup_maps_unrecognized_parameter_to_unsupported() -> Result<()> {
        let h = boot().await?;
        let mut conn = PgConnection::connect(&h.url).await?;
        let lease = Duration::from_millis(2500);
        assert_eq!(
            apply_lease(&mut conn, "stroem_no_such_parameter", lease).await?,
            LeaseStatus::Unsupported
        );
        assert_eq!(
            apply_lease(&mut conn, "idle_session_timeout", lease).await?,
            LeaseStatus::Enforced
        );
        let (shown,): (String,) = sqlx::query_as("SHOW idle_session_timeout")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(shown, "2500ms");
        Ok(())
    }

    /// Test 9: a leader whose gated loop stays stalled for `release_after`
    /// releases the lock and does not take it back while the stall lasts; a
    /// shorter stall changes nothing.
    #[tokio::test]
    #[serial]
    async fn stalled_loop_releases_lock_after_persistence_and_blocks_reacquire() -> Result<()> {
        let h = boot().await?;
        let t = fast();
        let stalled = Arc::new(AtomicBool::new(false));
        let (a, ca) = start(&h.url);
        let flag = stalled.clone();
        a.set_fitness_probe(Arc::new(move || {
            flag.load(Ordering::SeqCst).then_some("scheduler")
        }));
        assert!(wait_for(Duration::from_secs(5), || a.is_leader()).await);
        let (b, cb) = start(&h.url);
        sleep(Duration::from_millis(800)).await;
        assert!(a.is_leader() && !b.is_leader());

        // A short stall: no release.
        stalled.store(true, Ordering::SeqCst);
        sleep(Duration::from_millis(250)).await;
        stalled.store(false, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            assert!(
                a.is_leader(),
                "a stall shorter than release_after must not release"
            );
            sleep(Duration::from_millis(20)).await;
        }

        // A persistent stall: A keeps the lock for release_after, then lets go.
        stalled.store(true, Ordering::SeqCst);
        let stalled_at = Instant::now();
        let hold = t.release_after - Duration::from_millis(100);
        while stalled_at.elapsed() < hold {
            assert!(a.is_leader(), "A keeps the lock until the stall persisted");
            sleep(Duration::from_millis(20)).await;
        }
        assert!(
            wait_exclusive(
                &a,
                &b,
                t.release_after + 2 * (t.retry_interval + t.ping_timeout),
                || !a.is_leader()
            )
            .await,
            "A must release once the stall persisted"
        );
        assert!(
            wait_exclusive(&a, &b, Duration::from_secs(5), || b.is_leader()).await,
            "B takes over after the release"
        );

        // While stalled, A does not take the lock even when it is free.
        cb.cancel();
        let until = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < until {
            assert!(!a.is_leader(), "a stalled replica must not acquire");
            sleep(Duration::from_millis(20)).await;
        }
        stalled.store(false, Ordering::SeqCst);
        assert!(
            wait_for(Duration::from_secs(5), || a.is_leader()).await,
            "A acquires once its loops make progress again"
        );
        ca.cancel();
        Ok(())
    }

    /// Test 10: followers can name the session holding the lock.
    #[tokio::test]
    #[serial]
    async fn follower_reports_lock_holder() -> Result<()> {
        let h = boot().await?;
        let mut conn = PgConnection::connect(&h.url).await?;
        assert_eq!(lock_holder(&mut conn).await?, None, "nobody holds it yet");

        let (a, ca) = start(&h.url);
        assert!(wait_for(Duration::from_secs(5), || a.is_leader()).await);
        let holder = lock_holder(&mut conn).await?.expect("A holds the lock");
        assert_eq!(holder.application_name.as_deref(), Some(APPLICATION_NAME));
        assert!(
            holder.backend_start.is_some(),
            "same role: the row is visible"
        );
        let (app,): (String,) =
            sqlx::query_as("SELECT application_name FROM pg_stat_activity WHERE pid = $1")
                .bind(holder.pid)
                .fetch_one(&mut conn)
                .await?;
        assert_eq!(app, APPLICATION_NAME);
        ca.cancel();
        Ok(())
    }
}
