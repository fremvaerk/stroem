//! Integration tests for `cascade::apply` — the write side of the step
//! cascade. Exercises the six stroem-db `_tx` primitives against a real
//! Postgres database (via testcontainers) to verify transactionality and
//! row-count guards.

use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep};
use stroem_server::cascade::execute;
use stroem_server::cascade::{apply, ApplyError, Change, Plan, RollupOutcome};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

async fn create_job(pool: &PgPool) -> Uuid {
    JobRepo::create(
        pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
        None,
        None,
    )
    .await
    .expect("create job")
}

fn step(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "noop".to_string(),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: Some(json!({"script": "true"})),
        input: None,
        status: status.to_string(),
        required_ability: "script".to_string(),
        required_tags: vec!["script".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
        for_each_expr: None,
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
        max_retries: None,
        retry_backoff_secs: None,
        retry_strategy: None,
        retry_jitter: false,
        action_workspace: None,
        action_revision: None,
    }
}

fn placeholder(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        for_each_expr: Some("[1,2]".to_string()),
        ..step(job_id, name, status)
    }
}

fn instance(job_id: Uuid, source: &str, i: i32, status: &str) -> NewJobStep {
    NewJobStep {
        loop_source: Some(source.to_string()),
        loop_index: Some(i),
        loop_total: Some(2),
        loop_item: Some(json!(i)),
        ..step(job_id, &format!("{source}[{i}]"), status)
    }
}

async fn step_statuses(pool: &PgPool, job_id: Uuid) -> HashMap<String, String> {
    JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .expect("get steps")
        .into_iter()
        .map(|s| (s.step_name, s.status))
        .collect()
}

#[tokio::test]
async fn apply_is_transactional_expand_rolls_back_without_commit() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[placeholder(job_id, "p", "pending")]).await?;
    let plan = Plan {
        changes: vec![Change::Expand {
            placeholder: "p".into(),
            instances: vec![
                instance(job_id, "p", 0, "ready"),
                instance(job_id, "p", 1, "ready"),
            ],
        }],
    };
    {
        let mut tx = pool.begin().await?;
        let applied = apply(&mut tx, job_id, &plan).await.unwrap();
        assert_eq!(applied.expanded, 1);
        // dropped without commit
    }
    let s = step_statuses(&pool, job_id).await;
    assert_eq!(s["p"], "pending");
    assert!(
        !s.contains_key("p[0]"),
        "instances must not exist without the placeholder transition"
    );
    Ok(())
}

#[tokio::test]
async fn apply_sets_timestamps_and_statuses() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "pending"),
            step(job_id, "b", "pending"),
            step(job_id, "c", "pending"),
            placeholder(job_id, "p", "pending"),
            placeholder(job_id, "q", "running"),
            instance(job_id, "q", 0, "completed"),
        ],
    )
    .await?;
    let plan = Plan {
        changes: vec![
            Change::Promote { step: "a".into() },
            Change::Skip { step: "b".into() },
            Change::Fail {
                step: "c".into(),
                error: "when condition error: x".into(),
            },
            Change::Expand {
                placeholder: "p".into(),
                instances: vec![instance(job_id, "p", 0, "ready")],
            },
            Change::Rollup {
                placeholder: "q".into(),
                outcome: RollupOutcome::Completed(json!([1])),
            },
        ],
    };
    let mut tx = pool.begin().await?;
    let applied = apply(&mut tx, job_id, &plan).await.unwrap();
    tx.commit().await?;
    assert_eq!(applied.promoted, 1);
    assert_eq!(applied.skipped, 1);
    assert_eq!(applied.failed, 1);
    assert_eq!(applied.expanded, 1);
    assert_eq!(applied.rolled_up, 1);

    let rows = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let by = |n: &str| rows.iter().find(|r| r.step_name == n).unwrap();
    assert_eq!(by("a").status, "ready");
    let ready_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT ready_at FROM job_step WHERE job_id = $1 AND step_name = 'a'")
            .bind(job_id)
            .fetch_one(&pool)
            .await?;
    assert!(ready_at.is_some(), "Promote sets ready_at");
    assert_eq!(by("b").status, "skipped");
    assert!(by("b").completed_at.is_some(), "Skip sets completed_at");
    assert_eq!(by("c").status, "failed");
    assert_eq!(
        by("c").error_message.as_deref(),
        Some("when condition error: x")
    );
    assert!(by("c").completed_at.is_some());
    assert_eq!(by("p").status, "running");
    assert!(by("p").started_at.is_some(), "Expand sets started_at");
    assert_eq!(by("p[0]").status, "ready");
    assert_eq!(by("q").status, "completed");
    assert_eq!(by("q").output, Some(json!([1])));
    assert!(by("q").completed_at.is_some());
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "running",
        "Expand moves a pending job to running"
    );
    Ok(())
}

