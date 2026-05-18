use crate::state::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};

/// GET /healthz — unauthenticated, k8s-probe-compatible health check.
///
/// Returns 200 when the database is reachable and the process is alive,
/// 503 otherwise. Body contains only `status` and `db` — no leader identity
/// or per-task fields are included to avoid leaking cluster topology to
/// unauthenticated callers.
///
/// For full HA diagnostics (leader flag, per-task liveness), use
/// `GET /healthz/detail` which requires a valid worker-token Bearer credential.
pub async fn healthz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Database connectivity — time-boxed to 3 seconds to avoid blocking probes.
    let db_ok = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sqlx::query("SELECT 1").execute(&state.pool),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);

    let status = if db_ok { "ok" } else { "unhealthy" };
    let code = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        code,
        Json(json!({
            "status": status,
            "db": if db_ok { "ok" } else { "error" },
        })),
    )
}

/// GET /healthz/detail — authenticated health check with full HA diagnostics.
///
/// Requires a valid `Authorization: Bearer <worker_token>` header. Returns the
/// same shape as the old `/healthz` response: `status`, `checks` (db, leader,
/// scheduler, recovery, event_source). The leader flag and per-task strings
/// are only exposed on this authenticated endpoint to avoid leaking cluster
/// topology to unauthenticated clients.
///
/// Auth: worker-token Bearer is the simplest available gate at this layer
/// without importing the full JWT middleware stack into the health module.
pub async fn healthz_detail(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Note: caller authentication (worker_token check) is enforced by the
    // `require_worker_token` middleware applied to this route in `web/mod.rs`.

    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    // Database connectivity
    let db_ok = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sqlx::query("SELECT 1").execute(&state.pool),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);
    checks.insert("db".into(), json!(if db_ok { "ok" } else { "error" }));
    if !db_ok {
        all_ok = false;
    }

    let is_leader = state.leader.is_leader();
    checks.insert("leader".into(), json!(is_leader));

    let sched = state
        .background_tasks
        .scheduler_alive
        .load(Ordering::Relaxed);
    let recovery = state
        .background_tasks
        .recovery_alive
        .load(Ordering::Relaxed);
    let event_source = state
        .background_tasks
        .event_source_alive
        .load(Ordering::Relaxed);

    if is_leader {
        checks.insert(
            "scheduler".into(),
            json!(if sched { "ok" } else { "stopped" }),
        );
        checks.insert(
            "recovery".into(),
            json!(if recovery { "ok" } else { "stopped" }),
        );
        checks.insert(
            "event_source".into(),
            json!(if event_source { "ok" } else { "stopped" }),
        );
        if !sched || !recovery || !event_source {
            all_ok = false;
        }
    } else {
        // Followers report task state for visibility but don't fail health
        // on it — only the leader is supposed to be doing this work.
        checks.insert(
            "scheduler".into(),
            json!(if sched { "ok" } else { "follower" }),
        );
        checks.insert(
            "recovery".into(),
            json!(if recovery { "ok" } else { "follower" }),
        );
        checks.insert(
            "event_source".into(),
            json!(if event_source { "ok" } else { "follower" }),
        );
    }

    let status = if all_ok {
        "ok"
    } else if db_ok {
        "degraded"
    } else {
        "unhealthy"
    };

    let code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (code, Json(json!({ "status": status, "checks": checks })))
}

#[cfg(test)]
mod tests {
    use crate::state::BackgroundTasks;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_background_tasks_default_false() {
        let tasks = BackgroundTasks::new();
        assert!(!tasks.scheduler_alive.load(Ordering::Relaxed));
        assert!(!tasks.recovery_alive.load(Ordering::Relaxed));
        assert!(!tasks.event_source_alive.load(Ordering::Relaxed));
    }

    #[test]
    fn test_background_tasks_set_true() {
        let tasks = BackgroundTasks::new();
        tasks.scheduler_alive.store(true, Ordering::Relaxed);
        tasks.recovery_alive.store(true, Ordering::Relaxed);
        tasks.event_source_alive.store(true, Ordering::Relaxed);
        assert!(tasks.scheduler_alive.load(Ordering::Relaxed));
        assert!(tasks.recovery_alive.load(Ordering::Relaxed));
        assert!(tasks.event_source_alive.load(Ordering::Relaxed));
    }

    #[test]
    fn test_alive_guard_drop_clears_flag() {
        use crate::state::AliveGuard;

        let flag = Arc::new(AtomicBool::new(false));
        {
            let _guard = AliveGuard::new(flag.clone());
            assert!(
                flag.load(Ordering::Relaxed),
                "flag must be true while guard is alive"
            );
        }
        // Guard dropped here
        assert!(
            !flag.load(Ordering::Relaxed),
            "flag must be false after guard is dropped"
        );
    }

    #[test]
    fn test_background_tasks_clone_shares_arc() {
        let tasks = BackgroundTasks::new();
        let cloned = tasks.clone();

        tasks.scheduler_alive.store(true, Ordering::Relaxed);
        assert!(cloned.scheduler_alive.load(Ordering::Relaxed));
    }
}
