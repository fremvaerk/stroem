use anyhow::Result;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::FlowStep;
use stroem_db::{
    run_migrations, JobRepo, JobStepRepo, NewJobStep, RefreshTokenRepo, UserAuthLinkRepo, UserRepo,
    WorkerRepo,
};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    // Use a lightweight pool for tests: no eager min_connections (avoids
    // timeouts when many containers start simultaneously under Docker pressure),
    // and a longer acquire timeout to tolerate slow container startup.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&url)
        .await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

#[tokio::test]
async fn test_create_and_get_job() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create a job
    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        Some(serde_json::json!({"name": "test"})),
        "user",
        None,
    )
    .await?;

    // Retrieve it
    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist");

    // Check fields
    assert_eq!(job.workspace, "default");
    assert_eq!(job.task_name, "test-task");
    assert_eq!(job.mode, "distributed");
    assert_eq!(job.status, "pending");
    assert_eq!(job.source_type, "user");
    assert_eq!(job.input, Some(serde_json::json!({"name": "test"})));

    Ok(())
}

#[tokio::test]
async fn test_list_jobs() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create multiple jobs
    for i in 0..5 {
        JobRepo::create(
            &pool,
            "default",
            &format!("task-{}", i),
            "distributed",
            None,
            "api",
            None,
        )
        .await?;
    }

    // List with pagination
    let jobs = JobRepo::list(&pool, None, None, 3, 0).await?;
    assert_eq!(jobs.len(), 3);

    // List with offset
    let jobs = JobRepo::list(&pool, None, None, 3, 3).await?;
    assert_eq!(jobs.len(), 2);

    // List with workspace filter
    let jobs = JobRepo::list(&pool, Some("default"), None, 10, 0).await?;
    assert_eq!(jobs.len(), 5);

    // List by task name — single match
    let jobs = JobRepo::list_by_task(&pool, "default", "task-0", None, 10, 0).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].task_name, "task-0");

    // List by task name — nonexistent task
    let jobs = JobRepo::list_by_task(&pool, "default", "nope", None, 10, 0).await?;
    assert!(jobs.is_empty());

    // List by task name — wrong workspace
    let jobs = JobRepo::list_by_task(&pool, "other", "task-0", None, 10, 0).await?;
    assert!(jobs.is_empty());

    // List by task name — multiple jobs for same task, newest first
    for _ in 0..3 {
        JobRepo::create(&pool, "default", "task-0", "distributed", None, "api", None).await?;
    }
    let jobs = JobRepo::list_by_task(&pool, "default", "task-0", None, 10, 0).await?;
    assert_eq!(jobs.len(), 4); // 1 original + 3 new
                               // Verify newest-first ordering
    for pair in jobs.windows(2) {
        assert!(pair[0].created_at >= pair[1].created_at);
    }

    // Pagination on list_by_task
    let page1 = JobRepo::list_by_task(&pool, "default", "task-0", None, 2, 0).await?;
    assert_eq!(page1.len(), 2);
    let page2 = JobRepo::list_by_task(&pool, "default", "task-0", None, 2, 2).await?;
    assert_eq!(page2.len(), 2);
    let page3 = JobRepo::list_by_task(&pool, "default", "task-0", None, 2, 4).await?;
    assert!(page3.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_create_steps_and_claim() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Register a worker
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    // Create a job
    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    // Create steps
    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo hello"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo world"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];

    JobStepRepo::create_steps(&pool, &steps).await?;

    // Claim a ready step
    let claimed = JobStepRepo::claim_ready_step(&pool, &["shell".to_string()], worker_id)
        .await?
        .expect("Should claim a step");

    assert_eq!(claimed.step_name, "step1");
    assert_eq!(claimed.status, "running");
    assert_eq!(claimed.worker_id, Some(worker_id));

    // Try to claim again - should get None (no more ready steps)
    let claimed_again =
        JobStepRepo::claim_ready_step(&pool, &["shell".to_string()], worker_id).await?;
    assert!(claimed_again.is_none());

    Ok(())
}

