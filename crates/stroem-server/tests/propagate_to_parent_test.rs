//! Integration tests for `propagate_to_parent` — child job completion propagating
//! to parent jobs via the public `orchestrate_after_step` / `handle_job_terminal` API.
//!
//! Each test spins up its own isolated Postgres container so they can run fully
//! in parallel.

use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{ActionDef, FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep, WorkerRepo};
use stroem_server::config::{DbConfig, LogStorageConfig, RecoveryConfig, ServerConfig};
use stroem_server::job_recovery::{handle_job_terminal, orchestrate_after_step};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

// ─── Helpers ────────────────────────────────────────────────────────────────

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

fn setup_state(
    pool: PgPool,
    workspace_config: WorkspaceConfig,
    log_dir: &std::path::Path,
) -> AppState {
    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: "postgres://unused".to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test".to_string(),
        auth: None,
        recovery: RecoveryConfig {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 60,
        },
    };
    let mgr = WorkspaceManager::from_config("default", workspace_config);
    let log_storage = LogStorage::new(log_dir);
    AppState::new(pool, mgr, config, log_storage, HashMap::new())
}

/// Build a minimal workspace config with parent-task and child-task.
///
/// - parent-task: one step "run-child" of type task referencing "child-task"
/// - child-task: one shell step "do-work" using the noop action
fn make_workspace_config() -> WorkspaceConfig {
    let mut actions = HashMap::new();
    actions.insert(
        "noop".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: Some("true".to_string()),
            script: None,
            runner: None,
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );
    actions.insert(
        "run-child-action".to_string(),
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some("child-task".to_string()),
            cmd: None,
            script: None,
            runner: None,
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    let mut parent_flow = HashMap::new();
    parent_flow.insert(
        "run-child".to_string(),
        FlowStep {
            action: "run-child-action".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );

    let mut child_flow = HashMap::new();
    child_flow.insert(
        "do-work".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );

    let mut tasks = HashMap::new();
    tasks.insert(
        "parent-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: parent_flow,
            on_success: vec![],
            on_error: vec![],
        },
    );
    tasks.insert(
        "child-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: child_flow,
            on_success: vec![],
            on_error: vec![],
        },
    );

    WorkspaceConfig {
        actions,
        tasks,
        triggers: HashMap::new(),
        secrets: HashMap::new(),
        connections: HashMap::new(),
        connection_types: HashMap::new(),
        on_success: vec![],
        on_error: vec![],
    }
}

/// A NewJobStep with task-type action (the parent's "run-child" step).
fn task_step(job_id: Uuid, name: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "run-child-action".to_string(),
        action_type: "task".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: "running".to_string(),
        required_tags: vec![],
        runner: "none".to_string(),
    }
}

/// A NewJobStep with shell-type action (the child's "do-work" step).
fn shell_step(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "noop".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "true"})),
        input: None,
        status: status.to_string(),
        required_tags: vec!["shell".to_string()],
        runner: "local".to_string(),
    }
}

/// Register a worker (needed for `mark_running` FK checks).
async fn register_worker(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    WorkerRepo::register(
        pool,
        id,
        "test-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await
    .expect("worker registration");
    id
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

// ─── Test 1: child_completed_propagates_to_parent ────────────────────────────

/// When a child job's sole step completes, the parent's task step must be
/// marked completed and the parent job must reach "completed" status.
#[tokio::test]
async fn child_completed_propagates_to_parent() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let temp_dir = TempDir::new()?;
    let state = setup_state(pool.clone(), make_workspace_config(), temp_dir.path());

    let parent_job_id = JobRepo::create(
        &pool,
        "default",
        "parent-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;
    let child_job_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&format!("{}/run-child", parent_job_id)),
        Some(parent_job_id),
        Some("run-child"),
    )
    .await?;

    // Parent has task step in "running" (server dispatched it)
    JobStepRepo::create_steps(&pool, &[task_step(parent_job_id, "run-child")]).await?;
    // Child has shell step in "ready"
    JobStepRepo::create_steps(&pool, &[shell_step(child_job_id, "do-work", "ready")]).await?;

    // Complete the child step
    let output = json!({"result": "success"});
    JobStepRepo::mark_completed(&pool, child_job_id, "do-work", Some(output)).await?;

    // Orchestrate the child job — should propagate to parent
    orchestrate_after_step(&state, child_job_id, "do-work").await?;

    // Parent step should be completed
    let parent_statuses = step_statuses(&pool, parent_job_id).await;
    assert_eq!(
        parent_statuses["run-child"], "completed",
        "parent task step must be completed after child job completes"
    );

    // Parent job should be completed
    let parent_job = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(
        parent_job.status, "completed",
        "parent job must reach completed status"
    );

    Ok(())
}

