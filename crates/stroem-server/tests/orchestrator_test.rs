//! Integration tests for the orchestrator state machine (`on_step_completed`).
//!
//! These tests exercise the core DAG execution logic — step promotion, skip
//! propagation, and terminal-state detection — directly against a real
//! Postgres database (via testcontainers), without going through the HTTP
//! layer.  Each test spins up its own isolated container so they can run
//! fully in parallel.

use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep, WorkerRepo};
use stroem_server::orchestrator::on_step_completed;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Start a fresh Postgres container, run all migrations, and return the pool.
/// The container is returned too so it lives for the duration of the test.
async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

/// Register a worker so that `mark_running` foreign-key checks pass.
async fn register_worker(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    WorkerRepo::register(pool, id, "test-worker", &["script".to_string()], None)
        .await
        .expect("worker registration");
    id
}

/// Create a job and return its ID.
async fn create_job(pool: &PgPool) -> Uuid {
    JobRepo::create(
        pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await
    .expect("job creation")
}

/// Build a `NewJobStep` with sensible defaults for orchestrator tests.
fn step(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "noop".to_string(),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "true"})),
        input: None,
        status: status.to_string(),
        required_tags: vec!["script".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
    }
}

/// Build a `TaskDef` whose `flow` is supplied by the caller.
fn make_task(flow: HashMap<String, FlowStep>) -> TaskDef {
    TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,
        on_success: vec![],
        on_error: vec![],
    }
}

/// Build a `FlowStep` with no dependencies and `continue_on_failure = false`.
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
        inline_action: None,
    }
}

/// Build a `FlowStep` with `continue_on_failure = true`.
fn flow_step_cof(depends_on: Vec<&str>) -> FlowStep {
    FlowStep {
        continue_on_failure: true,
        ..flow_step(depends_on)
    }
}

/// Build a `FlowStep` with a `when` condition expression.
fn flow_step_when(depends_on: Vec<&str>, when_expr: &str) -> FlowStep {
    FlowStep {
        when: Some(when_expr.to_string()),
        ..flow_step(depends_on)
    }
}

/// Build a `NewJobStep` with a `when_condition` set.
fn step_when(job_id: Uuid, name: &str, status: &str, when_expr: &str) -> NewJobStep {
    NewJobStep {
        when_condition: Some(when_expr.to_string()),
        ..step(job_id, name, status)
    }
}

/// Collect step statuses for a job, keyed by step name.
async fn step_statuses(pool: &PgPool, job_id: Uuid) -> HashMap<String, String> {
    JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .expect("get steps")
        .into_iter()
        .map(|s| (s.step_name, s.status))
        .collect()
}

// ─── Test 1: Linear DAG (A → B → C) ────────────────────────────────────────