#[tokio::test]
async fn test_claim_concurrency() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create a job with multiple ready steps
    let job_id = JobRepo::create(
        &pool,
        "default",
        "concurrent-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    let mut steps = Vec::new();
    for i in 0..10 {
        steps.push(NewJobStep {
            job_id,
            step_name: format!("step{}", i),
            action_name: "test-action".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo test"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        });
    }

    JobStepRepo::create_steps(&pool, &steps).await?;

    // Spawn multiple concurrent claims
    let mut handles = Vec::new();
    for i in 0..10 {
        let pool_clone = pool.clone();
        let worker_id = Uuid::new_v4();
        let handle = tokio::spawn(async move {
            // Register worker first
            WorkerRepo::register(
                &pool_clone,
                worker_id,
                &format!("worker-{}", i),
                &["shell".to_string()],
                &["shell".to_string()],
                None,
            )
            .await
            .unwrap();

            // Try to claim
            JobStepRepo::claim_ready_step(&pool_clone, &["shell".to_string()], worker_id).await
        });
        handles.push(handle);
    }

    // Wait for all claims
    let mut claimed_count = 0;
    let mut claimed_steps = Vec::new();
    for handle in handles {
        if let Ok(Ok(Some(step))) = handle.await {
            claimed_count += 1;
            claimed_steps.push(step.step_name.clone());
        }
    }

    // Verify exactly 10 steps were claimed (no double claims)
    assert_eq!(claimed_count, 10);

    // Verify no duplicates
    claimed_steps.sort();
    claimed_steps.dedup();
    assert_eq!(claimed_steps.len(), 10);

    Ok(())
}

#[tokio::test]
async fn test_step_lifecycle() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "lifecycle-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    // Create a ready step
    let steps = vec![NewJobStep {
        job_id,
        step_name: "test-step".to_string(),
        action_name: "test-action".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: Some(serde_json::json!({"cmd": "echo test"})),
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["shell".to_string()],
        runner: "local".to_string(),
    }];

    JobStepRepo::create_steps(&pool, &steps).await?;

    // Mark running
    JobStepRepo::mark_running(&pool, job_id, "test-step", worker_id).await?;

    let step = JobStepRepo::get_steps_for_job(&pool, job_id)
        .await?
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(step.status, "running");
    assert!(step.started_at.is_some());

    // Mark completed
    JobStepRepo::mark_completed(
        &pool,
        job_id,
        "test-step",
        Some(serde_json::json!({"result": "success"})),
    )
    .await?;

    let step = JobStepRepo::get_steps_for_job(&pool, job_id)
        .await?
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(step.status, "completed");
    assert!(step.completed_at.is_some());
    assert_eq!(step.output, Some(serde_json::json!({"result": "success"})));

    Ok(())
}

#[tokio::test]
async fn test_update_input() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "input-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    let steps = vec![NewJobStep {
        job_id,
        step_name: "render-step".to_string(),
        action_name: "action1".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["shell".to_string()],
        runner: "local".to_string(),
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Input should be null initially
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert!(steps[0].input.is_none());

    // Update input
    let rendered = serde_json::json!({"greeting": "hello world"});
    JobStepRepo::update_input(&pool, job_id, "render-step", Some(rendered.clone())).await?;

    // Verify input was persisted
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].input, Some(rendered));

    Ok(())
}

#[tokio::test]
async fn test_promote_ready_steps() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "dag-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    // Create a DAG: step1 -> step2 -> step3
    //                   \-> step4 /
    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step3".to_string(),
            action_name: "action3".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo 3"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step4".to_string(),
            action_name: "action4".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({"cmd": "echo 4"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];

    JobStepRepo::create_steps(&pool, &steps).await?;

    // Define flow
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "action1".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "action2".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );
    flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "action3".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step2".to_string(), "step4".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );
    flow.insert(
        "step4".to_string(),
        FlowStep {
            action: "action4".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            inline_action: None,
        },
    );

    // Complete step1
    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;

    // Promote ready steps
    let promoted = JobStepRepo::promote_ready_steps(&pool, job_id, &flow).await?;

    // step2 and step4 should be promoted
    assert_eq!(promoted.len(), 2);
    assert!(promoted.contains(&"step2".to_string()));
    assert!(promoted.contains(&"step4".to_string()));

    // Complete step2 and step4
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step4", None).await?;

    // Promote again
    let promoted = JobStepRepo::promote_ready_steps(&pool, job_id, &flow).await?;

    // step3 should now be promoted
    assert_eq!(promoted.len(), 1);
    assert!(promoted.contains(&"step3".to_string()));

    Ok(())
}

#[tokio::test]
async fn test_worker_register_and_heartbeat() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let worker_id = Uuid::new_v4();
    let capabilities = vec!["shell".to_string(), "docker".to_string()];

    // Register
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &capabilities,
        &capabilities,
        None,
    )
    .await?;

    // Get
    let worker = WorkerRepo::get(&pool, worker_id)
        .await?
        .expect("Worker should exist");
    assert_eq!(worker.name, "test-worker");
    assert_eq!(worker.status, "active");

    let first_heartbeat = worker.last_heartbeat;

    // Wait a bit
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Heartbeat
    WorkerRepo::heartbeat(&pool, worker_id).await?;

    // Verify heartbeat updated
    let worker_after = WorkerRepo::get(&pool, worker_id)
        .await?
        .expect("Worker should exist");
    assert!(worker_after.last_heartbeat > first_heartbeat);

    Ok(())
}

#[tokio::test]
async fn test_all_steps_terminal() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "terminal-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];

    JobStepRepo::create_steps(&pool, &steps).await?;

    // Not all terminal yet
    let all_terminal = JobStepRepo::all_steps_terminal(&pool, job_id).await?;
    assert!(!all_terminal);

    // Complete both
    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;

    // Now all terminal
    let all_terminal = JobStepRepo::all_steps_terminal(&pool, job_id).await?;
    assert!(all_terminal);

    Ok(())
}

#[tokio::test]
async fn test_any_step_failed() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "failure-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];

    JobStepRepo::create_steps(&pool, &steps).await?;

    // No failures yet
    let any_failed = JobStepRepo::any_step_failed(&pool, job_id).await?;
    assert!(!any_failed);

    // Fail one step
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Something went wrong").await?;

    // Now there's a failure
    let any_failed = JobStepRepo::any_step_failed(&pool, job_id).await?;
    assert!(any_failed);

    Ok(())
}

#[tokio::test]
async fn test_mark_failed_stores_error() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "error-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    let steps = vec![NewJobStep {
        job_id,
        step_name: "fail-step".to_string(),
        action_name: "action1".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["shell".to_string()],
        runner: "local".to_string(),
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Mark as failed with error message
    JobStepRepo::mark_failed(&pool, job_id, "fail-step", "Process exited with code 127").await?;

    // Verify error_message and status
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = &steps[0];
    assert_eq!(step.status, "failed");
    assert_eq!(
        step.error_message.as_deref(),
        Some("Process exited with code 127")
    );
    assert!(step.completed_at.is_some());

    Ok(())
}

