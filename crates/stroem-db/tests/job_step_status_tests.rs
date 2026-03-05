use anyhow::Result;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::FlowStep;
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep, WorkerRepo};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

// ─── Test infrastructure ──────────────────────────────────────────────

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

/// Create a minimal job and return its ID.
async fn make_job(pool: &PgPool, task_name: &str) -> Result<Uuid> {
    JobRepo::create(pool, "default", task_name, "distributed", None, "api", None).await
}

/// Build a `NewJobStep` with sensible defaults.  Only `step_name` and `status`
/// need to differ between most test cases; everything else can be shared.
fn make_step(job_id: Uuid, step_name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: step_name.to_string(),
        action_name: "test-action".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: status.to_string(),
        required_tags: vec!["shell".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
    }
}

// ─── mark_failed tests ────────────────────────────────────────────────

/// `mark_failed` sets status to "failed", records the error message, and
/// populates `completed_at`.
#[tokio::test]
async fn test_mark_failed_sets_status_and_error_message() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "fail-test").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "ready")]).await?;

    JobStepRepo::mark_failed(&pool, job_id, "step1", "exit code 1").await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 1);
    let step = &steps[0];

    assert_eq!(step.status, "failed");
    assert_eq!(step.error_message.as_deref(), Some("exit code 1"));
    assert!(step.completed_at.is_some(), "completed_at must be set");

    Ok(())
}

/// `mark_failed` should persist a long, multiline error message verbatim.
#[tokio::test]
async fn test_mark_failed_preserves_multiline_error_message() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "fail-multiline").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "ready")]).await?;

    let long_error = "Error on line 1\nError on line 2\nError on line 3";
    JobStepRepo::mark_failed(&pool, job_id, "step1", long_error).await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(
        steps[0].error_message.as_deref(),
        Some(long_error),
        "full multiline error must survive the round-trip"
    );

    Ok(())
}

/// `mark_failed` with an empty string stores an empty `error_message`, not NULL.
#[tokio::test]
async fn test_mark_failed_with_empty_error_string() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "fail-empty-err").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "ready")]).await?;

    JobStepRepo::mark_failed(&pool, job_id, "step1", "").await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = &steps[0];
    assert_eq!(step.status, "failed");
    // The DB column is TEXT NOT NULL-equivalent via the binding; an empty string
    // is stored as an empty string, not NULL.
    assert_eq!(
        step.error_message.as_deref(),
        Some(""),
        "empty error string must not be coerced to NULL"
    );

    Ok(())
}

/// `mark_failed` applied to a step that is already in a terminal state
/// (`completed`) overwrites the status — the UPDATE matches and writes.
/// This documents the current (unconditional UPDATE) behaviour.
#[tokio::test]
async fn test_mark_failed_overwrites_completed_step() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "fail-overwrite").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "ready")]).await?;

    // First complete the step normally.
    JobStepRepo::mark_completed(
        &pool,
        job_id,
        "step1",
        Some(serde_json::json!({"result": "ok"})),
    )
    .await?;

    {
        let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
        assert_eq!(steps[0].status, "completed");
    }

    // Now mark it failed — the unconditional UPDATE applies.
    JobStepRepo::mark_failed(&pool, job_id, "step1", "late failure").await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "failed");
    assert_eq!(steps[0].error_message.as_deref(), Some("late failure"));

    Ok(())
}

/// `mark_failed` on a non-existent step is a no-op (zero rows updated) and
/// must not return an error.
#[tokio::test]
async fn test_mark_failed_on_nonexistent_step_is_noop() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "fail-noop").await?;

    // No steps created — updating a ghost step must succeed without panicking.
    let result = JobStepRepo::mark_failed(&pool, job_id, "ghost-step", "no such step").await;
    assert!(result.is_ok(), "mark_failed on missing step must not error");

    // The job has no steps; nothing changed.
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert!(steps.is_empty());

    Ok(())
}

// ─── mark_skipped tests ───────────────────────────────────────────────

/// `mark_skipped` sets status to "skipped" and populates `completed_at`.
/// `error_message` is left NULL because skipping carries no error.
#[tokio::test]
async fn test_mark_skipped_sets_status_and_completed_at() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "skip-test").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "pending")]).await?;

    JobStepRepo::mark_skipped(&pool, job_id, "step1").await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 1);
    let step = &steps[0];

    assert_eq!(step.status, "skipped");
    assert!(step.completed_at.is_some(), "completed_at must be set");
    assert!(
        step.error_message.is_none(),
        "skipped step must not have an error_message"
    );

    Ok(())
}