#[tokio::test]
async fn apply_guard_miss_returns_error_and_writes_nothing_after_rollback() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(job_id, "a", "pending"), step(job_id, "b", "cancelled")],
    )
    .await?;
    let plan = Plan {
        changes: vec![
            Change::Promote { step: "a".into() },
            Change::Promote { step: "b".into() },
        ],
    };
    let mut tx = pool.begin().await?;
    let err = apply(&mut tx, job_id, &plan).await.unwrap_err();
    assert!(
        matches!(err, ApplyError::GuardMiss { ref step } if step.contains("b")),
        "{err}"
    );
    tx.rollback().await?;
    let s = step_statuses(&pool, job_id).await;
    assert_eq!(s["a"], "pending", "the whole plan rolled back");
    assert_eq!(s["b"], "cancelled");
    Ok(())
}

#[tokio::test]
async fn apply_rollup_on_non_running_placeholder_is_a_guard_miss() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[placeholder(job_id, "q", "cancelled")]).await?;
    let plan = Plan {
        changes: vec![Change::Rollup {
            placeholder: "q".into(),
            outcome: RollupOutcome::Completed(json!([])),
        }],
    };
    let mut tx = pool.begin().await?;
    let err = apply(&mut tx, job_id, &plan).await.unwrap_err();
    assert!(matches!(err, ApplyError::GuardMiss { .. }));
    Ok(())
}

#[tokio::test]
async fn apply_job_running_update_may_match_zero_rows() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    sqlx::query("UPDATE job SET status = 'running' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;
    JobStepRepo::create_steps(&pool, &[placeholder(job_id, "p", "pending")]).await?;
    let plan = Plan {
        changes: vec![Change::Expand {
            placeholder: "p".into(),
            instances: vec![instance(job_id, "p", 0, "ready")],
        }],
    };
    let mut tx = pool.begin().await?;
    apply(&mut tx, job_id, &plan).await.unwrap();
    tx.commit().await?;
    assert_eq!(step_statuses(&pool, job_id).await["p"], "running");
    Ok(())
}

fn make_task(flow: HashMap<String, FlowStep>) -> TaskDef {
    TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,
        retry: None,
        on_success: vec![],
        on_error: vec![],
        on_suspended: vec![],
        on_cancel: vec![],
    }
}

fn flow_step(depends_on: Vec<&str>) -> FlowStep {
    FlowStep {
        action: "noop".to_string(),
        name: None,
        description: None,
        depends_on: depends_on.into_iter().map(str::to_string).collect(),
        input: HashMap::new(),
        continue_on_failure: false,
        timeout: None,
        when: None,
        for_each: None,
        sequential: false,
        retry: None,
        inline_action: None,
    }
}

#[tokio::test]
async fn execute_empty_plan_touches_nothing() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "running")]).await?;
    let task = make_task(HashMap::from([("a".to_string(), flow_step(vec![]))]));
    let plan = execute(&pool, job_id, &task, Some(&WorkspaceConfig::new())).await?;
    assert!(plan.changes.is_empty());
    Ok(())
}

/// Proves `execute` re-reads the job/steps on each loop iteration rather than
/// reusing a stale snapshot — NOT the `Err(GuardMiss) => rollback; continue` retry
/// arm itself. `b` is already `cancelled` by the time `execute` takes its first
/// (only) snapshot, so `run` plans nothing and `execute` returns through the
/// empty-plan early exit without ever calling `apply`. See
/// `execute_retries_after_a_real_guard_miss` for a test that forces a genuine
/// mid-flight guard miss and exercises the retry arm.
#[tokio::test]
async fn execute_replans_from_a_fresh_snapshot() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(job_id, "a", "completed"), step(job_id, "b", "pending")],
    )
    .await?;
    let task = make_task(HashMap::from([
        ("a".to_string(), flow_step(vec![])),
        ("b".to_string(), flow_step(vec!["a"])),
    ]));
    // Prove the plan is non-empty on this snapshot, then invalidate it.
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let stale = stroem_server::cascade::run(&task, &job, &steps, Some(&WorkspaceConfig::new()))?;
    assert_eq!(stale.changes, vec![Change::Promote { step: "b".into() }]);
    JobStepRepo::cancel_pending_steps(&pool, job_id).await?;

    let plan = execute(&pool, job_id, &task, Some(&WorkspaceConfig::new())).await?;
    assert!(
        plan.changes.is_empty(),
        "the re-run sees b cancelled and plans nothing"
    );
    assert_eq!(step_statuses(&pool, job_id).await["b"], "cancelled");
    Ok(())
}