#[tokio::test]
async fn test_job_status_transitions() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create job -> pending
    let job_id = JobRepo::create(
        &pool,
        "default",
        "transition-test",
        "distributed",
        Some(serde_json::json!({"key": "value"})),
        "user",
        None,
    )
    .await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "pending");
    assert!(job.started_at.is_none());
    assert!(job.completed_at.is_none());

    // Mark running
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;
    JobRepo::mark_running(&pool, job_id, worker_id).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "running");
    assert!(job.started_at.is_some());
    assert_eq!(job.worker_id, Some(worker_id));

    // Mark completed
    JobRepo::mark_completed(&pool, job_id, Some(serde_json::json!({"result": "ok"}))).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    assert!(job.completed_at.is_some());
    assert_eq!(job.output, Some(serde_json::json!({"result": "ok"})));

    // Test failed path
    let job_id2 = JobRepo::create(
        &pool,
        "default",
        "transition-test-2",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    JobRepo::mark_failed(&pool, job_id2).await?;

    let job = JobRepo::get(&pool, job_id2).await?.unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.completed_at.is_some());

    Ok(())
}

#[tokio::test]
async fn test_claim_with_capability_filter() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "cap-test",
        "distributed",
        None,
        "user",
        None,
    )
    .await?;

    // Create steps with different action_types
    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "shell-step".to_string(),
            action_name: "shell-action".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "docker-step".to_string(),
            action_name: "docker-action".to_string(),
            action_type: "docker".to_string(),
            action_image: Some("alpine:latest".to_string()),
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["docker".to_string()],
            runner: "none".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Worker with only "shell" capability
    let shell_worker = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        shell_worker,
        "shell-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let claimed =
        JobStepRepo::claim_ready_step(&pool, &["shell".to_string()], shell_worker).await?;
    let claimed = claimed.unwrap();
    assert_eq!(claimed.step_name, "shell-step");
    assert_eq!(claimed.action_type, "shell");

    // Worker with only "docker" capability
    let docker_worker = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        docker_worker,
        "docker-worker",
        &["docker".to_string()],
        &["docker".to_string()],
        None,
    )
    .await?;

    let claimed =
        JobStepRepo::claim_ready_step(&pool, &["docker".to_string()], docker_worker).await?;
    let claimed = claimed.unwrap();
    assert_eq!(claimed.step_name, "docker-step");
    assert_eq!(claimed.action_type, "docker");

    // No more steps for either type
    let nothing =
        JobStepRepo::claim_ready_step(&pool, &["shell".to_string()], shell_worker).await?;
    assert!(nothing.is_none());

    Ok(())
}

// ─── Worker list tests ────────────────────────────────────────────────

#[tokio::test]
async fn test_worker_list() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Register two workers with different tags
    let w1 = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        w1,
        "worker-alpha",
        &["shell".to_string()],
        &["shell".to_string(), "docker".to_string()],
        None,
    )
    .await?;

    let w2 = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        w2,
        "worker-beta",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    // List all workers
    let workers = WorkerRepo::list(&pool, 50, 0).await?;
    assert_eq!(workers.len(), 2);

    // Verify both workers present with correct tags
    let alpha = workers.iter().find(|w| w.name == "worker-alpha").unwrap();
    assert_eq!(alpha.status, "active");
    let alpha_tags: Vec<String> = serde_json::from_value(alpha.tags.clone())?;
    assert!(alpha_tags.contains(&"shell".to_string()));
    assert!(alpha_tags.contains(&"docker".to_string()));

    let beta = workers.iter().find(|w| w.name == "worker-beta").unwrap();
    assert_eq!(beta.status, "active");
    let beta_tags: Vec<String> = serde_json::from_value(beta.tags.clone())?;
    assert_eq!(beta_tags, vec!["shell".to_string()]);

    // Test pagination
    let page1 = WorkerRepo::list(&pool, 1, 0).await?;
    assert_eq!(page1.len(), 1);

    let page2 = WorkerRepo::list(&pool, 1, 1).await?;
    assert_eq!(page2.len(), 1);

    let page3 = WorkerRepo::list(&pool, 1, 2).await?;
    assert_eq!(page3.len(), 0);

    Ok(())
}

// ─── User repo tests ──────────────────────────────────────────────────

#[tokio::test]
async fn test_create_user_and_get_by_email() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(
        &pool,
        user_id,
        "alice@example.com",
        Some("$argon2id$hashed"),
        Some("Alice"),
    )
    .await?;

    let user = UserRepo::get_by_email(&pool, "alice@example.com")
        .await?
        .expect("User should exist");
    assert_eq!(user.user_id, user_id);
    assert_eq!(user.email, "alice@example.com");
    assert_eq!(user.name.as_deref(), Some("Alice"));
    assert_eq!(user.password_hash.as_deref(), Some("$argon2id$hashed"));

    Ok(())
}

#[tokio::test]
async fn test_get_user_by_id() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "bob@example.com", None, None).await?;

    let user = UserRepo::get_by_id(&pool, user_id)
        .await?
        .expect("User should exist");
    assert_eq!(user.email, "bob@example.com");
    assert!(user.password_hash.is_none());

    Ok(())
}

#[tokio::test]
async fn test_get_nonexistent_user() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let result = UserRepo::get_by_email(&pool, "nobody@example.com").await?;
    assert!(result.is_none());

    let result = UserRepo::get_by_id(&pool, Uuid::new_v4()).await?;
    assert!(result.is_none());

    Ok(())
}