// ─── Test 2: child_failed_propagates_to_parent ───────────────────────────────

/// When a child job's step fails, the parent's task step must be marked failed
/// and the parent job must reach "failed" status.
#[tokio::test]
async fn child_failed_propagates_to_parent() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let temp_dir = TempDir::new()?;
    let state = setup_state(pool.clone(), make_workspace_config(), temp_dir.path());
    let worker_id = register_worker(&pool).await;

    let parent_job_id = JobRepo::create(
        &pool,
        "default",
        "parent-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;
    let child_job_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&format!("{}/run-child", parent_job_id)),
        Some(parent_job_id),
        Some("run-child"),
    )
    .await?;

    JobStepRepo::create_steps(&pool, &[task_step(parent_job_id, "run-child")]).await?;
    JobStepRepo::create_steps(&pool, &[shell_step(child_job_id, "do-work", "ready")]).await?;

    // Fail the child step (must go through running first)
    JobStepRepo::mark_running(&pool, child_job_id, "do-work", worker_id).await?;
    JobStepRepo::mark_failed(&pool, child_job_id, "do-work", "exit code 1").await?;

    // Orchestrate the child job
    orchestrate_after_step(&state, child_job_id, "do-work").await?;

    // Parent step should be failed
    let parent_statuses = step_statuses(&pool, parent_job_id).await;
    assert_eq!(
        parent_statuses["run-child"], "failed",
        "parent task step must be failed after child job fails"
    );

    // Parent job should be failed
    let parent_job = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(
        parent_job.status, "failed",
        "parent job must reach failed status"
    );

    Ok(())
}

// ─── Test 3: child_cancelled_propagates_to_parent ────────────────────────────

/// When a child job is cancelled, the parent's task step must be marked
/// cancelled via `handle_job_terminal`.
#[tokio::test]
async fn child_cancelled_propagates_to_parent() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let temp_dir = TempDir::new()?;
    let state = setup_state(pool.clone(), make_workspace_config(), temp_dir.path());
    let worker_id = register_worker(&pool).await;

    let parent_job_id = JobRepo::create(
        &pool,
        "default",
        "parent-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;
    let child_job_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&format!("{}/run-child", parent_job_id)),
        Some(parent_job_id),
        Some("run-child"),
    )
    .await?;

    JobStepRepo::create_steps(&pool, &[task_step(parent_job_id, "run-child")]).await?;
    JobStepRepo::create_steps(&pool, &[shell_step(child_job_id, "do-work", "ready")]).await?;

    // Mark the child step as running, then cancelled
    JobStepRepo::mark_running(&pool, child_job_id, "do-work", worker_id).await?;
    JobStepRepo::mark_cancelled(&pool, child_job_id, "do-work").await?;

    // Mark the child job itself as cancelled directly (simulates cancel API)
    JobRepo::cancel(&pool, child_job_id).await?;

    // Fire handle_job_terminal — this propagates cancellation to parent
    handle_job_terminal(&state, child_job_id).await?;

    // Parent step should be cancelled
    let parent_statuses = step_statuses(&pool, parent_job_id).await;
    assert_eq!(
        parent_statuses["run-child"], "cancelled",
        "parent task step must be cancelled after child job is cancelled"
    );

    Ok(())
}

// ─── Test 4: deep_nesting_three_levels ───────────────────────────────────────

