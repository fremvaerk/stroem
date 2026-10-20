use crate::config::ServerConfig;
use crate::leader::{FitnessProbe, LoopFault, UnfitLoop};
use crate::state::{AppState, BackgroundTasks};
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};

/// A scheduler iteration is a DB check plus a job creation per due trigger,
/// and the loop never sleeps longer than `scheduler::MAX_SLEEP`.
const SCHEDULER_STALL_SECS: u64 = 300;
/// One reconcile renders templates and may create a few jobs.
const EVENT_SOURCE_STALL_SECS: u64 = 600;
/// A sweep includes retention (blob deletes), so allow a long iteration.
const RECOVERY_STALL_FLOOR_SECS: u64 = 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskHealth {
    Ok,
    /// The task exited (or was never started): its `AliveGuard` is not held.
    Stopped,
    /// The task is alive but its loop has not come round for too long.
    Stalled,
}

pub(crate) fn task_health(
    alive: bool,
    beat_age_secs: Option<u64>,
    stall_after_secs: u64,
) -> TaskHealth {
    match (alive, beat_age_secs) {
        (false, _) => TaskHealth::Stopped,
        (true, Some(age)) if age > stall_after_secs => TaskHealth::Stalled,
        (true, _) => TaskHealth::Ok,
    }
}

struct TaskReport {
    name: &'static str,
    health: TaskHealth,
    /// Whether the loop ever ran. Tells a task that died from one that was
    /// never spawned (router-only tests, the window before `main` starts it).
    started: bool,
}

fn recovery_stall_secs(config: &ServerConfig) -> u64 {
    RECOVERY_STALL_FLOOR_SECS.max(config.recovery.sweep_interval_secs.saturating_mul(5))
}

fn background_report(state: &AppState) -> [TaskReport; 3] {
    background_report_of(&state.background_tasks, recovery_stall_secs(&state.config))
}

fn background_report_of(bg: &BackgroundTasks, recovery_stall: u64) -> [TaskReport; 3] {
    [
        (
            "scheduler",
            &bg.scheduler_alive,
            &bg.scheduler_beat,
            SCHEDULER_STALL_SECS,
        ),
        (
            "recovery",
            &bg.recovery_alive,
            &bg.recovery_beat,
            recovery_stall,
        ),
        (
            "event_source",
            &bg.event_source_alive,
            &bg.event_source_beat,
            EVENT_SOURCE_STALL_SECS,
        ),
    ]
    .map(|(name, alive, beat, stall_after)| {
        let age = beat.age().map(|d| d.as_secs());
        TaskReport {
            name,
            health: task_health(alive.load(Ordering::Relaxed), age, stall_after),
            started: age.is_some(),
        }
    })
}

/// The first leader-gated loop that `/livez` fails on: `Stalled`, or
/// `Stopped` after it started (spec 2026-10-20 § 4.4). A loop that never
/// beat is not unfit.
pub(crate) fn unfit_loop(bg: &BackgroundTasks, recovery_stall: u64) -> Option<UnfitLoop> {
    background_report_of(bg, recovery_stall)
        .into_iter()
        .find_map(|t| {
            let fault = match t.health {
                TaskHealth::Stalled => LoopFault::Stalled,
                TaskHealth::Stopped if t.started => LoopFault::Stopped,
                _ => return None,
            };
            Some(UnfitLoop {
                name: t.name,
                fault,
            })
        })
}

/// The leader's fitness probe: names an unfit leader-gated loop, so the
/// leader releases the lock instead of holding it for a process that does no
/// leader work (`LeaderElection::set_fitness_probe`).
pub fn leader_fitness_probe(state: &AppState) -> FitnessProbe {
    let bg = state.background_tasks.clone();
    let recovery_stall = recovery_stall_secs(&state.config);
    Arc::new(move || unfit_loop(&bg, recovery_stall))
}

async fn db_ok(state: &AppState) -> bool {
    // Time-boxed to 3 seconds to avoid blocking probes.
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sqlx::query("SELECT 1").execute(&state.pool),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// GET /livez — unauthenticated LIVENESS probe: should this process be restarted?
///
/// 503 when a background loop is STALLED (alive but not making progress) or
/// ran and then EXITED — on any replica: a hung standby is no standby, and a
/// replica whose loop exited cannot do that loop's work as leader and, with
/// self-release, never becomes leader again (spec 2026-10-20 § 4.4.1). The
/// database is deliberately NOT checked: restarting the server does not fix a
/// database outage. Body is `{"status": "ok" | "stalled" | "stopped"}` — no
/// task name, no leader identity.
pub async fn livez(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let report = background_report(&state);
    let status = if report.iter().any(|t| t.health == TaskHealth::Stalled) {
        "stalled"
    } else if report
        .iter()
        .any(|t| t.health == TaskHealth::Stopped && t.started)
    {
        "stopped"
    } else {
        "ok"
    };
    let code = if status == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(json!({ "status": status })))
}