#[tokio::test]
async fn test_duplicate_email_fails() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    UserRepo::create(&pool, Uuid::new_v4(), "dup@example.com", None, None).await?;
    let result = UserRepo::create(&pool, Uuid::new_v4(), "dup@example.com", None, None).await;
    assert!(result.is_err());

    Ok(())
}

// ─── Refresh token repo tests ─────────────────────────────────────────

#[tokio::test]
async fn test_create_and_get_refresh_token() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "token@example.com", None, None).await?;

    let expires_at = Utc::now() + Duration::days(30);
    RefreshTokenRepo::create(&pool, "hash123", user_id, expires_at).await?;

    let row = RefreshTokenRepo::get_by_hash(&pool, "hash123")
        .await?
        .expect("Token should exist");
    assert_eq!(row.user_id, user_id);
    assert_eq!(row.token_hash, "hash123");

    Ok(())
}

#[tokio::test]
async fn test_delete_refresh_token() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "del@example.com", None, None).await?;

    let expires_at = Utc::now() + Duration::days(30);
    RefreshTokenRepo::create(&pool, "delhash", user_id, expires_at).await?;

    RefreshTokenRepo::delete(&pool, "delhash").await?;
    let result = RefreshTokenRepo::get_by_hash(&pool, "delhash").await?;
    assert!(result.is_none());

    Ok(())
}

#[tokio::test]
async fn test_delete_all_tokens_for_user() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "alltoken@example.com", None, None).await?;

    let expires_at = Utc::now() + Duration::days(30);
    RefreshTokenRepo::create(&pool, "tok1", user_id, expires_at).await?;
    RefreshTokenRepo::create(&pool, "tok2", user_id, expires_at).await?;

    RefreshTokenRepo::delete_all_for_user(&pool, user_id).await?;

    assert!(RefreshTokenRepo::get_by_hash(&pool, "tok1")
        .await?
        .is_none());
    assert!(RefreshTokenRepo::get_by_hash(&pool, "tok2")
        .await?
        .is_none());

    Ok(())
}

// ─── User list tests ─────────────────────────────────────────────────

#[tokio::test]
async fn test_user_list() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Empty list on fresh DB
    let empty = UserRepo::list(&pool, 50, 0).await?;
    assert!(empty.is_empty());

    // Create users with mixed auth: some with password, some without
    for i in 0..5 {
        let password = if i % 2 == 0 {
            Some("$argon2id$hashed")
        } else {
            None
        };
        UserRepo::create(
            &pool,
            Uuid::new_v4(),
            &format!("user{}@example.com", i),
            password,
            Some(&format!("User {}", i)),
        )
        .await?;
        // Small sleep so created_at ordering is deterministic
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    // List all
    let users = UserRepo::list(&pool, 50, 0).await?;
    assert_eq!(users.len(), 5);
    // Newest first
    assert_eq!(users[0].email, "user4@example.com");
    assert_eq!(users[4].email, "user0@example.com");

    // Verify password_hash presence matches what we created
    // Even-indexed users (0,2,4) have passwords; odd (1,3) don't
    // List is newest-first: user4, user3, user2, user1, user0
    assert!(users[0].password_hash.is_some()); // user4 (even)
    assert!(users[1].password_hash.is_none()); // user3 (odd)
    assert!(users[2].password_hash.is_some()); // user2 (even)
    assert!(users[3].password_hash.is_none()); // user1 (odd)
    assert!(users[4].password_hash.is_some()); // user0 (even)

    // Pagination
    let page1 = UserRepo::list(&pool, 2, 0).await?;
    assert_eq!(page1.len(), 2);

    let page2 = UserRepo::list(&pool, 2, 2).await?;
    assert_eq!(page2.len(), 2);

    let page3 = UserRepo::list(&pool, 2, 4).await?;
    assert_eq!(page3.len(), 1);

    let empty = UserRepo::list(&pool, 2, 6).await?;
    assert!(empty.is_empty());

    Ok(())
}

// ─── User last login tests ──────────────────────────────────────────

#[tokio::test]
async fn test_user_touch_last_login() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let uid = Uuid::new_v4();
    UserRepo::create(
        &pool,
        uid,
        "login@example.com",
        Some("$argon2id$hashed"),
        None,
    )
    .await?;

    // Initially null
    let user = UserRepo::get_by_id(&pool, uid).await?.unwrap();
    assert!(user.last_login_at.is_none());

    // Touch
    UserRepo::touch_last_login(&pool, uid).await?;
    let user = UserRepo::get_by_id(&pool, uid).await?.unwrap();
    assert!(user.last_login_at.is_some());
    let first_login = user.last_login_at.unwrap();

    // Touch again — timestamp should advance
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    UserRepo::touch_last_login(&pool, uid).await?;
    let user = UserRepo::get_by_id(&pool, uid).await?.unwrap();
    assert!(user.last_login_at.unwrap() > first_login);

    Ok(())
}

// ─── User auth link batch tests ──────────────────────────────────────

