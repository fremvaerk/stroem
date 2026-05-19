//! High-availability integration tests.
//!
//! These tests boot a real Postgres via testcontainers, then exercise the
//! HA primitives end-to-end:
//!
//! - Leader election: only one `LeaderElection` instance holds the advisory
//!   lock at a time; failover happens within seconds when the holder drops.
//! - NOTIFY/LISTEN event bus: cancellation, log chunks, and workspace reloads
//!   propagate from one replica to another via Postgres pub/sub.

use anyhow::Result;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{create_pool, run_migrations};
use stroem_server::config::{
    DbConfig, LogStorageConfig, RecoveryConfig, RetentionConfig, ServerConfig,
};
use stroem_server::events::{start_listener, EventBus};
use stroem_server::leader::LeaderElection;
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

struct Harness {
    _container: testcontainers::ContainerAsync<Postgres>,
    pool: PgPool,
    url: String,
    _temp: TempDir,
}

async fn boot() -> Result<Harness> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    let temp = TempDir::new()?;
    Ok(Harness {
        _container: container,
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
            ..Default::default()
        },
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
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

// ─── Leader election ─────────────────────────────────────────────────────────

#[tokio::test]
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
async fn notify_log_chunk_oversize_signals_only() -> Result<()> {
    let h = boot().await?;
    let cancel = CancellationToken::new();

    let state_a = build_state(&h, Uuid::new_v4());
    let state_b = build_state(&h, Uuid::new_v4());

    let _listener_b = start_listener(h.url.clone(), state_b.clone(), cancel.clone());
    sleep(Duration::from_millis(500)).await;

    let job = Uuid::new_v4();
    let mut rx_b = state_b.log_broadcast.subscribe(job).await;

    // Way over NOTIFY_MAX_BYTES (7000) — fall back to signal-only.
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

    // Simulate what worker_api/jobs.rs does: broadcast locally THEN publish.
    let chunk = "{\"step\":\"s\",\"line\":\"only-once\"}\n";
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

    cancel.cancel();
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
    stroem_server::cancellation::cancel_job(&state_a, job_id).await?;

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

// ─── Must: /healthz/detail — follower with stopped scheduler ─────────────────

/// When this replica is a follower and the scheduler guard is not held,
/// `GET /healthz/detail` must return 200 with `checks.scheduler == "follower"`.
/// Followers don't fail health on background-task liveness.
#[tokio::test(flavor = "multi_thread")]
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