/// GET /healthz — unauthenticated READINESS probe: can this replica serve
/// traffic?
///
/// Returns 200 when the database is reachable, 503 otherwise. Background
/// loops are deliberately not part of it — a hung scheduler must restart the
/// pod (`/livez`), not pull a replica that still serves API and worker
/// requests out of the Service. Body contains only `status` and `db` — no
/// leader identity or per-task fields, to avoid leaking cluster topology to
/// unauthenticated callers.
///
/// For full HA diagnostics (leader flag, per-task liveness), use
/// `GET /healthz/detail` which requires a valid worker-token Bearer credential.
pub async fn healthz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db_ok = db_ok(&state).await;

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
/// Requires a valid `Authorization: Bearer <worker_token>` header. Returns
/// `status` and `checks` (db, leader, scheduler, recovery, event_source). Each
/// task is `ok`, `stalled`, or — when its guard is not held — `stopped` on the
/// leader or when it ran before, and `follower` for a never-started task on a
/// follower. The leader flag and per-task strings are
/// only exposed on this authenticated endpoint to avoid leaking cluster
/// topology to unauthenticated clients.
pub async fn healthz_detail(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Note: caller authentication (worker_token check) is enforced by the
    // `require_worker_token` middleware applied to this route in `web/mod.rs`.

    let mut checks = serde_json::Map::new();

    let db_ok = db_ok(&state).await;
    checks.insert("db".into(), json!(if db_ok { "ok" } else { "error" }));
    let mut all_ok = db_ok;

    let is_leader = state.leader.is_leader();
    checks.insert("leader".into(), json!(is_leader));

    for task in background_report(&state) {
        let label = match task.health {
            TaskHealth::Ok => "ok",
            TaskHealth::Stalled => "stalled",
            TaskHealth::Stopped if is_leader || task.started => "stopped",
            // A follower whose task never started (not spawned yet) does not
            // fail health on it.
            TaskHealth::Stopped => "follower",
        };
        if matches!(label, "stalled" | "stopped") {
            all_ok = false;
        }
        checks.insert(task.name.into(), json!(label));
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
    use super::{task_health, unfit_loop, TaskHealth, SCHEDULER_STALL_SECS};
    use crate::leader::{LoopFault, UnfitLoop};
    use crate::state::{BackgroundTasks, Heartbeat};
    use std::time::Duration;

    /// Ages come from a monotonic clock: a wall-clock correction must neither
    /// fake a stall nor hide one (Codex review 2026-09-17).
    #[test]
    fn test_heartbeat_age_none_until_first_beat() {
        let hb = Heartbeat::default();
        assert_eq!(hb.age(), None);
        hb.beat();
        assert!(hb.age().unwrap() < Duration::from_secs(5));
        hb.backdate(Duration::from_secs(3600));
        let age = hb.age().unwrap();
        assert!(age >= Duration::from_secs(3600) && age < Duration::from_secs(3605));
    }

    #[test]
    fn test_task_health_stopped_when_guard_dropped() {
        assert_eq!(task_health(false, Some(1), 300), TaskHealth::Stopped);
        assert_eq!(task_health(false, None, 300), TaskHealth::Stopped);
    }

    /// Prod 2026-09-16: the scheduler task was alive (guard held) but never
    /// ran again. Liveness must notice a loop that stopped iterating.
    #[test]
    fn test_task_health_stalled_when_alive_but_beat_too_old() {
        assert_eq!(task_health(true, Some(301), 300), TaskHealth::Stalled);
        assert_eq!(task_health(true, Some(300), 300), TaskHealth::Ok);
        assert_eq!(task_health(true, Some(0), 300), TaskHealth::Ok);
    }

    #[test]
    fn test_unfit_loop_names_the_stalled_gated_loop() {
        let tasks = BackgroundTasks::new();
        tasks.scheduler_alive.store(true, Ordering::Relaxed);
        tasks.recovery_alive.store(true, Ordering::Relaxed);
        tasks.event_source_alive.store(true, Ordering::Relaxed);
        // Not started yet: healthy.
        assert_eq!(unfit_loop(&tasks, 1800), None);
        tasks.scheduler_beat.beat();
        tasks.recovery_beat.beat();
        tasks.event_source_beat.beat();
        assert_eq!(unfit_loop(&tasks, 1800), None);
        tasks
            .scheduler_beat
            .backdate(Duration::from_secs(SCHEDULER_STALL_SECS + 1));
        assert_eq!(
            unfit_loop(&tasks, 1800),
            Some(UnfitLoop {
                name: "scheduler",
                fault: LoopFault::Stalled
            })
        );
    }

    /// A loop that ran and then exited is unfit (spec § 4.4); `/livez` fails
    /// on it on every replica, so a leader that released stays visible.
    #[test]
    fn test_unfit_loop_names_a_started_loop_that_stopped() {
        let tasks = BackgroundTasks::new();
        tasks.scheduler_alive.store(true, Ordering::Relaxed);
        tasks.recovery_alive.store(true, Ordering::Relaxed);
        tasks.event_source_alive.store(true, Ordering::Relaxed);
        tasks.scheduler_beat.beat();
        tasks.recovery_beat.beat();
        tasks.event_source_beat.beat();
        tasks.recovery_alive.store(false, Ordering::Relaxed);
        assert_eq!(
            unfit_loop(&tasks, 1800),
            Some(UnfitLoop {
                name: "recovery",
                fault: LoopFault::Stopped
            })
        );
    }

    /// A loop that never beat is not unfit, alive or not: it may not be
    /// spawned yet (router-only tests, the window before `main` starts it).
    #[test]
    fn test_unfit_loop_ignores_loops_that_never_started() {
        let tasks = BackgroundTasks::new();
        assert!(!tasks.recovery_alive.load(Ordering::Relaxed));
        assert_eq!(unfit_loop(&tasks, 1800), None);
    }

    #[test]
    fn test_task_health_ok_before_first_beat() {
        // Spawned, first iteration not finished yet.
        assert_eq!(task_health(true, None, 300), TaskHealth::Ok);
    }
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