#[tokio::test]
async fn test_auth_link_list_by_user_ids() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let u1 = Uuid::new_v4();
    let u2 = Uuid::new_v4();
    let u3 = Uuid::new_v4();
    UserRepo::create(&pool, u1, "a@example.com", None, None).await?;
    UserRepo::create(&pool, u2, "b@example.com", None, None).await?;
    UserRepo::create(&pool, u3, "c@example.com", None, None).await?;

    // u1 has two auth links, u2 has one, u3 has none
    UserAuthLinkRepo::create(&pool, u1, "google", "ext1").await?;
    UserAuthLinkRepo::create(&pool, u1, "github", "ext2").await?;
    UserAuthLinkRepo::create(&pool, u2, "google", "ext3").await?;

    // Batch query for u1 and u2
    let links = UserAuthLinkRepo::list_by_user_ids(&pool, &[u1, u2]).await?;
    assert_eq!(links.len(), 3);

    // Only u1
    let links = UserAuthLinkRepo::list_by_user_ids(&pool, &[u1]).await?;
    assert_eq!(links.len(), 2);

    // u3 has no links
    let links = UserAuthLinkRepo::list_by_user_ids(&pool, &[u3]).await?;
    assert!(links.is_empty());

    // Empty input
    let links = UserAuthLinkRepo::list_by_user_ids(&pool, &[]).await?;
    assert!(links.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_user_list_with_auth_links() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // User with password + OIDC
    let u1 = Uuid::new_v4();
    UserRepo::create(
        &pool,
        u1,
        "both@example.com",
        Some("$argon2id$hash"),
        Some("Both"),
    )
    .await?;
    UserAuthLinkRepo::create(&pool, u1, "google", "g1").await?;

    // User with OIDC only (no password)
    let u2 = Uuid::new_v4();
    UserRepo::create(&pool, u2, "oidc@example.com", None, Some("OIDC Only")).await?;
    UserAuthLinkRepo::create(&pool, u2, "github", "gh1").await?;

    // User with password only (no OIDC)
    let u3 = Uuid::new_v4();
    UserRepo::create(
        &pool,
        u3,
        "pass@example.com",
        Some("$argon2id$hash"),
        Some("Password Only"),
    )
    .await?;

    // List users and their auth links together (simulates what the API handler does)
    let users = UserRepo::list(&pool, 50, 0).await?;
    let user_ids: Vec<Uuid> = users.iter().map(|u| u.user_id).collect();
    let links = UserAuthLinkRepo::list_by_user_ids(&pool, &user_ids).await?;

    assert_eq!(users.len(), 3);
    assert_eq!(links.len(), 2); // u1 has 1 link, u2 has 1 link, u3 has 0

    // Verify u1 (both) has password and an auth link
    let u1_row = users.iter().find(|u| u.user_id == u1).unwrap();
    assert!(u1_row.password_hash.is_some());
    let u1_links: Vec<&str> = links
        .iter()
        .filter(|l| l.user_id == u1)
        .map(|l| l.provider_id.as_str())
        .collect();
    assert_eq!(u1_links, vec!["google"]);

    // Verify u2 (OIDC only) has no password but has auth link
    let u2_row = users.iter().find(|u| u.user_id == u2).unwrap();
    assert!(u2_row.password_hash.is_none());
    let u2_links: Vec<&str> = links
        .iter()
        .filter(|l| l.user_id == u2)
        .map(|l| l.provider_id.as_str())
        .collect();
    assert_eq!(u2_links, vec!["github"]);

    // Verify u3 (password only) has password but no auth link
    let u3_row = users.iter().find(|u| u.user_id == u3).unwrap();
    assert!(u3_row.password_hash.is_some());
    let u3_links: Vec<&str> = links
        .iter()
        .filter(|l| l.user_id == u3)
        .map(|l| l.provider_id.as_str())
        .collect();
    assert!(u3_links.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_expired_token_still_retrievable() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "expired@example.com", None, None).await?;

    // Create a token that's already expired
    let expires_at = Utc::now() - Duration::hours(1);
    RefreshTokenRepo::create(&pool, "exphash", user_id, expires_at).await?;

    // Token exists in DB (app logic checks expiry)
    let row = RefreshTokenRepo::get_by_hash(&pool, "exphash")
        .await?
        .expect("Expired token should still be retrievable");
    assert!(row.expires_at < Utc::now());

    Ok(())
}

// ─── Status filter tests ─────────────────────────────────────────────

#[tokio::test]
async fn test_list_jobs_with_status_filter() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create 3 jobs, transition them to different statuses
    let job1 =
        JobRepo::create(&pool, "default", "task-a", "distributed", None, "api", None).await?;
    let job2 =
        JobRepo::create(&pool, "default", "task-b", "distributed", None, "api", None).await?;
    let _job3 =
        JobRepo::create(&pool, "default", "task-c", "distributed", None, "api", None).await?;

    // job1 → completed, job2 → failed, job3 stays pending
    JobRepo::mark_completed(&pool, job1, None).await?;
    JobRepo::mark_failed(&pool, job2).await?;

    // Filter by completed
    let jobs = JobRepo::list(&pool, None, Some("completed"), 10, 0).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, "completed");

    // Filter by failed
    let jobs = JobRepo::list(&pool, None, Some("failed"), 10, 0).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, "failed");

    // Filter by pending
    let jobs = JobRepo::list(&pool, None, Some("pending"), 10, 0).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, "pending");

    // No filter — all 3
    let jobs = JobRepo::list(&pool, None, None, 10, 0).await?;
    assert_eq!(jobs.len(), 3);

    // Count with status filter
    assert_eq!(JobRepo::count(&pool, None, Some("completed")).await?, 1);
    assert_eq!(JobRepo::count(&pool, None, Some("failed")).await?, 1);
    assert_eq!(JobRepo::count(&pool, None, Some("pending")).await?, 1);
    assert_eq!(JobRepo::count(&pool, None, None).await?, 3);

    Ok(())
}

