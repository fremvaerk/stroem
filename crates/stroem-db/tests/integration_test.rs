use anyhow::Result;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::FlowStep;
use stroem_db::{
    create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep, RefreshTokenRepo, UserRepo,
    WorkerRepo,
};
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
    let jobs = JobRepo::list(&pool, None, 3, 0).await?;
    assert_eq!(jobs.len(), 3);

    // List with offset
    let jobs = JobRepo::list(&pool, None, 3, 3).await?;
    assert_eq!(jobs.len(), 2);

    // List with workspace filter
    let jobs = JobRepo::list(&pool, Some("default"), 10, 0).await?;
    assert_eq!(jobs.len(), 5);

    Ok(())
}

#[tokio::test]
async fn test_create_steps_and_claim() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Register a worker
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

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
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

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
        },
    ];

    JobStepRepo::create_steps(&pool, &steps).await?;

    // Define flow
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "action1".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "action2".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
        },
    );
    flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "action3".to_string(),
            depends_on: vec!["step2".to_string(), "step4".to_string()],
            input: HashMap::new(),
        },
    );
    flow.insert(
        "step4".to_string(),
        FlowStep {
            action: "action4".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
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
    WorkerRepo::register(&pool, worker_id, "test-worker", &capabilities).await?;

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
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;
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
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Worker with only "shell" capability
    let shell_worker = Uuid::new_v4();
    WorkerRepo::register(&pool, shell_worker, "shell-worker", &["shell".to_string()]).await?;

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