/// Grandparent → parent → child: completing the grandchild propagates all the
/// way up so that both the parent and grandparent jobs reach "completed".
#[tokio::test]
async fn deep_nesting_three_levels() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let temp_dir = TempDir::new()?;

    // Build a workspace with three tasks:
    //   grandparent-task: one task step "run-parent" → parent-task
    //   parent-task:      one task step "run-child"  → child-task
    //   child-task:       one shell step "do-work"
    let mut actions = HashMap::new();
    actions.insert(
        "noop".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: Some("true".to_string()),
            script: None,
            runner: None,
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );
    for (action_name, task_name) in &[
        ("run-parent-action", "parent-task"),
        ("run-child-action", "child-task"),
    ] {
        actions.insert(
            action_name.to_string(),
            ActionDef {
                action_type: "task".to_string(),
                name: None,
                description: None,
                task: Some(task_name.to_string()),
                cmd: None,
                script: None,
                runner: None,
                tags: vec![],
                image: None,
                command: None,
                entrypoint: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
                manifest: None,
            },
        );
    }

    let mut tasks = HashMap::new();

    let mut gp_flow = HashMap::new();
    gp_flow.insert(
        "run-parent".to_string(),
        FlowStep {
            action: "run-parent-action".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );
    tasks.insert(
        "grandparent-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: gp_flow,
            on_success: vec![],
            on_error: vec![],
        },
    );

    let mut p_flow = HashMap::new();
    p_flow.insert(
        "run-child".to_string(),
        FlowStep {
            action: "run-child-action".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );
    tasks.insert(
        "parent-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: p_flow,
            on_success: vec![],
            on_error: vec![],
        },
    );

    let mut c_flow = HashMap::new();
    c_flow.insert(
        "do-work".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );
    tasks.insert(
        "child-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: c_flow,
            on_success: vec![],
            on_error: vec![],
        },
    );

    let ws_config = WorkspaceConfig {
        actions,
        tasks,
        triggers: HashMap::new(),
        secrets: HashMap::new(),
        connections: HashMap::new(),
        connection_types: HashMap::new(),
        on_success: vec![],
        on_error: vec![],
    };

    let state = setup_state(pool.clone(), ws_config, temp_dir.path());

    // Create grandparent job
    let gp_job_id = JobRepo::create(
        &pool,
        "default",
        "grandparent-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create parent job as child of grandparent
    let p_job_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "parent-task",
        "distributed",
        None,
        "task",
        Some(&format!("{}/run-parent", gp_job_id)),
        Some(gp_job_id),
        Some("run-parent"),
    )
    .await?;

    // Create child job as child of parent
    let c_job_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&format!("{}/run-child", p_job_id)),
        Some(p_job_id),
        Some("run-child"),
    )
    .await?;

    // Set up steps
    // Grandparent: "run-parent" task step (running — dispatched by server)
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id: gp_job_id,
            step_name: "run-parent".to_string(),
            action_name: "run-parent-action".to_string(),
            action_type: "task".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "running".to_string(),
            required_tags: vec![],
            runner: "none".to_string(),
        }],
    )
    .await?;

    // Parent: "run-child" task step (running)
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id: p_job_id,
            step_name: "run-child".to_string(),
            action_name: "run-child-action".to_string(),
            action_type: "task".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "running".to_string(),
            required_tags: vec![],
            runner: "none".to_string(),
        }],
    )
    .await?;

    // Child: "do-work" shell step (ready)
    JobStepRepo::create_steps(&pool, &[shell_step(c_job_id, "do-work", "ready")]).await?;

    // Complete the child step
    JobStepRepo::mark_completed(&pool, c_job_id, "do-work", None).await?;

    // Orchestrate child job — should propagate all the way up
    orchestrate_after_step(&state, c_job_id, "do-work").await?;

    // Parent step should be completed
    let p_statuses = step_statuses(&pool, p_job_id).await;
    assert_eq!(
        p_statuses["run-child"], "completed",
        "parent run-child step must be completed"
    );

    // Parent job should be completed
    let p_job = JobRepo::get(&pool, p_job_id).await?.unwrap();
    assert_eq!(p_job.status, "completed", "parent job must be completed");

    // Grandparent step should be completed
    let gp_statuses = step_statuses(&pool, gp_job_id).await;
    assert_eq!(
        gp_statuses["run-parent"], "completed",
        "grandparent run-parent step must be completed"
    );

    // Grandparent job should be completed
    let gp_job = JobRepo::get(&pool, gp_job_id).await?.unwrap();
    assert_eq!(
        gp_job.status, "completed",
        "grandparent job must be completed"
    );

    Ok(())
}