#[tokio::test]
async fn test_list_jobs_with_workspace_and_status_filter() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create jobs in two workspaces
    let j1 = JobRepo::create(&pool, "ws-a", "task", "distributed", None, "api", None).await?;
    let _j2 = JobRepo::create(&pool, "ws-a", "task", "distributed", None, "api", None).await?;
    let j3 = JobRepo::create(&pool, "ws-b", "task", "distributed", None, "api", None).await?;

    // j1 → completed, j2 stays pending, j3 → completed
    JobRepo::mark_completed(&pool, j1, None).await?;
    JobRepo::mark_completed(&pool, j3, None).await?;

    // ws-a + completed = 1
    let jobs = JobRepo::list(&pool, Some("ws-a"), Some("completed"), 10, 0).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].workspace, "ws-a");
    assert_eq!(jobs[0].status, "completed");

    // ws-a + pending = 1
    let jobs = JobRepo::list(&pool, Some("ws-a"), Some("pending"), 10, 0).await?;
    assert_eq!(jobs.len(), 1);

    // ws-b + completed = 1
    let jobs = JobRepo::list(&pool, Some("ws-b"), Some("completed"), 10, 0).await?;
    assert_eq!(jobs.len(), 1);

    // Count matches
    assert_eq!(
        JobRepo::count(&pool, Some("ws-a"), Some("completed")).await?,
        1
    );
    assert_eq!(JobRepo::count(&pool, Some("ws-a"), None).await?, 2);

    Ok(())
}

#[tokio::test]
async fn test_list_by_task_with_status_filter() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let j1 = JobRepo::create(&pool, "default", "deploy", "distributed", None, "api", None).await?;
    let _j2 = JobRepo::create(&pool, "default", "deploy", "distributed", None, "api", None).await?;
    let _j3 = JobRepo::create(&pool, "default", "deploy", "distributed", None, "api", None).await?;

    // j1 → completed, j2/j3 stay pending
    JobRepo::mark_completed(&pool, j1, None).await?;

    // list_by_task with status filter
    let jobs = JobRepo::list_by_task(&pool, "default", "deploy", Some("completed"), 10, 0).await?;
    assert_eq!(jobs.len(), 1);

    let jobs = JobRepo::list_by_task(&pool, "default", "deploy", Some("pending"), 10, 0).await?;
    assert_eq!(jobs.len(), 2);

    let jobs = JobRepo::list_by_task(&pool, "default", "deploy", None, 10, 0).await?;
    assert_eq!(jobs.len(), 3);

    // count_by_task with status filter
    assert_eq!(
        JobRepo::count_by_task(&pool, "default", "deploy", Some("completed")).await?,
        1
    );
    assert_eq!(
        JobRepo::count_by_task(&pool, "default", "deploy", Some("pending")).await?,
        2
    );
    assert_eq!(
        JobRepo::count_by_task(&pool, "default", "deploy", None).await?,
        3
    );

    Ok(())
}

#[tokio::test]
async fn test_status_filter_returns_empty_for_nonexistent_status() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    JobRepo::create(&pool, "default", "task", "distributed", None, "api", None).await?;

    // No jobs with "running" status
    let jobs = JobRepo::list(&pool, None, Some("running"), 10, 0).await?;
    assert!(jobs.is_empty());
    assert_eq!(JobRepo::count(&pool, None, Some("running")).await?, 0);

    Ok(())
}

// ─── Transaction rollback test ───────────────────────────────────────

#[tokio::test]
async fn test_transaction_rollback_on_step_failure() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = Uuid::new_v4();

    // Start a transaction, create job, then force step creation to fail
    let mut tx = pool.begin().await?;

    JobRepo::create_with_parent_tx_id(
        &mut *tx,
        job_id,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
        None,
        None,
    )
    .await?;

    // Verify job exists within the transaction
    let row = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM job WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(row.0, 1, "Job should exist within transaction");

    // Now explicitly roll back (simulating a step creation failure)
    tx.rollback().await?;

    // Verify job does NOT exist after rollback
    let row = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM job WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.0, 0, "Job should not exist after rollback");

    Ok(())
}

#[tokio::test]
async fn test_transaction_commit_persists_job_and_steps() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = Uuid::new_v4();

    let mut tx = pool.begin().await?;

    JobRepo::create_with_parent_tx_id(
        &mut *tx,
        job_id,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
        None,
        None,
    )
    .await?;

    let steps = vec![NewJobStep {
        job_id,
        step_name: "build".to_string(),
        action_name: "greet".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["shell".to_string()],
        runner: "local".to_string(),
    }];

    JobStepRepo::create_steps_tx(&mut *tx, &steps).await?;

    tx.commit().await?;

    // Both job and step should exist after commit
    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist after commit");
    assert_eq!(job.task_name, "test-task");

    let job_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(job_steps.len(), 1);
    assert_eq!(job_steps[0].step_name, "build");

    Ok(())
}

// ─── Cancellation tests ───────────────────────────────────────────────

#[tokio::test]
async fn test_cancel_job() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create a job with default pending status
    let job_id = JobRepo::create(
        &pool,
        "default",
        "cancel-test",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Verify initial state is pending
    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist");
    assert_eq!(job.status, "pending");
    assert!(job.completed_at.is_none());

    // Cancel the pending job — should succeed
    let cancelled = JobRepo::cancel(&pool, job_id).await?;
    assert!(cancelled, "cancel() should return true for a pending job");

    // Verify status and completed_at are set
    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist");
    assert_eq!(job.status, "cancelled");
    assert!(
        job.completed_at.is_some(),
        "completed_at should be set after cancel"
    );

    // Attempt to cancel again — already terminal, should return false
    let cancelled_again = JobRepo::cancel(&pool, job_id).await?;
    assert!(
        !cancelled_again,
        "cancel() should return false when job is already terminal"
    );

    Ok(())
}

