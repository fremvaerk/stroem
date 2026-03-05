use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use stroem_db::{create_pool, run_migrations};
use stroem_server::config::{
    DbConfig, LogStorageConfig, RecoveryConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use stroem_worker::config::WorkerConfig;
use stroem_worker::executor::StepExecutor;
use stroem_worker::poller::run_worker;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Self-contained E2E test environment with real HTTP server, worker, and Postgres.
pub struct TestEnv {
    pub pool: PgPool,
    pub server_url: String,
    cancel_token: CancellationToken,
    server_handle: JoinHandle<()>,
    worker_handle: Option<JoinHandle<Result<()>>>,
    http: reqwest::Client,
    _temp_dir: TempDir,
    _container: testcontainers::ContainerAsync<Postgres>,
}

/// Ensure cancel signal is sent even if a test panics before calling shutdown().
impl Drop for TestEnv {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

const WORKFLOW_YAML: &str = r#"
actions:
  echo-greeting:
    type: script
    cmd: |
      echo "Hello {{ input.name }}!"
      echo 'OUTPUT: {"greeting": "Hello {{ input.name }}!"}'
    input:
      name: { type: string, default: "World" }

  echo-upper:
    type: script
    cmd: |
      MSG="{{ input.msg }}"
      echo "${MSG}" | tr a-z A-Z
      UPPER=$(echo "${MSG}" | tr a-z A-Z)
      echo "OUTPUT: {\"upper\": \"${UPPER}\"}"
    input:
      msg: { type: string, required: true }

  fail-action:
    type: script
    cmd: |
      echo "About to fail"
      exit 1

  slow-action:
    type: script
    cmd: sleep 300

  hook-notify:
    type: script
    cmd: |
      echo "Hook fired: status={{ input.status }}, task={{ input.task_name }}"
      echo 'OUTPUT: {"notified": true}'
    input:
      status: { type: string, default: "" }
      task_name: { type: string, default: "" }

  success-notify:
    type: script
    cmd: |
      echo "Success hook fired: task={{ input.task_name }}"
      echo 'OUTPUT: {"success_notified": true}'
    input:
      task_name: { type: string, default: "" }

  run-child:
    type: task
    task: simple-echo
    input:
      name: { type: string, default: "World" }

tasks:
  simple-echo:
    mode: distributed
    input:
      name: { type: string, default: "E2E" }
    flow:
      greet:
        action: echo-greeting
        input:
          name: "{{ input.name }}"

  two-step-chain:
    mode: distributed
    input:
      name: { type: string, default: "E2E" }
    flow:
      greet:
        action: echo-greeting
        input:
          name: "{{ input.name }}"
      shout:
        action: echo-upper
        depends_on: [greet]
        input:
          msg: "{{ greet.output.greeting }}"

  failing-task:
    mode: distributed
    flow:
      fail:
        action: fail-action

  hook-on-error:
    mode: distributed
    flow:
      fail:
        action: fail-action
    on_error:
      - action: hook-notify
        input:
          status: "{{ hook.status }}"
          task_name: "{{ hook.task_name }}"

  hook-on-success:
    mode: distributed
    input:
      name: { type: string, default: "SuccessTest" }
    flow:
      greet:
        action: echo-greeting
        input:
          name: "{{ input.name }}"
    on_success:
      - action: success-notify
        input:
          task_name: "{{ hook.task_name }}"

  slow-task:
    mode: distributed
    flow:
      wait:
        action: slow-action

  parent-task:
    mode: distributed
    input:
      name: { type: string, default: "SubJob" }
    flow:
      child:
        action: run-child
        input:
          name: "{{ input.name }}"
"#;

impl TestEnv {
    /// Spin up a full E2E environment: Postgres, HTTP server, worker.
    pub async fn setup() -> Result<Self> {
        // Initialize tracing for test output (idempotent — safe if called multiple times)
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter("warn")
            .try_init();

        // 1. Start Postgres container
        let container = Postgres::default().start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let db_url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
        let pool = create_pool(&db_url).await?;
        run_migrations(&pool).await?;

        // 2. Create temp dir with workflow YAML
        let temp_dir = TempDir::new()?;
        let workflows_dir = temp_dir.path().join(".workflows");
        std::fs::create_dir_all(&workflows_dir)?;
        std::fs::write(workflows_dir.join("test.yaml"), WORKFLOW_YAML)?;

        // Log and workspace cache dirs
        let log_dir = temp_dir.path().join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let workspace_cache_dir = temp_dir.path().join("worker-cache");
        std::fs::create_dir_all(&workspace_cache_dir)?;

        // 3. Build server config
        let server_config = ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            db: DbConfig {
                url: db_url.clone(),
            },
            log_storage: LogStorageConfig {
                local_dir: log_dir.to_string_lossy().to_string(),
                s3: None,
            },
            workspaces: HashMap::from([(
                "default".to_string(),
                WorkspaceSourceDef::Folder {
                    path: temp_dir.path().to_string_lossy().to_string(),
                },
            )]),
            libraries: HashMap::new(),
            git_auth: HashMap::new(),
            worker_token: "e2e-test-token".to_string(),
            auth: None,
            recovery: RecoveryConfig {
                heartbeat_timeout_secs: 5,
                sweep_interval_secs: 2,
            },
        };

        // 4. Load workspaces using real FolderSource
        let workspace_manager = WorkspaceManager::new(
            server_config.workspaces.clone(),
            HashMap::new(),
            HashMap::new(),
        )
        .await;

        // 5. Create AppState
        let log_storage = LogStorage::new(&server_config.log_storage.local_dir);
        let state = AppState::new(
            pool.clone(),
            workspace_manager,
            server_config.clone(),
            log_storage,
            HashMap::new(),
        );

        // 6. Start background tasks
        let cancel_token = CancellationToken::new();
        let _recovery = stroem_server::recovery::start(state.clone(), cancel_token.clone());
        let _scheduler = stroem_server::scheduler::start(state.clone(), cancel_token.clone());

        // 7. Build router and bind to :0
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_url = format!("http://{}", addr);

        // 8. Spawn server
        let server_cancel = cancel_token.clone();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(server_cancel.cancelled_owned())
            .await;
        });

        // 9. Build worker config and spawn worker
        let worker_config = WorkerConfig {
            server_url: server_url.clone(),
            worker_token: "e2e-test-token".to_string(),
            worker_name: "e2e-worker".to_string(),
            max_concurrent: 4,
            poll_interval_secs: 1,
            workspace_cache_dir: workspace_cache_dir.to_string_lossy().to_string(),
            tags: vec!["script".to_string()],
            runner_image: None,
            docker: None,
            kubernetes: None,
            request_timeout_secs: None,
            connect_timeout_secs: None,
        };

        let worker_cancel = cancel_token.clone();
        let worker_handle = tokio::spawn(async move {
            run_worker(worker_config, StepExecutor::new(), worker_cancel).await
        });

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;

        let env = Self {
            pool,
            server_url: server_url.clone(),
            cancel_token,
            server_handle,
            worker_handle: Some(worker_handle),
            http,
            _temp_dir: temp_dir,
            _container: container,
        };

        // 10. Wait for server to be ready (health check)
        env.wait_for_server_ready(Duration::from_secs(15)).await?;

        Ok(env)
    }

    async fn wait_for_server_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("Server did not become ready within {:?}", timeout);
            }
            match self
                .http
                .get(format!("{}/api/config", self.server_url))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    /// Execute a task, returns the job ID.
    pub async fn execute_task(&self, workspace: &str, task: &str, input: Value) -> Result<Uuid> {
        let url = format!(
            "{}/api/workspaces/{}/tasks/{}/execute",
            self.server_url, workspace, task
        );
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "input": input }))
            .send()
            .await
            .context("Failed to POST execute")?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse execute response")?;
        if !status.is_success() {
            anyhow::bail!("Execute failed ({}): {}", status, body);
        }
        let job_id = body["job_id"]
            .as_str()
            .context("Missing job_id in response")?;
        Uuid::parse_str(job_id).with_context(|| format!("Invalid job_id UUID: {}", job_id))
    }

    /// Poll until a job reaches a terminal status, return the status string.
    pub async fn wait_for_job(&self, job_id: Uuid, timeout: Duration) -> Result<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() > deadline {
                let job = self.get_job(job_id).await?;
                anyhow::bail!(
                    "Job {} did not reach terminal state within {:?}, current: {}",
                    job_id,
                    timeout,
                    job["status"]
                );
            }
            let job = self.get_job(job_id).await?;
            let status = job["status"].as_str().unwrap_or("unknown");
            match status {
                "completed" | "failed" | "cancelled" => return Ok(status.to_string()),
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }

    /// GET /api/jobs/{id} with HTTP status check.
    pub async fn get_job(&self, job_id: Uuid) -> Result<Value> {
        let resp = self
            .http
            .get(format!("{}/api/jobs/{}", self.server_url, job_id))
            .send()
            .await
            .context("Failed to GET job")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("Failed to parse job response")?;
        if !status.is_success() {
            anyhow::bail!("GET job {} failed ({}): {}", job_id, status, body);
        }
        Ok(body)
    }

    /// Get steps from the job detail response (steps are inline in GET /api/jobs/{id}).
    pub async fn get_steps(&self, job_id: Uuid) -> Result<Vec<Value>> {
        let job = self.get_job(job_id).await?;
        Ok(job["steps"].as_array().cloned().unwrap_or_default())
    }

    /// GET /api/jobs/{id}/steps/{step}/logs with HTTP status check.
    pub async fn get_step_logs(&self, job_id: Uuid, step: &str) -> Result<String> {
        let resp = self
            .http
            .get(format!(
                "{}/api/jobs/{}/steps/{}/logs",
                self.server_url, job_id, step
            ))
            .send()
            .await
            .context("Failed to GET step logs")?;
        let status = resp.status();
        let body = resp.text().await.context("Failed to read step log body")?;
        if !status.is_success() {
            anyhow::bail!(
                "GET logs for job {}/{} failed ({}): {}",
                job_id,
                step,
                status,
                body
            );
        }
        Ok(body)
    }

    /// POST /api/jobs/{id}/cancel
    pub async fn cancel_job(&self, job_id: Uuid) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/api/jobs/{}/cancel", self.server_url, job_id))
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await?;
            anyhow::bail!("Cancel failed: {}", body);
        }
        Ok(())
    }

    /// Wait for a step within a job to reach a specific status.
    pub async fn wait_for_step_status(
        &self,
        job_id: Uuid,
        step_name: &str,
        target_status: &str,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!(
                    "Step '{}' of job {} did not reach '{}' within {:?}",
                    step_name,
                    job_id,
                    target_status,
                    timeout
                );
            }
            let steps = self.get_steps(job_id).await?;
            if let Some(step) = steps
                .iter()
                .find(|s| s["step_name"].as_str() == Some(step_name))
            {
                if step["status"].as_str() == Some(target_status) {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Stop the worker process (cancel its token) without stopping the server.
    /// Used by tests that need to simulate worker disappearance.
    pub async fn stop_worker(&mut self) {
        if let Some(mut handle) = self.worker_handle.take() {
            handle.abort();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut handle).await;
        }
    }

    /// Clean shutdown: cancel all tasks, await handles.
    pub async fn shutdown(&mut self) {
        self.cancel_token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.server_handle).await;
        if let Some(mut handle) = self.worker_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), &mut handle).await;
        }
    }
}
