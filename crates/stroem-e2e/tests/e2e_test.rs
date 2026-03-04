mod harness;

use harness::TestEnv;
use serde_json::json;
use std::time::Duration;

/// Test 1: Single-step task execution with input propagation.
#[tokio::test(flavor = "multi_thread")]
async fn test_simple_task_execution() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "simple-echo", json!({"name": "Alice"}))
        .await
        .expect("Failed to execute task");

    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Job did not complete");
    assert_eq!(status, "completed");

    // Verify step completed
    let steps = env.get_steps(job_id).await.expect("Failed to get steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["step_name"].as_str(), Some("greet"));
    assert_eq!(steps[0]["status"].as_str(), Some("completed"));

    // Verify logs contain the expected output
    let logs = env
        .get_step_logs(job_id, "greet")
        .await
        .expect("Failed to get logs");
    assert!(
        logs.contains("Hello Alice!"),
        "Logs should contain 'Hello Alice!', got: {}",
        logs
    );

    env.shutdown().await;
}

/// Test 2: Multi-step task where step 2 depends on step 1's output.
/// Also verifies the structured output object is parsed and stored.
#[tokio::test(flavor = "multi_thread")]
async fn test_multi_step_output_propagation() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "two-step-chain", json!({"name": "Bob"}))
        .await
        .expect("Failed to execute task");

    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Job did not complete");
    assert_eq!(status, "completed");

    // Both steps should be completed
    let steps = env.get_steps(job_id).await.expect("Failed to get steps");
    assert_eq!(steps.len(), 2);
    for step in &steps {
        assert_eq!(
            step["status"].as_str(),
            Some("completed"),
            "Step '{}' should be completed",
            step["step_name"]
        );
    }

    // Verify greet step has structured output parsed from OUTPUT: JSON
    let greet_step = steps
        .iter()
        .find(|s| s["step_name"].as_str() == Some("greet"))
        .expect("greet step not found");
    assert_eq!(
        greet_step["output"]["greeting"].as_str(),
        Some("Hello Bob!"),
        "Greet step output should have parsed greeting, got: {}",
        greet_step["output"]
    );

    // The shout step should produce uppercase output from step 1
    let shout_logs = env
        .get_step_logs(job_id, "shout")
        .await
        .expect("Failed to get shout logs");
    assert!(
        shout_logs.contains("HELLO BOB!"),
        "Shout logs should contain uppercase 'HELLO BOB!', got: {}",
        shout_logs
    );

    env.shutdown().await;
}

/// Test 3: A task with a step that exits non-zero -> job fails.
#[tokio::test(flavor = "multi_thread")]
async fn test_failing_task() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "failing-task", json!({}))
        .await
        .expect("Failed to execute task");

    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Job did not reach terminal state");
    assert_eq!(status, "failed");

    // Verify step has error info
    let steps = env.get_steps(job_id).await.expect("Failed to get steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["status"].as_str(), Some("failed"));
    let error = steps[0]["error_message"].as_str().unwrap_or("");
    assert!(
        error.contains("Exit code") || error.contains("exit code"),
        "Step error should mention exit code, got: {}",
        error
    );

    env.shutdown().await;
}

/// Test 4: on_error hook fires when a task fails, creating a hook job.
#[tokio::test(flavor = "multi_thread")]
async fn test_on_error_hook_fires() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "hook-on-error", json!({}))
        .await
        .expect("Failed to execute task");

    // Wait for the main job to fail
    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Main job did not reach terminal state");
    assert_eq!(status, "failed");

    // Poll for the hook job to appear and complete (hooks are async)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let hook_job_id;
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("Hook job did not appear within timeout");
        }

        let rows = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            "SELECT job_id, task_name, status FROM job WHERE source_type = 'hook' AND source_id = $1",
        )
        .bind(job_id.to_string())
        .fetch_all(&env.pool)
        .await
        .expect("DB query failed");

        if let Some(row) = rows.first() {
            if row.2 == "completed" || row.2 == "failed" {
                hook_job_id = row.0;
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Verify the hook job completed (not failed -- the hook action itself should succeed)
    let hook_job = env
        .get_job(hook_job_id)
        .await
        .expect("Failed to get hook job");
    assert_eq!(
        hook_job["status"].as_str(),
        Some("completed"),
        "Hook job should complete successfully"
    );

    env.shutdown().await;
}

/// Test 5: Worker recovery -- stale worker's step is failed by the recovery sweeper.
/// Stops the worker before backdating heartbeat to eliminate race conditions.
#[tokio::test(flavor = "multi_thread")]
async fn test_worker_recovery() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "slow-task", json!({}))
        .await
        .expect("Failed to execute task");

    // Wait for the step to reach "running" (worker has claimed and started it)
    env.wait_for_step_status(job_id, "wait", "running", Duration::from_secs(15))
        .await
        .expect("Step did not reach running status");

    // Stop the worker so it can't send heartbeats that race with our DB manipulation
    env.stop_worker().await;

    // Manipulate DB: set worker heartbeat far in the past to simulate stale worker
    sqlx::query("UPDATE worker SET last_heartbeat = NOW() - interval '300 seconds'")
        .execute(&env.pool)
        .await
        .expect("Failed to update worker heartbeat");

    // Recovery sweeper runs every 2s with 5s timeout, so it should detect this quickly
    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Job did not reach terminal state after recovery");
    assert_eq!(status, "failed");

    // Verify the step error mentions heartbeat timeout
    let steps = env.get_steps(job_id).await.expect("Failed to get steps");
    let step = steps
        .iter()
        .find(|s| s["step_name"].as_str() == Some("wait"))
        .expect("Step 'wait' not found");
    let error = step["error_message"].as_str().unwrap_or("");
    assert!(
        error.contains("heartbeat timeout") || error.contains("Worker heartbeat timeout"),
        "Step error should mention heartbeat timeout, got: {}",
        error
    );

    env.shutdown().await;
}