/// Completing A promotes B to ready; completing B promotes C; completing C
/// closes the job as "completed".
#[tokio::test]
async fn test_linear_dag_step_promotion() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step(vec!["a"]));
    flow.insert("c".to_string(), flow_step(vec!["b"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step(job_id, "b", "pending"),
            step(job_id, "c", "pending"),
        ],
    )
    .await?;

    // Complete A → B should become ready
    JobStepRepo::mark_completed(&pool, job_id, "a", None).await?;
    on_step_completed(&pool, job_id, "a", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["b"], "ready",
        "B must be promoted after A completes"
    );
    assert_eq!(statuses["c"], "pending", "C must still be pending");

    // Job is not terminal yet
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "pending");

    // Complete B → C should become ready
    JobStepRepo::mark_completed(&pool, job_id, "b", None).await?;
    on_step_completed(&pool, job_id, "b", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["c"], "ready",
        "C must be promoted after B completes"
    );

    // Complete C → job should complete
    JobStepRepo::mark_completed(&pool, job_id, "c", None).await?;
    on_step_completed(&pool, job_id, "c", &task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 2: Parallel DAG (A, B → C) ────────────────────────────────────────

/// C must not be promoted until both A and B have completed.
#[tokio::test]
async fn test_parallel_dag_fan_in() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step(vec![]));
    flow.insert("c".to_string(), flow_step(vec!["a", "b"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step(job_id, "b", "ready"),
            step(job_id, "c", "pending"),
        ],
    )
    .await?;

    // Complete A — C must still be pending because B is not done
    JobStepRepo::mark_completed(&pool, job_id, "a", None).await?;
    on_step_completed(&pool, job_id, "a", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["c"], "pending",
        "C must stay pending while B is outstanding"
    );

    // Complete B — now C must be promoted
    JobStepRepo::mark_completed(&pool, job_id, "b", None).await?;
    on_step_completed(&pool, job_id, "b", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["c"], "ready",
        "C must be promoted once both A and B complete"
    );

    // Complete C → job complete
    JobStepRepo::mark_completed(&pool, job_id, "c", None).await?;
    on_step_completed(&pool, job_id, "c", &task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 3: Failed step blocks dependents ───────────────────────────────────

/// When A fails its dependent B must be skipped and the job must end as
/// "failed".
#[tokio::test]
async fn test_failed_step_skips_dependents() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let worker_id = register_worker(&pool).await;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step(vec!["a"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(job_id, "a", "ready"), step(job_id, "b", "pending")],
    )
    .await?;

    JobStepRepo::mark_running(&pool, job_id, "a", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "a", "exit code 1").await?;
    on_step_completed(&pool, job_id, "a", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["b"], "skipped", "B must be skipped when A fails");

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test 4: continue_on_failure promotes dependent despite failure ───────────

/// `continue_on_failure` is a property of a step B that says "run me even if
/// my dependency A failed".  When A fails and B has `continue_on_failure:
/// true`, B must be promoted to ready rather than skipped.
#[tokio::test]
async fn test_continue_on_failure_promotes_dependent() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let worker_id = register_worker(&pool).await;

    let mut flow = HashMap::new();
    // A is a normal step (no continue_on_failure)
    flow.insert("a".to_string(), flow_step(vec![]));
    // B opts in to running even when A failed
    flow.insert("b".to_string(), flow_step_cof(vec!["a"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(job_id, "a", "ready"), step(job_id, "b", "pending")],
    )
    .await?;

    JobStepRepo::mark_running(&pool, job_id, "a", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "a", "non-fatal error").await?;
    on_step_completed(&pool, job_id, "a", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["b"], "ready",
        "B must be promoted when A has continue_on_failure=true"
    );

    // Job is not terminal yet (B is pending execution)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "pending");

    Ok(())
}

// ─── Test 5: All steps completed → job status = "completed" ─────────────────

/// A single-step job whose step completes must transition the job to
/// "completed" with the aggregated output.
#[tokio::test]
async fn test_all_steps_completed_job_completes() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("only".to_string(), flow_step(vec![]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "only", "ready")]).await?;

    let output = json!({"result": 42});
    JobStepRepo::mark_completed(&pool, job_id, "only", Some(output.clone())).await?;
    on_step_completed(&pool, job_id, "only", &task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    // Terminal step output is aggregated into job output
    assert_eq!(job.output, Some(json!({"only": {"result": 42}})));

    Ok(())
}

// ─── Test 6: Mix of completed and failed with no path forward → "failed" ─────

/// When one step completes and another independently fails (and no steps
/// remain pending), the job must be "failed".
#[tokio::test]
async fn test_mix_completed_and_failed_job_fails() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let worker_id = register_worker(&pool).await;

    // Two independent steps: ok and bad.  bad has no continue_on_failure.
    let mut flow = HashMap::new();
    flow.insert("ok".to_string(), flow_step(vec![]));
    flow.insert("bad".to_string(), flow_step(vec![]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(job_id, "ok", "ready"), step(job_id, "bad", "ready")],
    )
    .await?;

    // Complete the ok step
    JobStepRepo::mark_completed(&pool, job_id, "ok", None).await?;
    on_step_completed(&pool, job_id, "ok", &task, None).await?;

    // Job is still running (bad step is outstanding)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "pending");

    // Fail the bad step
    JobStepRepo::mark_running(&pool, job_id, "bad", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "bad", "unexpected error").await?;
    on_step_completed(&pool, job_id, "bad", &task, None).await?;

    // Now all steps are terminal and bad failed without continue_on_failure
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test 7: Cascading skip through multiple layers ──────────────────────────

/// When the root of a chain fails, every downstream step — even those two
/// hops away — must be skipped in a single orchestrator call.
#[tokio::test]
async fn test_cascading_skip_multi_level() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let worker_id = register_worker(&pool).await;

    // Chain: a → b → c
    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step(vec!["a"]));
    flow.insert("c".to_string(), flow_step(vec!["b"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step(job_id, "b", "pending"),
            step(job_id, "c", "pending"),
        ],
    )
    .await?;

    JobStepRepo::mark_running(&pool, job_id, "a", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "a", "root failure").await?;
    on_step_completed(&pool, job_id, "a", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["b"], "skipped");
    assert_eq!(statuses["c"], "skipped");

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test 8: continue_on_failure — only tolerable failures → job completes ───

/// When every failed step has `continue_on_failure: true`, the job should
/// end as "completed", not "failed".
#[tokio::test]
async fn test_all_tolerable_failures_job_completes() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let worker_id = register_worker(&pool).await;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step_cof(vec![]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "ready")]).await?;

    JobStepRepo::mark_running(&pool, job_id, "a", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "a", "tolerable error").await?;
    on_step_completed(&pool, job_id, "a", &task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "completed",
        "job with only tolerable failures must complete, not fail"
    );

    Ok(())
}