/// `mark_skipped` on a step that starts as "ready" (already claimed before
/// the DAG was updated) also transitions to skipped.
#[tokio::test]
async fn test_mark_skipped_on_ready_step() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "skip-ready").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "ready")]).await?;

    JobStepRepo::mark_skipped(&pool, job_id, "step1").await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "skipped");
    assert!(steps[0].completed_at.is_some());

    Ok(())
}

/// `mark_skipped` on an already-completed step overwrites the status — same
/// unconditional UPDATE behaviour as `mark_failed`.
#[tokio::test]
async fn test_mark_skipped_overwrites_completed_step() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "skip-overwrite").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "ready")]).await?;

    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;
    {
        let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
        assert_eq!(steps[0].status, "completed");
    }

    JobStepRepo::mark_skipped(&pool, job_id, "step1").await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "skipped");

    Ok(())
}

/// `mark_skipped` on a non-existent step is a no-op and must not return an
/// error.
#[tokio::test]
async fn test_mark_skipped_on_nonexistent_step_is_noop() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "skip-noop").await?;

    let result = JobStepRepo::mark_skipped(&pool, job_id, "ghost-step").await;
    assert!(
        result.is_ok(),
        "mark_skipped on missing step must not error"
    );

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert!(steps.is_empty());

    Ok(())
}

// ─── Parent/child relationship column tests ───────────────────────────

/// A child job created with `create_with_parent` stores `parent_job_id` and
/// `parent_step_name` and those values are retrievable via `JobRepo::get`.
#[tokio::test]
async fn test_job_with_parent_columns_are_stored_and_retrieved() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create the parent job.
    let parent_id = JobRepo::create(
        &pool,
        "default",
        "parent-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create a child job that references the parent.
    let child_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&format!("{}/{}", parent_id, "dispatch-step")),
        Some(parent_id),
        Some("dispatch-step"),
        None,
    )
    .await?;

    let child = JobRepo::get(&pool, child_id)
        .await?
        .expect("child job must exist");

    assert_eq!(
        child.parent_job_id,
        Some(parent_id),
        "parent_job_id must equal the parent's job_id"
    );
    assert_eq!(
        child.parent_step_name.as_deref(),
        Some("dispatch-step"),
        "parent_step_name must match the step that spawned this child"
    );
    assert_eq!(child.source_type, "task");

    Ok(())
}

/// A job created via the normal `create` path has NULL parent columns.
#[tokio::test]
async fn test_job_without_parent_has_null_parent_columns() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "root-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let job = JobRepo::get(&pool, job_id).await?.expect("job must exist");

    assert!(
        job.parent_job_id.is_none(),
        "top-level job must have no parent_job_id"
    );
    assert!(
        job.parent_step_name.is_none(),
        "top-level job must have no parent_step_name"
    );

    Ok(())
}

/// Multiple generations of nesting are stored correctly: grandchild references
/// child which references root.
#[tokio::test]
async fn test_job_grandchild_parent_chain() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let root_id =
        JobRepo::create(&pool, "default", "root", "distributed", None, "api", None).await?;

    let child_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child",
        "distributed",
        None,
        "task",
        None,
        Some(root_id),
        Some("step-a"),
        None,
    )
    .await?;

    let grandchild_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "grandchild",
        "distributed",
        None,
        "task",
        None,
        Some(child_id),
        Some("step-b"),
        None,
    )
    .await?;

    let grandchild = JobRepo::get(&pool, grandchild_id)
        .await?
        .expect("grandchild must exist");

    assert_eq!(grandchild.parent_job_id, Some(child_id));
    assert_eq!(grandchild.parent_step_name.as_deref(), Some("step-b"));

    let child = JobRepo::get(&pool, child_id)
        .await?
        .expect("child must exist");
    assert_eq!(child.parent_job_id, Some(root_id));
    assert_eq!(child.parent_step_name.as_deref(), Some("step-a"));

    Ok(())
}

// ─── Step status transition tests ────────────────────────────────────

/// Full "happy path" transition: ready → running → failed.
/// Verifies each intermediate state and that the correct timestamps are set.
#[tokio::test]
async fn test_step_transition_ready_to_running_to_failed() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "transition-fail").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "work", "ready")]).await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "worker-1",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    // ready → running
    JobStepRepo::mark_running(&pool, job_id, "work", worker_id).await?;
    {
        let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
        let step = &steps[0];
        assert_eq!(step.status, "running");
        assert!(step.started_at.is_some());
        assert!(step.completed_at.is_none());
        assert_eq!(step.worker_id, Some(worker_id));
    }

    // running → failed
    JobStepRepo::mark_failed(&pool, job_id, "work", "command not found").await?;
    {
        let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
        let step = &steps[0];
        assert_eq!(step.status, "failed");
        assert!(step.started_at.is_some(), "started_at must persist");
        assert!(step.completed_at.is_some());
        assert_eq!(step.error_message.as_deref(), Some("command not found"));
    }

    Ok(())
}