// ─── Test 5: parent_with_mixed_steps ─────────────────────────────────────────

/// A parent job with a shell step followed by a task step: completing the
/// shell step promotes the task step, completing the child job completes parent.
#[tokio::test]
async fn parent_with_mixed_steps() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let temp_dir = TempDir::new()?;

    // Build workspace config with a parent that has both a shell and task step
    let mut actions = HashMap::new();
    actions.insert(
        "noop".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: Some("true".to_string()),
            script: None,
            runner: None,
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );
    actions.insert(
        "run-child-action".to_string(),
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some("child-task".to_string()),
            cmd: None,
            script: None,
            runner: None,
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    let mut parent_flow = HashMap::new();
    parent_flow.insert(
        "shell-step".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );
    parent_flow.insert(
        "task-step".to_string(),
        FlowStep {
            action: "run-child-action".to_string(),
            name: None,
            description: None,
            depends_on: vec!["shell-step".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );

    let mut child_flow = HashMap::new();
    child_flow.insert(
        "do-work".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );

    let mut tasks = HashMap::new();
    tasks.insert(
        "parent-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: parent_flow,
            on_success: vec![],
            on_error: vec![],
        },
    );
    tasks.insert(
        "child-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: child_flow,
            on_success: vec![],
            on_error: vec![],
        },
    );

    let ws_config = WorkspaceConfig {
        actions,
        tasks,
        triggers: HashMap::new(),
        secrets: HashMap::new(),
        connections: HashMap::new(),
        connection_types: HashMap::new(),
        on_success: vec![],
        on_error: vec![],
    };

    let state = setup_state(pool.clone(), ws_config, temp_dir.path());

    // Create parent job
    let parent_job_id = JobRepo::create(
        &pool,
        "default",
        "parent-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create parent steps: shell-step (ready), task-step (pending — awaits shell-step)
    JobStepRepo::create_steps(
        &pool,
        &[
            shell_step(parent_job_id, "shell-step", "ready"),
            NewJobStep {
                job_id: parent_job_id,
                step_name: "task-step".to_string(),
                action_name: "run-child-action".to_string(),
                action_type: "task".to_string(),
                action_image: None,
                action_spec: None,
                input: None,
                status: "pending".to_string(),
                required_tags: vec![],
                runner: "none".to_string(),
            },
        ],
    )
    .await?;

    // Complete the shell step — orchestrator should promote task-step to ready
    JobStepRepo::mark_completed(&pool, parent_job_id, "shell-step", None).await?;
    orchestrate_after_step(&state, parent_job_id, "shell-step").await?;

    let statuses = step_statuses(&pool, parent_job_id).await;
    assert_eq!(
        statuses["task-step"], "ready",
        "task-step must be promoted to ready after shell-step completes"
    );

    // Simulate server dispatching the task step by marking it "running"
    JobStepRepo::mark_running_server(&pool, parent_job_id, "task-step").await?;

    // Create the child job for that task-step
    let child_job_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&format!("{}/task-step", parent_job_id)),
        Some(parent_job_id),
        Some("task-step"),
    )
    .await?;

    JobStepRepo::create_steps(&pool, &[shell_step(child_job_id, "do-work", "ready")]).await?;

    // Complete the child job's work
    JobStepRepo::mark_completed(&pool, child_job_id, "do-work", None).await?;
    orchestrate_after_step(&state, child_job_id, "do-work").await?;

    // Parent task-step should be completed
    let statuses = step_statuses(&pool, parent_job_id).await;
    assert_eq!(
        statuses["task-step"], "completed",
        "parent task-step must be completed after child job completes"
    );

    // Parent job should be completed
    let parent_job = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(
        parent_job.status, "completed",
        "parent job must reach completed status when all steps finish"
    );

    Ok(())
}