// ─── Test 9: Diamond DAG — join step promoted only when both parents done ─────

/// Classic diamond: root → left, root → right, left+right → join.
/// The join step must stay pending until both branches complete.
#[tokio::test]
async fn test_diamond_dag_join_waits_for_both_branches() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("root".to_string(), flow_step(vec![]));
    flow.insert("left".to_string(), flow_step(vec!["root"]));
    flow.insert("right".to_string(), flow_step(vec!["root"]));
    flow.insert("join".to_string(), flow_step(vec!["left", "right"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "root", "ready"),
            step(job_id, "left", "pending"),
            step(job_id, "right", "pending"),
            step(job_id, "join", "pending"),
        ],
    )
    .await?;

    // Complete root — left and right promoted, join still pending
    JobStepRepo::mark_completed(&pool, job_id, "root", None).await?;
    on_step_completed(&pool, job_id, "root", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["left"], "ready");
    assert_eq!(statuses["right"], "ready");
    assert_eq!(statuses["join"], "pending");

    // Complete left — join still needs right
    JobStepRepo::mark_completed(&pool, job_id, "left", None).await?;
    on_step_completed(&pool, job_id, "left", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["join"], "pending");

    // Complete right — join is now ready
    JobStepRepo::mark_completed(&pool, job_id, "right", None).await?;
    on_step_completed(&pool, job_id, "right", &task, None).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["join"], "ready");

    // Complete join → job complete
    JobStepRepo::mark_completed(&pool, job_id, "join", None).await?;
    on_step_completed(&pool, job_id, "join", &task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 10: Conditional step promoted when condition is true ────────────────

/// When step A completes with `{"proceed": true}`, step B (which has
/// `when: "{{ a.output.proceed }}"`) must be promoted to ready.
#[tokio::test]
async fn test_conditional_step_promoted_when_condition_true() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert(
        "b".to_string(),
        flow_step_when(vec!["a"], "{{ a.output.proceed }}"),
    );
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step_when(job_id, "b", "pending", "{{ a.output.proceed }}"),
        ],
    )
    .await?;

    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_completed(&pool, job_id, "a", Some(json!({"proceed": true}))).await?;
    on_step_completed(&pool, job_id, "a", &task, Some(&ws)).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["b"], "ready",
        "B must be promoted when condition evaluates to true"
    );

    Ok(())
}

// ─── Test 11: Conditional step skipped when condition is false ────────────────