/// Full transition: pending → (promoted to) ready → running → skipped.
/// `mark_skipped` is not normally called on a running step by the orchestrator,
/// but the DB layer itself does not enforce this constraint.
#[tokio::test]
async fn test_step_transition_pending_to_skipped() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "transition-skip").await?;
    // Create two steps: step1 (ready), step2 (pending, depends on step1).
    let steps = vec![
        make_step(job_id, "step1", "ready"),
        make_step(job_id, "step2", "pending"),
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // step1 fails.
    JobStepRepo::mark_failed(&pool, job_id, "step1", "oops").await?;

    // step2 is still pending.
    {
        let all = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
        let s2 = all.iter().find(|s| s.step_name == "step2").unwrap();
        assert_eq!(s2.status, "pending");
    }

    // Orchestrator decides to skip step2.
    JobStepRepo::mark_skipped(&pool, job_id, "step2").await?;

    let all = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let s2 = all.iter().find(|s| s.step_name == "step2").unwrap();
    assert_eq!(s2.status, "skipped");
    assert!(s2.completed_at.is_some());
    assert!(s2.error_message.is_none());

    Ok(())
}

/// `skip_unreachable_steps` skips a pending step whose dependency failed, when
/// `continue_on_failure` is false.
#[tokio::test]
async fn test_skip_unreachable_steps_skips_blocked_pending() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "skip-unreachable").await?;
    let steps = vec![
        make_step(job_id, "build", "ready"),
        make_step(job_id, "deploy", "pending"),
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Build fails.
    JobStepRepo::mark_failed(&pool, job_id, "build", "compile error").await?;

    // Define the DAG: deploy depends on build.
    let mut flow: HashMap<String, FlowStep> = HashMap::new();
    flow.insert(
        "build".to_string(),
        FlowStep {
            action: "build-action".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            inline_action: None,
        },
    );
    flow.insert(
        "deploy".to_string(),
        FlowStep {
            action: "deploy-action".to_string(),
            name: None,
            description: None,
            depends_on: vec!["build".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            inline_action: None,
        },
    );

    let skipped = JobStepRepo::skip_unreachable_steps(&pool, job_id, &flow).await?;

    assert_eq!(skipped, vec!["deploy".to_string()]);

    let all = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let deploy = all.iter().find(|s| s.step_name == "deploy").unwrap();
    assert_eq!(deploy.status, "skipped");
    assert!(deploy.completed_at.is_some());

    Ok(())
}

/// A step with `continue_on_failure: true` is NOT skipped even when its
/// dependency has failed.
#[tokio::test]
async fn test_skip_unreachable_steps_respects_continue_on_failure() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "skip-continue").await?;
    let steps = vec![
        make_step(job_id, "build", "ready"),
        make_step(job_id, "notify", "pending"),
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    JobStepRepo::mark_failed(&pool, job_id, "build", "oops").await?;

    let mut flow: HashMap<String, FlowStep> = HashMap::new();
    flow.insert(
        "build".to_string(),
        FlowStep {
            action: "build-action".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            inline_action: None,
        },
    );
    flow.insert(
        "notify".to_string(),
        FlowStep {
            action: "notify-action".to_string(),
            name: None,
            description: None,
            depends_on: vec!["build".to_string()],
            input: HashMap::new(),
            // notify always runs, regardless of build outcome.
            continue_on_failure: true,
            timeout: None,
            inline_action: None,
        },
    );

    let skipped = JobStepRepo::skip_unreachable_steps(&pool, job_id, &flow).await?;
    assert!(
        skipped.is_empty(),
        "notify must not be skipped when continue_on_failure is true"
    );

    let all = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let notify = all.iter().find(|s| s.step_name == "notify").unwrap();
    assert_eq!(notify.status, "pending", "notify must remain pending");

    Ok(())
}

// ─── Transaction tests ────────────────────────────────────────────────