/// Test 6: Cancelling a running job via API -> job reaches "cancelled" status.
#[tokio::test(flavor = "multi_thread")]
async fn test_job_cancellation() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "slow-task", json!({}))
        .await
        .expect("Failed to execute task");

    // Wait for the step to reach "running"
    env.wait_for_step_status(job_id, "wait", "running", Duration::from_secs(15))
        .await
        .expect("Step did not reach running status");

    // Cancel the job
    env.cancel_job(job_id).await.expect("Failed to cancel job");

    // Wait for the job to reach "cancelled"
    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Job did not reach terminal state after cancel");
    assert_eq!(
        status, "cancelled",
        "Job should be cancelled, got: {}",
        status
    );

    env.shutdown().await;
}

/// Test 7: Default input values are used when input is omitted.
#[tokio::test(flavor = "multi_thread")]
async fn test_default_input_values() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    // Execute with empty input — should use default "E2E"
    let job_id = env
        .execute_task("default", "simple-echo", json!({}))
        .await
        .expect("Failed to execute task");

    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Job did not complete");
    assert_eq!(status, "completed");

    let logs = env
        .get_step_logs(job_id, "greet")
        .await
        .expect("Failed to get logs");
    assert!(
        logs.contains("Hello E2E!"),
        "Default input 'E2E' should be used, got: {}",
        logs
    );

    env.shutdown().await;
}

/// Test 8: on_success hook fires when a task completes successfully.
#[tokio::test(flavor = "multi_thread")]
async fn test_on_success_hook_fires() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "hook-on-success", json!({}))
        .await
        .expect("Failed to execute task");

    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Job did not complete");
    assert_eq!(status, "completed");

    // Poll for the success hook job
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let hook_job_id;
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("Success hook job did not appear within timeout");
        }

        let rows = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            "SELECT job_id, task_name, status FROM job WHERE source_type = 'hook' AND source_id = $1",
        )
        .bind(job_id.to_string())
        .fetch_all(&env.pool)
        .await
        .expect("DB query failed");

        if let Some(row) = rows.first() {
            if row.2 == "completed" || row.2 == "failed" {
                hook_job_id = row.0;
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let hook_job = env
        .get_job(hook_job_id)
        .await
        .expect("Failed to get hook job");
    assert_eq!(
        hook_job["status"].as_str(),
        Some("completed"),
        "Success hook job should complete successfully"
    );

    env.shutdown().await;
}

/// Test 9: Task action (type: task) creates a child sub-job that completes.
#[tokio::test(flavor = "multi_thread")]
async fn test_task_action_sub_job() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    let job_id = env
        .execute_task("default", "parent-task", json!({"name": "Child"}))
        .await
        .expect("Failed to execute parent task");

    let status = env
        .wait_for_job(job_id, Duration::from_secs(30))
        .await
        .expect("Parent job did not complete");
    assert_eq!(status, "completed");

    // The parent job's "child" step should be completed
    let steps = env
        .get_steps(job_id)
        .await
        .expect("Failed to get parent steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["step_name"].as_str(), Some("child"));
    assert_eq!(steps[0]["status"].as_str(), Some("completed"));

    // A child job should exist with source_type = 'task'
    let child_rows = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
        "SELECT job_id, task_name, status FROM job WHERE source_type = 'task' AND source_id LIKE $1",
    )
    .bind(format!("{}/%", job_id))
    .fetch_all(&env.pool)
    .await
    .expect("DB query failed");

    assert!(
        !child_rows.is_empty(),
        "Child job should exist with source_type = 'task'"
    );
    assert_eq!(child_rows[0].1, "simple-echo");
    assert_eq!(child_rows[0].2, "completed");

    // Verify the child job actually ran with the propagated input
    let child_job = env
        .get_job(child_rows[0].0)
        .await
        .expect("Failed to get child job");
    let child_steps = child_job["steps"]
        .as_array()
        .expect("Child should have steps");
    assert_eq!(child_steps.len(), 1);
    assert_eq!(child_steps[0]["status"].as_str(), Some("completed"));

    env.shutdown().await;
}

/// Test 10: Cancelling a pending job (before any step is claimed).
#[tokio::test(flavor = "multi_thread")]
async fn test_cancel_pending_job() {
    let mut env = TestEnv::setup().await.expect("Failed to set up E2E env");

    // Stop the worker so no step gets claimed
    env.stop_worker().await;

    let job_id = env
        .execute_task("default", "simple-echo", json!({"name": "NeverRun"}))
        .await
        .expect("Failed to execute task");

    // Cancel immediately while still pending
    env.cancel_job(job_id)
        .await
        .expect("Failed to cancel pending job");

    let status = env
        .wait_for_job(job_id, Duration::from_secs(10))
        .await
        .expect("Job did not reach terminal state");
    assert_eq!(
        status, "cancelled",
        "Pending job should be cancelled, got: {}",
        status
    );

    env.shutdown().await;
}