/// When step A completes with `{"proceed": false}`, step B (which has
/// `when: "{{ a.output.proceed }}"`) must be skipped and the job must complete.
#[tokio::test]
async fn test_conditional_step_skipped_when_condition_false() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert(
        "b".to_string(),
        flow_step_when(vec!["a"], "{{ a.output.proceed }}"),
    );
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step_when(job_id, "b", "pending", "{{ a.output.proceed }}"),
        ],
    )
    .await?;

    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_completed(&pool, job_id, "a", Some(json!({"proceed": false}))).await?;
    on_step_completed(&pool, job_id, "a", &task, Some(&ws)).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["b"], "skipped",
        "B must be skipped when condition evaluates to false"
    );

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "completed",
        "job must complete when only remaining step is skipped"
    );

    Ok(())
}

// ─── Test 12: Cascade — conditional skip propagates to downstream ─────────────

/// Steps: A (ready), B (pending, depends on A, `when: "{{ a.output.go }}"`),
/// C (pending, depends on B).  When A completes with `{"go": false}`, B must
/// be skipped by condition evaluation and C must be skipped as unreachable.
#[tokio::test]
async fn test_conditional_skip_cascades_to_downstream() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert(
        "b".to_string(),
        flow_step_when(vec!["a"], "{{ a.output.go }}"),
    );
    flow.insert("c".to_string(), flow_step(vec!["b"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step_when(job_id, "b", "pending", "{{ a.output.go }}"),
            step(job_id, "c", "pending"),
        ],
    )
    .await?;

    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_completed(&pool, job_id, "a", Some(json!({"go": false}))).await?;
    on_step_completed(&pool, job_id, "a", &task, Some(&ws)).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["b"], "skipped", "B must be skipped by condition");
    assert_eq!(
        statuses["c"], "skipped",
        "C must be skipped because its dependency B was skipped"
    );

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "completed",
        "job must complete when all remaining steps are skipped"
    );

    Ok(())
}

// ─── Test 13: All conditional steps false → job completes ────────────────────

/// Steps: A (ready, no when), B (pending, depends on A, `when: "false"`),
/// C (pending, depends on A, `when: "false"`).  When A completes, both B and C
/// must be skipped and the job must complete.
#[tokio::test]
async fn test_all_conditional_steps_false_job_completes() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step_when(vec!["a"], "false"));
    flow.insert("c".to_string(), flow_step_when(vec!["a"], "false"));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step_when(job_id, "b", "pending", "false"),
            step_when(job_id, "c", "pending", "false"),
        ],
    )
    .await?;

    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_completed(&pool, job_id, "a", None).await?;
    on_step_completed(&pool, job_id, "a", &task, Some(&ws)).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["b"], "skipped", "B must be skipped (when: false)");
    assert_eq!(statuses["c"], "skipped", "C must be skipped (when: false)");

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "completed",
        "job must complete when all remaining steps are skipped"
    );

    Ok(())
}

// ─── Test 14: continue_on_failure with skipped dep + truthy when ──────────────

/// Steps: A (ready), B (pending, depends on A, `when: "{{ a.output.deploy }}"`),
/// C (pending, depends on B, `continue_on_failure: true`, `when: "true"`).
/// When A completes with `{"deploy": false}`, B is skipped by condition.
/// C has continue_on_failure, so a skipped B still satisfies its deps; and
/// `when: "true"` is truthy, so C must be promoted to ready.
#[tokio::test]
async fn test_continue_on_failure_accepts_skipped_dep_with_truthy_when() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert(
        "b".to_string(),
        flow_step_when(vec!["a"], "{{ a.output.deploy }}"),
    );
    flow.insert(
        "c".to_string(),
        FlowStep {
            continue_on_failure: true,
            when: Some("true".to_string()),
            ..flow_step(vec!["b"])
        },
    );
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step_when(job_id, "b", "pending", "{{ a.output.deploy }}"),
            step_when(job_id, "c", "pending", "true"),
        ],
    )
    .await?;

    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_completed(&pool, job_id, "a", Some(json!({"deploy": false}))).await?;
    on_step_completed(&pool, job_id, "a", &task, Some(&ws)).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(
        statuses["b"], "skipped",
        "B must be skipped when condition is false"
    );
    assert_eq!(
        statuses["c"], "ready",
        "C must be promoted: continue_on_failure accepts skipped dep and when:true is truthy"
    );

    Ok(())
}