#[tokio::test]
async fn test_cancel_job_running() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create a worker and a job, then transition to running
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "cancel-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "cancel-running-test",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Transition to running via mark_running_if_pending
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;

    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist");
    assert_eq!(job.status, "running");

    // Cancel the running job — should succeed
    let cancelled = JobRepo::cancel(&pool, job_id).await?;
    assert!(cancelled, "cancel() should return true for a running job");

    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist");
    assert_eq!(job.status, "cancelled");
    assert!(job.completed_at.is_some());

    Ok(())
}

#[tokio::test]
async fn test_cancel_job_already_completed() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "cancel-completed-test",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Mark completed before attempting cancel
    JobRepo::mark_completed(&pool, job_id, Some(serde_json::json!({"result": "ok"}))).await?;

    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist");
    assert_eq!(job.status, "completed");

    // Cancel on an already-completed job — should return false
    let cancelled = JobRepo::cancel(&pool, job_id).await?;
    assert!(
        !cancelled,
        "cancel() should return false for a completed job"
    );

    // Status must remain completed
    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("Job should exist");
    assert_eq!(job.status, "completed");

    Ok(())
}

#[tokio::test]
async fn test_get_child_jobs() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Create a parent job
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

    // Create two active child jobs linked to the parent
    let child1_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&parent_id.to_string()),
        Some(parent_id),
        Some("step-a"),
    )
    .await?;

    let child2_id = JobRepo::create_with_parent(
        &pool,
        "default",
        "child-task",
        "distributed",
        None,
        "task",
        Some(&parent_id.to_string()),
        Some(parent_id),
        Some("step-b"),
    )
    .await?;

    // Both children are pending — get_child_jobs returns pending + running
    let children = JobRepo::get_child_jobs(&pool, parent_id).await?;
    assert_eq!(
        children.len(),
        2,
        "Both pending children should be returned"
    );

    let child_ids: Vec<Uuid> = children.iter().map(|j| j.job_id).collect();
    assert!(child_ids.contains(&child1_id));
    assert!(child_ids.contains(&child2_id));

    // Complete one child — it should no longer appear in get_child_jobs
    JobRepo::mark_completed(&pool, child1_id, None).await?;

    let children = JobRepo::get_child_jobs(&pool, parent_id).await?;
    assert_eq!(children.len(), 1, "Completed child should not be returned");
    assert_eq!(children[0].job_id, child2_id);

    // Complete the second child — result should be empty
    JobRepo::mark_completed(&pool, child2_id, None).await?;

    let children = JobRepo::get_child_jobs(&pool, parent_id).await?;
    assert!(children.is_empty(), "No active children should remain");

    Ok(())
}

#[tokio::test]
async fn test_cancel_pending_steps() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Need a worker to mark a step as running
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "cancel-steps-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "cancel-steps-test",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create three steps: one pending, one ready, one running
    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "pending-step".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "ready-step".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "running-step".to_string(),
            action_name: "action3".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Transition the third step to running
    JobStepRepo::mark_running(&pool, job_id, "running-step", worker_id).await?;

    // cancel_pending_steps should cancel pending + ready (2 steps), leaving running untouched
    let count = JobStepRepo::cancel_pending_steps(&pool, job_id).await?;
    assert_eq!(count, 2, "Two steps (pending + ready) should be cancelled");

    // Verify individual step statuses
    let all_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let status_map: HashMap<String, String> = all_steps
        .iter()
        .map(|s| (s.step_name.clone(), s.status.clone()))
        .collect();

    assert_eq!(status_map["pending-step"], "cancelled");
    assert_eq!(status_map["ready-step"], "cancelled");
    assert_eq!(
        status_map["running-step"], "running",
        "Running step must not be cancelled by cancel_pending_steps"
    );

    Ok(())
}

#[tokio::test]
async fn test_get_running_steps() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "running-steps-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "running-steps-test",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create steps in mixed statuses
    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "pending-step".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "ready-step".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "running-step-a".to_string(),
            action_name: "action3".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "running-step-b".to_string(),
            action_name: "action4".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "completed-step".to_string(),
            action_name: "action5".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Before any running steps, result should be empty
    let running = JobStepRepo::get_running_steps(&pool, job_id).await?;
    assert!(running.is_empty(), "No running steps initially");

    // Transition two steps to running and one to completed
    JobStepRepo::mark_running(&pool, job_id, "running-step-a", worker_id).await?;
    JobStepRepo::mark_running(&pool, job_id, "running-step-b", worker_id).await?;
    JobStepRepo::mark_running(&pool, job_id, "completed-step", worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "completed-step", None).await?;

    // Only the two actively running steps should be returned
    let running = JobStepRepo::get_running_steps(&pool, job_id).await?;
    assert_eq!(running.len(), 2, "Exactly two steps should be running");

    let running_names: Vec<&str> = running.iter().map(|s| s.step_name.as_str()).collect();
    assert!(running_names.contains(&"running-step-a"));
    assert!(running_names.contains(&"running-step-b"));

    // Pending, ready, and completed steps must not appear
    assert!(!running_names.contains(&"pending-step"));
    assert!(!running_names.contains(&"ready-step"));
    assert!(!running_names.contains(&"completed-step"));

    Ok(())
}

