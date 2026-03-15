use crate::state::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};

/// GET /healthz — unauthenticated health check.
///
/// Verifies database connectivity and background task liveness.
/// Returns 200 with `status: "ok"` when all checks pass, or 503 otherwise.
pub async fn healthz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    // Database connectivity — time-boxed to 3 seconds to avoid blocking health checks
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

    // Scheduler liveness
    let sched = state
        .background_tasks
        .scheduler_alive
        .load(Ordering::Relaxed);
    checks.insert(
        "scheduler".into(),
        json!(if sched { "ok" } else { "stopped" }),
    );
    if !sched {
        all_ok = false;
    }

    // Recovery sweeper liveness
    let recovery = state
        .background_tasks
        .recovery_alive
        .load(Ordering::Relaxed);
    checks.insert(
        "recovery".into(),
        json!(if recovery { "ok" } else { "stopped" }),
    );
    if !recovery {
        all_ok = false;
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
    }

    #[test]
    fn test_background_tasks_set_true() {
        let tasks = BackgroundTasks::new();
        tasks.scheduler_alive.store(true, Ordering::Relaxed);
        tasks.recovery_alive.store(true, Ordering::Relaxed);
        assert!(tasks.scheduler_alive.load(Ordering::Relaxed));
        assert!(tasks.recovery_alive.load(Ordering::Relaxed));
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