/// Committing a transaction that creates a child job (with parent columns) and
/// its steps persists everything atomically.
#[tokio::test]
async fn test_transaction_commit_persists_child_job_and_steps() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let parent_id = make_job(&pool, "parent-tx").await?;
    let child_id = Uuid::new_v4();

    let mut tx = pool.begin().await?;

    JobRepo::create_with_parent_tx_id(
        &mut *tx,
        child_id,
        "default",
        "child-tx",
        "distributed",
        None,
        "task",
        Some(&format!("{}/{}", parent_id, "spawn-step")),
        Some(parent_id),
        Some("spawn-step"),
        None,
    )
    .await?;

    let steps = vec![make_step(child_id, "run", "ready")];
    JobStepRepo::create_steps_tx(&mut *tx, &steps).await?;

    tx.commit().await?;

    // Verify child job exists with correct parent columns.
    let child = JobRepo::get(&pool, child_id)
        .await?
        .expect("child job must exist after commit");
    assert_eq!(child.parent_job_id, Some(parent_id));
    assert_eq!(child.parent_step_name.as_deref(), Some("spawn-step"));
    assert_eq!(child.task_name, "child-tx");

    // Verify steps were also committed.
    let job_steps = JobStepRepo::get_steps_for_job(&pool, child_id).await?;
    assert_eq!(job_steps.len(), 1);
    assert_eq!(job_steps[0].step_name, "run");

    Ok(())
}

/// Rolling back a transaction that created a child job and its steps leaves
/// neither row visible outside the transaction.
#[tokio::test]
async fn test_transaction_rollback_discards_child_job_and_steps() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let parent_id = make_job(&pool, "parent-rollback").await?;
    let child_id = Uuid::new_v4();

    let mut tx = pool.begin().await?;

    JobRepo::create_with_parent_tx_id(
        &mut *tx,
        child_id,
        "default",
        "child-rollback",
        "distributed",
        None,
        "task",
        None,
        Some(parent_id),
        Some("spawn-step"),
        None,
    )
    .await?;

    let steps = vec![make_step(child_id, "run", "ready")];
    JobStepRepo::create_steps_tx(&mut *tx, &steps).await?;

    // Verify visibility within the transaction.
    let row = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM job WHERE job_id = $1")
        .bind(child_id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(row.0, 1, "child job must be visible inside the transaction");

    let row = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM job_step WHERE job_id = $1")
        .bind(child_id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(row.0, 1, "step must be visible inside the transaction");

    // Roll back.
    tx.rollback().await?;

    // Verify neither row is visible outside.
    let child = JobRepo::get(&pool, child_id).await?;
    assert!(child.is_none(), "child job must not exist after rollback");

    let job_steps = JobStepRepo::get_steps_for_job(&pool, child_id).await?;
    assert!(job_steps.is_empty(), "steps must not exist after rollback");

    Ok(())
}

// ─── `all_steps_terminal` with mixed terminal statuses ───────────────

/// `all_steps_terminal` returns true when every step is in one of the three
/// terminal states: completed, failed, or skipped (mixed).
#[tokio::test]
async fn test_all_steps_terminal_with_mixed_terminal_states() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "mixed-terminal").await?;
    let steps = vec![
        make_step(job_id, "step1", "ready"),
        make_step(job_id, "step2", "ready"),
        make_step(job_id, "step3", "pending"),
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Not terminal yet.
    assert!(!JobStepRepo::all_steps_terminal(&pool, job_id).await?);

    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;
    assert!(!JobStepRepo::all_steps_terminal(&pool, job_id).await?);

    JobStepRepo::mark_failed(&pool, job_id, "step2", "boom").await?;
    assert!(!JobStepRepo::all_steps_terminal(&pool, job_id).await?);

    JobStepRepo::mark_skipped(&pool, job_id, "step3").await?;
    // Now all three terminal statuses are represented.
    assert!(
        JobStepRepo::all_steps_terminal(&pool, job_id).await?,
        "job should be terminal when all steps are completed/failed/skipped"
    );

    Ok(())
}

/// `get_failed_step_names` returns only the names of failed steps, not
/// completed or skipped ones.
#[tokio::test]
async fn test_get_failed_step_names_filters_correctly() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "failed-names").await?;
    let steps = vec![
        make_step(job_id, "step1", "ready"),
        make_step(job_id, "step2", "ready"),
        make_step(job_id, "step3", "ready"),
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    JobStepRepo::mark_failed(&pool, job_id, "step1", "err").await?;
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    JobStepRepo::mark_skipped(&pool, job_id, "step3").await?;

    let mut failed = JobStepRepo::get_failed_step_names(&pool, job_id).await?;
    failed.sort();
    assert_eq!(failed, vec!["step1".to_string()]);

    Ok(())
}

/// `get_failed_step_names` returns an empty vec when no steps have failed.
#[tokio::test]
async fn test_get_failed_step_names_empty_when_no_failures() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = make_job(&pool, "no-failures").await?;
    JobStepRepo::create_steps(&pool, &[make_step(job_id, "step1", "ready")]).await?;

    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;

    let failed = JobStepRepo::get_failed_step_names(&pool, job_id).await?;
    assert!(failed.is_empty());

    Ok(())
}