/// Two concurrent executes for the same job: both return, the join is promoted
/// exactly once, final state equals a serial run. Smoke test of the re-run path.
#[tokio::test]
async fn execute_concurrently_promotes_join_once() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "l", "completed"),
            step(job_id, "r", "completed"),
            step(job_id, "join", "pending"),
        ],
    )
    .await?;
    let task = make_task(HashMap::from([
        ("l".to_string(), flow_step(vec![])),
        ("r".to_string(), flow_step(vec![])),
        ("join".to_string(), flow_step(vec!["l", "r"])),
    ]));
    let ws = WorkspaceConfig::new();
    let (p1, p2) = tokio::join!(
        execute(&pool, job_id, &task, Some(&ws)),
        execute(&pool, job_id, &task, Some(&ws)),
    );
    let (p1, p2) = (p1?, p2?);
    let promoted = p1.changes.len() + p2.changes.len();
    assert_eq!(promoted, 1, "exactly one of them applied the promotion");
    assert_eq!(step_statuses(&pool, job_id).await["join"], "ready");
    Ok(())
}

/// Deterministically exercises the `Err(GuardMiss) => rollback; continue` retry
/// arm, rather than relying on `execute_concurrently_promotes_join_once`'s race.
///
/// A dedicated connection `a` opens a transaction and issues (but does not
/// commit) the same `UPDATE job_step SET status = 'ready' ... WHERE status =
/// 'pending'` that `execute`'s `apply` would issue for `join`. That gives `a` an
/// exclusive row lock on `join` while the committed row is still `pending`.
///
/// `execute` is spawned concurrently on the pool: its snapshot SELECT sees the
/// still-committed `pending` status (under READ COMMITTED, `a`'s uncommitted
/// write is invisible), so it plans `Promote{join}` and its own `apply` issues
/// the same `UPDATE ... WHERE status = 'pending'` — which blocks on `a`'s row
/// lock instead of racing it. Once we observe (via `pg_stat_activity`) that the
/// blocked backend is actually waiting, we commit `a`. The blocked UPDATE then
/// requalifies its `WHERE` clause against the just-committed row (now `ready`),
/// matches zero rows, and `expect_rows` turns that into a real `GuardMiss` —
/// forcing `execute` through rollback + re-run, not just re-reading a snapshot
/// that was already stale before the first read (unlike
/// `execute_replans_from_a_fresh_snapshot`). The re-run's fresh snapshot sees
/// `join` already `ready` and plans nothing. Without the retry loop, `execute`
/// would instead return `Err` ("cascade guard miss...").
#[tokio::test]
async fn execute_retries_after_a_real_guard_miss() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "l", "completed"),
            step(job_id, "r", "completed"),
            step(job_id, "join", "pending"),
        ],
    )
    .await?;
    let task = make_task(HashMap::from([
        ("l".to_string(), flow_step(vec![])),
        ("r".to_string(), flow_step(vec![])),
        ("join".to_string(), flow_step(vec!["l", "r"])),
    ]));
    let ws = WorkspaceConfig::new();

    let mut a = pool.acquire().await?;
    sqlx::query("BEGIN").execute(&mut *a).await?;
    sqlx::query(
        "UPDATE job_step SET status = 'ready', ready_at = NOW() \
         WHERE job_id = $1 AND step_name = 'join' AND status = 'pending'",
    )
    .bind(job_id)
    .execute(&mut *a)
    .await?;

    let spawned = {
        let pool = pool.clone();
        let task = task.clone();
        let ws = ws.clone();
        tokio::spawn(async move { execute(&pool, job_id, &task, Some(&ws)).await })
    };

    // Wait for `execute`'s own UPDATE to actually block on `a`'s held row lock,
    // bounded so a regression that never blocks fails fast instead of hanging.
    let mut waited = 0;
    loop {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query ILIKE '%job_step%'",
        )
        .fetch_one(&pool)
        .await?;
        if blocked >= 1 {
            break;
        }
        waited += 1;
        if waited >= 200 {
            anyhow::bail!("timed out waiting for execute's UPDATE to block on the held row lock");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    sqlx::query("COMMIT").execute(&mut *a).await?;

    let plan = spawned.await??;
    assert!(
        plan.changes.is_empty(),
        "the re-run after the guard miss sees join already ready and plans nothing"
    );
    assert_eq!(step_statuses(&pool, job_id).await["join"], "ready");
    Ok(())
}