#[tokio::test]
async fn test_mark_cancelled_only_running() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "mark-cancelled-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "mark-cancelled-test",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create a completed step and a running step
    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "completed-step".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "running-step".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Put both through running, then complete one
    JobStepRepo::mark_running(&pool, job_id, "completed-step", worker_id).await?;
    JobStepRepo::mark_completed(
        &pool,
        job_id,
        "completed-step",
        Some(serde_json::json!({"ok": true})),
    )
    .await?;
    JobStepRepo::mark_running(&pool, job_id, "running-step", worker_id).await?;

    // Call mark_cancelled on the completed step — the WHERE status = 'running' guard must protect it
    JobStepRepo::mark_cancelled(&pool, job_id, "completed-step").await?;

    let all_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let status_map: HashMap<String, String> = all_steps
        .iter()
        .map(|s| (s.step_name.clone(), s.status.clone()))
        .collect();

    assert_eq!(
        status_map["completed-step"], "completed",
        "Completed step must remain completed after mark_cancelled"
    );

    // Call mark_cancelled on the running step — it should transition to cancelled
    JobStepRepo::mark_cancelled(&pool, job_id, "running-step").await?;

    let all_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let status_map: HashMap<String, String> = all_steps
        .iter()
        .map(|s| (s.step_name.clone(), s.status.clone()))
        .collect();

    assert_eq!(
        status_map["running-step"], "cancelled",
        "Running step should be cancelled by mark_cancelled"
    );

    Ok(())
}

#[tokio::test]
async fn test_cancel_pending_steps_empty() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "empty-cancel-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "empty-cancel-test",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create steps, then complete them all so none are pending or ready
    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step-a".to_string(),
            action_name: "action1".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step-b".to_string(),
            action_name: "action2".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["shell".to_string()],
            runner: "local".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Transition both to completed via running
    JobStepRepo::mark_running(&pool, job_id, "step-a", worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step-a", None).await?;
    JobStepRepo::mark_running(&pool, job_id, "step-b", worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step-b", None).await?;

    // No pending/ready steps remain — cancel_pending_steps should return 0
    let count = JobStepRepo::cancel_pending_steps(&pool, job_id).await?;
    assert_eq!(
        count, 0,
        "No steps should be cancelled when all are already completed"
    );

    // Verify statuses are unchanged
    let all_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    for step in &all_steps {
        assert_eq!(
            step.status, "completed",
            "Step {} should remain completed",
            step.step_name
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_get_status_counts() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Empty database — all counts should be zero (or absent from the map)
    let counts = JobRepo::get_status_counts(&pool).await?;
    assert_eq!(*counts.get("pending").unwrap_or(&0), 0);
    assert_eq!(*counts.get("running").unwrap_or(&0), 0);
    assert_eq!(*counts.get("completed").unwrap_or(&0), 0);
    assert_eq!(*counts.get("failed").unwrap_or(&0), 0);
    assert_eq!(*counts.get("cancelled").unwrap_or(&0), 0);

    // Create 3 pending jobs
    for i in 0..3 {
        JobRepo::create(
            &pool,
            "default",
            &format!("task-{i}"),
            "distributed",
            None,
            "api",
            None,
        )
        .await?;
    }

    // Mark one job as completed and one as failed
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "w1",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let job_ids: Vec<Uuid> = JobRepo::list(&pool, None, None, 10, 0)
        .await?
        .into_iter()
        .map(|j| j.job_id)
        .collect();

    JobRepo::mark_running(&pool, job_ids[0], worker_id).await?;
    JobRepo::mark_completed(&pool, job_ids[0], None).await?;

    JobRepo::mark_running(&pool, job_ids[1], worker_id).await?;
    JobRepo::mark_failed(&pool, job_ids[1]).await?;

    // Recount — expect 1 pending, 0 running, 1 completed, 1 failed
    let counts = JobRepo::get_status_counts(&pool).await?;
    assert_eq!(*counts.get("pending").unwrap_or(&0), 1);
    assert_eq!(*counts.get("running").unwrap_or(&0), 0);
    assert_eq!(*counts.get("completed").unwrap_or(&0), 1);
    assert_eq!(*counts.get("failed").unwrap_or(&0), 1);
    assert_eq!(*counts.get("cancelled").unwrap_or(&0), 0);

    // Cancel the remaining pending job
    JobRepo::cancel(&pool, job_ids[2]).await?;

    let counts = JobRepo::get_status_counts(&pool).await?;
    assert_eq!(*counts.get("pending").unwrap_or(&0), 0);
    assert_eq!(*counts.get("cancelled").unwrap_or(&0), 1);

    Ok(())
}

#[tokio::test]
async fn test_worker_register_stores_version() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Register WITH a version string
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "versioned-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        Some("0.5.9"),
    )
    .await?;

    let worker = WorkerRepo::get(&pool, worker_id)
        .await?
        .expect("Worker should exist");
    assert_eq!(worker.version.as_deref(), Some("0.5.9"));

    // Register WITHOUT a version string (legacy worker)
    let legacy_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        legacy_id,
        "legacy-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        None,
    )
    .await?;

    let legacy = WorkerRepo::get(&pool, legacy_id)
        .await?
        .expect("Legacy worker should exist");
    assert!(legacy.version.is_none());

    Ok(())
}

#[tokio::test]
async fn test_worker_list_includes_version() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "versioned-worker",
        &["shell".to_string()],
        &["shell".to_string()],
        Some("1.2.3"),
    )
    .await?;

    let workers = WorkerRepo::list(&pool, 10, 0).await?;
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].version.as_deref(), Some("1.2.3"));

    Ok(())
}
