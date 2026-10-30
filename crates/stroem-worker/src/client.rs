use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use stroem_common::file_field::ClaimedInputFile;
use stroem_common::secret::Secret;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// `/worker/jobs/{id}/logs` keeps axum's default 2 MiB request body limit; keep every push
/// comfortably under it so a fast step's buffered lines are never rejected with 413 and lost.
const LOG_PUSH_MAX_BYTES: usize = 1024 * 1024;

/// An input file answered 503 (storage not configured or transiently
/// unavailable) is retried every `INPUT_503_DELAY`, up to `INPUT_503_ATTEMPTS`
/// requests — the pinned-tarball policy (spec 2026-10-29-file-inputs § 6.1).
pub(crate) const INPUT_503_ATTEMPTS: u32 = 12;
pub(crate) const INPUT_503_DELAY: Duration = Duration::from_secs(5);

/// A fresh, absolute inputs directory for one claim request (spec § 5.2).
/// Nothing is created here: the poller creates it only when the claim
/// returns files to place.
fn fresh_inputs_dir() -> String {
    std::env::temp_dir()
        .join(format!("stroem-inputs-{}", Uuid::new_v4().simple()))
        .to_string_lossy()
        .into_owned()
}

/// HTTP client for communicating with the Strøm server
#[derive(Clone)]
pub struct ServerClient {
    client: reqwest::Client,
    base_url: Arc<str>,
    token: Arc<str>,
    /// Longer timeout for tarball downloads (default: 10 minutes)
    download_timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimedStep {
    pub job_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub step_name: String,
    pub action_name: String,
    pub action_type: String,
    pub action_image: Option<String>,
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub action_spec: Option<Secret<serde_json::Value>>,
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub input: Option<Secret<serde_json::Value>>,
    pub runner: Option<String>,
    pub timeout_secs: Option<i32>,
    pub revision: Option<String>,
    /// Name of the provider to use for agent steps (e.g. "anthropic").
    #[serde(default)]
    pub agent_provider_name: Option<String>,
    /// Rendered prompt for agent steps.
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub agent_prompt: Option<Secret<String>>,
    /// Rendered system prompt for agent steps.
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub agent_system_prompt: Option<Secret<String>>,
    /// MCP server definitions for agent steps with MCP tools.
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub mcp_servers: Option<
        Secret<std::collections::HashMap<String, stroem_common::models::workflow::McpServerDef>>,
    >,
    /// Persisted conversation state for resuming suspended agent steps.
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub agent_state: Option<Secret<serde_json::Value>>,
    /// Task tool metadata keyed by task name.
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub agent_tool_tasks: Option<Secret<serde_json::Value>>,
    /// Event source configuration, present when this is an event source consumer job.
    #[serde(
        default,
        serialize_with = "stroem_common::secret::serialize_opt_secret"
    )]
    pub event_source_config: Option<Secret<serde_json::Value>>,
    /// Storage key for the previous state snapshot (worker downloads to populate /state).
    #[serde(default)]
    pub state_storage_key: Option<String>,
    /// Whether the previous state snapshot contains a structured state.json file.
    #[serde(default)]
    pub state_has_json: Option<bool>,
    /// Storage key for the previous global workspace state snapshot (worker downloads to
    /// populate /global-state).
    #[serde(default)]
    pub global_state_storage_key: Option<String>,
    /// Whether the previous global state snapshot contains a structured state.json file.
    #[serde(default)]
    pub global_state_has_json: Option<bool>,
    /// Input files of the job this step exposes (spec § 5.4); empty for agent
    /// steps and on an older server.
    #[serde(default)]
    pub input_files: Vec<ClaimedInputFile>,
    /// The absolute inputs directory the claim request announced. The server
    /// rendered local paths under it; the worker creates it only when it
    /// places files (local and Docker steps).
    #[serde(default)]
    pub inputs_dir: Option<String>,
}

/// Raw claim response from server (job_id is Option since it's null when no work)
#[derive(Debug, Deserialize)]
struct ClaimResponse {
    pub workspace: Option<String>,
    pub job_id: Option<String>,
    pub task_name: Option<String>,
    pub step_name: Option<String>,
    pub action_name: Option<String>,
    pub action_type: Option<String>,
    pub action_image: Option<String>,
    pub action_spec: Option<Secret<serde_json::Value>>,
    pub input: Option<Secret<serde_json::Value>>,
    pub runner: Option<String>,
    pub timeout_secs: Option<i32>,
    pub revision: Option<String>,
    pub agent_provider_name: Option<String>,
    pub agent_prompt: Option<Secret<String>>,
    pub agent_system_prompt: Option<Secret<String>>,
    pub mcp_servers: Option<
        Secret<std::collections::HashMap<String, stroem_common::models::workflow::McpServerDef>>,
    >,
    pub agent_state: Option<Secret<serde_json::Value>>,
    pub agent_tool_tasks: Option<Secret<serde_json::Value>>,
    pub event_source_config: Option<Secret<serde_json::Value>>,
    pub state_storage_key: Option<String>,
    pub state_has_json: Option<bool>,
    pub global_state_storage_key: Option<String>,
    pub global_state_has_json: Option<bool>,
    #[serde(default)]
    pub input_files: Vec<ClaimedInputFile>,
}

#[derive(Debug, Serialize)]
struct RegisterRequest {
    name: String,
    capabilities: Vec<String>,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    exclusive: bool,
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    worker_id: Uuid,
}

#[derive(Debug, Serialize)]
struct HeartbeatRequest {
    worker_id: Uuid,
}

#[derive(Debug, Serialize)]
struct ClaimRequest {
    worker_id: Uuid,
    capabilities: Vec<String>,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    exclusive: bool,
    /// Present = this worker supports file inputs (spec § 5.2, § 6.5).
    #[serde(skip_serializing_if = "Option::is_none")]
    inputs_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct StepCompleteRequest {
    exit_code: i32,
    output: Option<serde_json::Value>,
    error: Option<String>,
}

/// The server answered 503 for a workspace tarball: its PinStore could not
/// load the pinned revision yet (spec § 5.4, e.g. git briefly unreachable on
/// that replica). `WorkspaceCache::ensure_revision` retries it.
#[derive(Debug)]
pub struct TarballUnavailable;

impl std::fmt::Display for TarballUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "workspace tarball temporarily unavailable (503)")
    }
}

impl std::error::Error for TarballUnavailable {}

/// The server answered 503 for an input file (file storage not configured,
/// or transiently unavailable). `download_input_file_with_retry` retries it.
#[derive(Debug)]
pub struct InputUnavailable {
    pub file_id: String,
}

impl std::fmt::Display for InputUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "input file {} temporarily unavailable (503)",
            self.file_id
        )
    }
}

impl std::error::Error for InputUnavailable {}

/// Download one input file, retrying ONLY a 503 (`InputUnavailable`) every
/// `delay`, up to `attempts` requests. Any other result (success, 404, a
/// size or digest mismatch, a transport error) is returned at once.
pub(crate) async fn download_input_file_with_retry(
    client: &ServerClient,
    job_id: Uuid,
    file: &ClaimedInputFile,
    dest: &Path,
    attempts: u32,
    delay: Duration,
) -> Result<()> {
    let mut attempt = 1;
    loop {
        match client.download_input_file(job_id, file, dest).await {
            Err(e) if attempt < attempts && e.downcast_ref::<InputUnavailable>().is_some() => {
                tracing::warn!(
                    %job_id,
                    file_id = %file.file_id,
                    attempt,
                    "Input file unavailable on the server (503), retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

impl ServerClient {
    pub fn new(
        base_url: &str,
        token: &str,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(connect_timeout_secs.unwrap_or(10)))
            .timeout(Duration::from_secs(request_timeout_secs.unwrap_or(30)))
            .build()
            .expect("Failed to build HTTP client");
        Self {
            client,
            base_url: Arc::from(base_url),
            token: Arc::from(token),
            download_timeout: Duration::from_secs(600),
        }
    }

    async fn check_response(
        response: reqwest::Response,
        context: &str,
    ) -> Result<reqwest::Response> {
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read body".to_string());
            anyhow::bail!("{} failed with status {}: {}", context, status, body);
        }
        Ok(response)
    }

    /// Register this worker with the server
    #[tracing::instrument(skip(self))]
    pub async fn register(
        &self,
        name: &str,
        capabilities: &[String],
        tags: &[String],
        exclusive: bool,
        version: Option<&str>,
    ) -> Result<Uuid> {
        let url = format!("{}/worker/register", self.base_url);
        let req = RegisterRequest {
            name: name.to_string(),
            capabilities: capabilities.to_vec(),
            tags: tags.to_vec(),
            exclusive,
            version: version.map(|v| v.to_string()),
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .context("Failed to send register request")?;

        let response = Self::check_response(response, "Register").await?;

        let resp: RegisterResponse = response
            .json()
            .await
            .context("Failed to parse register response")?;
        Ok(resp.worker_id)
    }

    /// Send a heartbeat to keep the worker alive
    #[tracing::instrument(skip(self))]
    pub async fn heartbeat(&self, worker_id: Uuid) -> Result<()> {
        let url = format!("{}/worker/heartbeat", self.base_url);
        let req = HeartbeatRequest { worker_id };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .context("Failed to send heartbeat request")?;

        Self::check_response(response, "Heartbeat").await?;

        Ok(())
    }

    /// Attempt to claim a step to execute
    #[tracing::instrument(skip(self))]
    pub async fn claim_step(
        &self,
        worker_id: Uuid,
        capabilities: &[String],
        tags: &[String],
        exclusive: bool,
    ) -> Result<Option<ClaimedStep>> {
        let url = format!("{}/worker/jobs/claim", self.base_url);
        let inputs_dir = fresh_inputs_dir();
        let req = ClaimRequest {
            worker_id,
            capabilities: capabilities.to_vec(),
            tags: tags.to_vec(),
            exclusive,
            inputs_dir: Some(inputs_dir.clone()),
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .context("Failed to send claim request")?;

        let response = Self::check_response(response, "Claim").await?;

        let resp: ClaimResponse = response
            .json()
            .await
            .context("Failed to parse claim response")?;

        // No work available if job_id is None
        let job_id_str = match resp.job_id {
            Some(id) => id,
            None => return Ok(None),
        };

        let step = ClaimedStep {
            job_id: Uuid::parse_str(&job_id_str).context("Invalid job_id in claim response")?,
            workspace: resp
                .workspace
                .context("Missing workspace in claim response")?,
            task_name: resp.task_name.unwrap_or_default(),
            step_name: resp
                .step_name
                .context("Missing step_name in claim response")?,
            action_name: resp
                .action_name
                .context("Missing action_name in claim response")?,
            action_type: resp
                .action_type
                .context("Missing action_type in claim response")?,
            action_image: resp.action_image,
            action_spec: resp.action_spec,
            input: resp.input,
            runner: resp.runner,
            timeout_secs: resp.timeout_secs,
            revision: resp.revision,
            agent_provider_name: resp.agent_provider_name,
            agent_prompt: resp.agent_prompt,
            agent_system_prompt: resp.agent_system_prompt,
            mcp_servers: resp.mcp_servers,
            agent_state: resp.agent_state,
            agent_tool_tasks: resp.agent_tool_tasks,
            event_source_config: resp.event_source_config,
            state_storage_key: resp.state_storage_key,
            state_has_json: resp.state_has_json,
            global_state_storage_key: resp.global_state_storage_key,
            global_state_has_json: resp.global_state_has_json,
            input_files: resp.input_files,
            inputs_dir: Some(inputs_dir),
        };

        Ok(Some(step))
    }

    /// Report that a step has started
    #[tracing::instrument(skip(self))]
    pub async fn report_step_start(
        &self,
        job_id: Uuid,
        step_name: &str,
        worker_id: Uuid,
    ) -> Result<()> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/start",
            self.base_url, job_id, step_name
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "worker_id": worker_id.to_string() }))
            .send()
            .await
            .context("Failed to send step start request")?;

        Self::check_response(response, "Step start").await?;

        Ok(())
    }

    /// Report that a step has completed
    #[tracing::instrument(skip(self, output))]
    pub async fn report_step_complete(
        &self,
        job_id: Uuid,
        step_name: &str,
        exit_code: i32,
        output: Option<serde_json::Value>,
        error: Option<String>,
    ) -> Result<()> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/complete",
            self.base_url, job_id, step_name
        );
        let req = StepCompleteRequest {
            exit_code,
            output,
            error,
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .context("Failed to send step complete request")?;

        Self::check_response(response, "Step complete").await?;

        Ok(())
    }

    /// Download workspace tarball from the server, returns (bytes, revision)
    ///
    /// Sends `If-None-Match` header if a cached revision is provided.
    /// Returns `Ok(None)` if the server returns 304 Not Modified.
    ///
    /// When `pin_revision` is `Some`, appends `?revision={rev}` to request a
    /// specific historical revision rather than the latest. In this mode the
    /// caller should pass `cached_revision: None` (the server never returns 304
    /// for pinned requests).
    #[tracing::instrument(skip(self))]
    pub async fn download_workspace_tarball(
        &self,
        workspace: &str,
        cached_revision: Option<&str>,
        pin_revision: Option<&str>,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let url = format!("{}/worker/workspace/{}.tar.gz", self.base_url, workspace);

        let mut req = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            // Override the default request timeout — tarball downloads can be large
            .timeout(self.download_timeout);

        if let Some(rev) = pin_revision {
            req = req.query(&[("revision", rev)]);
        }

        if let Some(rev) = cached_revision {
            req = req.header("If-None-Match", format!("\"{}\"", rev));
        }

        let response = req
            .send()
            .await
            .context("Failed to download workspace tarball")?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }

        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(anyhow::Error::new(TarballUnavailable));
        }

        let response = Self::check_response(response, "Download workspace").await?;

        let revision = response
            .headers()
            .get("X-Revision")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = response
            .bytes()
            .await
            .context("Failed to read workspace tarball bytes")?;

        Ok(Some((bytes.to_vec(), revision)))
    }

    /// Download one input file of `job_id` to `dest`
    /// (`{inputs_dir}/{file_id}/{safe_name}`, spec § 6.1): the body is
    /// streamed to a temp file in `dest`'s directory, checked against the
    /// claimed size and SHA-256, made read-only (`0444`) and renamed into
    /// place. On any error no file is left behind. A 503 is an
    /// [`InputUnavailable`] error. Errors name the file id, never its name.
    #[tracing::instrument(skip(self, file, dest), fields(file_id = %file.file_id))]
    pub async fn download_input_file(
        &self,
        job_id: Uuid,
        file: &ClaimedInputFile,
        dest: &Path,
    ) -> Result<()> {
        let id = &file.file_id;
        let dir = dest
            .parent()
            .with_context(|| format!("input file {id}: the destination has no directory"))?;
        let part = dir.join(format!(".part-{}", Uuid::new_v4().simple()));
        let mut result = self.fetch_input_file(job_id, file, &part).await;
        if result.is_ok() {
            result = tokio::fs::rename(&part, dest)
                .await
                .with_context(|| format!("input file {id}: cannot move it into place"));
        }
        if result.is_err() {
            // Best effort: the part file may not exist (failed before creation).
            let _ = tokio::fs::remove_file(&part).await;
        }
        result
    }

    /// Stream one input file into `part` and verify it (see `download_input_file`).
    async fn fetch_input_file(
        &self,
        job_id: Uuid,
        file: &ClaimedInputFile,
        part: &Path,
    ) -> Result<()> {
        let id = &file.file_id;
        let url = format!(
            "{}/worker/jobs/{}/input-files/{}",
            self.base_url, job_id, id
        );
        let mut response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            // Input files can be large (100 MiB by default), like tarballs.
            .timeout(self.download_timeout)
            .send()
            .await
            .with_context(|| format!("input file {id}: request failed"))?;

        let status = response.status();
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(anyhow::Error::new(InputUnavailable {
                file_id: id.clone(),
            }));
        }
        if !status.is_success() {
            anyhow::bail!("input file {id}: download failed with status {status}");
        }

        let mut out = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(part)
            .await
            .with_context(|| format!("input file {id}: cannot create the download file"))?;
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("input file {id}: reading the download failed"))?
        {
            written += chunk.len() as u64;
            // Stop at once: a body longer than claimed must not fill the disk.
            if written > file.size_bytes {
                anyhow::bail!(
                    "input file {id}: size mismatch (more than the expected {} bytes)",
                    file.size_bytes
                );
            }
            hasher.update(&chunk);
            out.write_all(&chunk)
                .await
                .with_context(|| format!("input file {id}: writing the download failed"))?;
        }
        out.flush()
            .await
            .with_context(|| format!("input file {id}: writing the download failed"))?;
        drop(out);

        if written != file.size_bytes {
            anyhow::bail!(
                "input file {id}: size mismatch (expected {} bytes, got {written})",
                file.size_bytes
            );
        }
        let digest = hex::encode(hasher.finalize());
        if !digest.eq_ignore_ascii_case(&file.sha256) {
            anyhow::bail!("input file {id}: SHA-256 mismatch");
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(part, std::fs::Permissions::from_mode(0o444))
                .await
                .with_context(|| format!("input file {id}: cannot make it read-only"))?;
        }
        Ok(())
    }

    /// Check if a job has been cancelled on the server
    #[tracing::instrument(skip(self))]
    pub async fn check_job_cancelled(&self, job_id: Uuid) -> Result<bool> {
        let url = format!("{}/worker/jobs/{}/cancelled", self.base_url, job_id);

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to check job cancellation")?;

        let response = Self::check_response(response, "Check cancelled").await?;

        #[derive(Deserialize)]
        struct CancelledResponse {
            cancelled: bool,
        }

        let resp: CancelledResponse = response
            .json()
            .await
            .context("Failed to parse cancellation response")?;

        Ok(resp.cancelled)
    }

    /// POST /worker/jobs/{id}/steps/{step}/task-tool
    ///
    /// Request the server to create a child job for a task tool call.
    /// Returns the child job's UUID.
    #[tracing::instrument(skip(self, input))]
    pub async fn agent_task_tool(
        &self,
        job_id: Uuid,
        step_name: &str,
        task_name: &str,
        input: serde_json::Value,
    ) -> anyhow::Result<Uuid> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/task-tool",
            self.base_url, job_id, step_name
        );
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({
                "task_name": task_name,
                "input": input,
            }))
            .send()
            .await
            .context("agent_task_tool request failed")?;
        let resp = Self::check_response(resp, "Agent task-tool").await?;
        let body: serde_json::Value = resp.json().await.context("parse task-tool response")?;
        let child_id = body["child_job_id"]
            .as_str()
            .context("missing child_job_id in response")?;
        Uuid::parse_str(child_id).context("invalid child_job_id UUID")
    }

    /// POST /worker/jobs/{id}/steps/{step}/suspend
    ///
    /// Suspend an agent step for `ask_user` — saves conversation state and
    /// marks the step as `suspended` on the server.
    #[tracing::instrument(skip(self, agent_state))]
    pub async fn agent_suspend_step(
        &self,
        job_id: Uuid,
        step_name: &str,
        agent_state: serde_json::Value,
        message: &str,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/suspend",
            self.base_url, job_id, step_name
        );
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({
                "agent_state": agent_state,
                "message": message,
            }))
            .send()
            .await
            .context("agent_suspend_step request failed")?;
        Self::check_response(resp, "Agent suspend step").await?;
        Ok(())
    }

    /// POST /worker/jobs/{id}/steps/{step}/agent-state
    ///
    /// Save intermediate agent conversation state (pending tool calls) so the
    /// step can be resumed after task-tool child jobs complete.
    #[tracing::instrument(skip(self, agent_state))]
    pub async fn agent_save_state(
        &self,
        job_id: Uuid,
        step_name: &str,
        agent_state: serde_json::Value,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/agent-state",
            self.base_url, job_id, step_name
        );
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({
                "agent_state": agent_state,
            }))
            .send()
            .await
            .context("agent_save_state request failed")?;
        Self::check_response(resp, "Agent save state").await?;
        Ok(())
    }

    /// Emit an event-source trigger to the server, creating a new job.
    ///
    /// Returns the ID of the created job.
    #[tracing::instrument(skip(self, input))]
    pub async fn emit_event_source(
        &self,
        workspace: &str,
        task: &str,
        input: serde_json::Value,
        source_id: &str,
    ) -> Result<String> {
        let url = format!("{}/worker/event-source/emit", self.base_url);
        let body = serde_json::json!({
            "workspace": workspace,
            "task": task,
            "input": input,
            "source_id": source_id,
        });

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&body)
            .send()
            .await
            .context("Failed to send event-source emit request")?;

        let response = Self::check_response(response, "Event-source emit").await?;

        #[derive(serde::Deserialize)]
        struct EmitResponse {
            job_id: String,
        }

        let resp: EmitResponse = response
            .json()
            .await
            .context("Failed to parse event-source emit response")?;

        Ok(resp.job_id)
    }

    /// Download the latest state tarball for a task. With `job_id` the server
    /// reads that job's own `(workspace, task, ref)` partition and ignores the
    /// path (spec § 7.6). Returns `None` if no state exists (204).
    #[tracing::instrument(skip(self))]
    pub async fn download_state_tarball(
        &self,
        workspace: &str,
        task_name: &str,
        job_id: Option<Uuid>,
    ) -> Result<Option<Vec<u8>>> {
        let mut url = format!("{}/worker/state/{}/{}", self.base_url, workspace, task_name);
        if let Some(id) = job_id {
            url.push_str(&format!("?job_id={id}"));
        }
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .timeout(self.download_timeout)
            .send()
            .await
            .context("Failed to download state tarball")?;

        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }

        let response = Self::check_response(response, "Download state").await?;
        let bytes = response
            .bytes()
            .await
            .context("Failed to read state tarball body")?;
        Ok(Some(bytes.to_vec()))
    }

    /// Upload a new state tarball after step completion.
    /// `has_json` indicates whether the tarball contains a structured `state.json` file.
    #[tracing::instrument(skip(self, data))]
    pub async fn upload_state_tarball(
        &self,
        workspace: &str,
        task_name: &str,
        job_id: Uuid,
        data: Vec<u8>,
        has_json: bool,
    ) -> Result<()> {
        let url = format!(
            "{}/worker/state/{}/{}/{}?has_json={}",
            self.base_url, workspace, task_name, job_id, has_json
        );
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/gzip")
            .body(data)
            .send()
            .await
            .context("Failed to upload state tarball")?;

        Self::check_response(response, "Upload state").await?;
        Ok(())
    }

    /// Download the latest global workspace state tarball. With `job_id` the
    /// server reads that job's own `(workspace, ref)` partition (spec § 7.6).
    /// Returns `None` if no global state exists (204).
    #[tracing::instrument(skip(self))]
    pub async fn download_global_state_tarball(
        &self,
        workspace: &str,
        job_id: Option<Uuid>,
    ) -> Result<Option<Vec<u8>>> {
        let mut url = format!("{}/worker/global-state/{}", self.base_url, workspace);
        if let Some(id) = job_id {
            url.push_str(&format!("?job_id={id}"));
        }
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .timeout(self.download_timeout)
            .send()
            .await
            .context("Failed to download global state tarball")?;

        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }

        let response = Self::check_response(response, "Download global state").await?;
        let bytes = response
            .bytes()
            .await
            .context("Failed to read global state tarball body")?;
        Ok(Some(bytes.to_vec()))
    }

    /// Upload a new global workspace state tarball after step completion.
    /// `has_json` indicates whether the tarball contains a structured `state.json` file.
    #[tracing::instrument(skip(self, data))]
    pub async fn upload_global_state_tarball(
        &self,
        workspace: &str,
        job_id: Uuid,
        data: Vec<u8>,
        has_json: bool,
    ) -> Result<()> {
        let url = format!(
            "{}/worker/global-state/{}/{}?has_json={}",
            self.base_url, workspace, job_id, has_json
        );
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/gzip")
            .body(data)
            .send()
            .await
            .context("Failed to upload global state tarball")?;

        Self::check_response(response, "Upload global state").await?;
        Ok(())
    }

    /// Upload a single artifact file to the server.
    ///
    /// Mirrors `upload_state_tarball`: the body is the raw file contents and
    /// `content_type` becomes the row's recorded MIME type. The server
    /// enforces per-file and per-job size caps; on rejection the response
    /// body is surfaced verbatim so the caller can log it.
    ///
    /// Body is `bytes::Bytes` so that retry loops can clone the request body
    /// as a refcount bump rather than re-allocating the full file each time.
    #[tracing::instrument(skip(self, body))]
    pub async fn upload_artifact(
        &self,
        job_id: Uuid,
        step_name: &str,
        name: &str,
        content_type: &str,
        body: bytes::Bytes,
    ) -> Result<()> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/artifacts/{}",
            self.base_url, job_id, step_name, name
        );
        let content_length = body.len();
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CONTENT_LENGTH, content_length)
            .body(body)
            .send()
            .await
            .context("upload_artifact request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("upload_artifact {status}: {body}");
        }
        Ok(())
    }

    /// Delete every artifact recorded for `(job_id, step_name)` plus the matching
    /// blob subtree. Used by the worker to roll back a partial upload when
    /// `upload_artifact` fails after some files have already landed.
    #[tracing::instrument(skip(self))]
    pub async fn delete_step_artifacts(&self, job_id: Uuid, step_name: &str) -> Result<()> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/artifacts",
            self.base_url, job_id, step_name
        );
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("delete_step_artifacts request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("delete_step_artifacts {status}: {body}");
        }
        Ok(())
    }

    /// Push log lines to the server.
    ///
    /// A step's buffered lines can arrive here all at once (e.g. a fast step's final flush,
    /// which never hits the periodic 1s pusher) — sent as a single request, that can exceed the
    /// server's default 2 MiB body limit and be rejected with 413, silently dropping the whole
    /// batch. Split into sequential requests of at most `LOG_PUSH_MAX_BYTES` each instead; a
    /// single entry larger than the cap is sent alone (the server may still reject it).
    #[tracing::instrument(skip(self, lines))]
    pub async fn push_logs(
        &self,
        job_id: Uuid,
        step_name: &str,
        lines: Vec<serde_json::Value>,
    ) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }

        // Build structured log line entries for the server
        let structured_lines: Vec<serde_json::Value> = lines
            .iter()
            .map(|v| {
                serde_json::json!({
                    "ts": v.get("timestamp").and_then(|t| t.as_str()).unwrap_or(""),
                    "stream": v.get("stream").and_then(|s| s.as_str()).unwrap_or("stdout"),
                    "line": v.get("line").and_then(|l| l.as_str()).unwrap_or(""),
                })
            })
            .collect();

        // Envelope skeleton `{"lines":[...],"step_name":"..."}`, everything
        // except the lines and the step name's own bytes, is exactly 25
        // bytes: `{"lines":[` (10) + `]` (1) + `,"step_name":` (13) + `}`
        // (1). The step name must be measured by its escaped, QUOTED JSON
        // length — the same bytes `send_log_chunk`'s `.json(&..)` puts on
        // the wire — not its raw `.len()`: a name full of characters that
        // expand under JSON escaping (`"`, `\`, control bytes) otherwise
        // undercounts the envelope and can push a tightly packed chunk's
        // real body over the cap. This makes "never over cap" exactly true
        // (not just generous): +8 bytes of slack for anything this count
        // missed still leaves the real body strictly under the cap (see
        // `test_push_logs_envelope_counts_escaped_step_name`).
        const ENVELOPE_SKELETON_BYTES: usize = 25;
        let step_name_json_len = serde_json::to_string(step_name)
            .context("Failed to measure step name size")?
            .len();
        let envelope_overhead = step_name_json_len + ENVELOPE_SKELETON_BYTES + 8;

        let mut chunks: Vec<Vec<serde_json::Value>> = Vec::new();
        let mut current: Vec<serde_json::Value> = Vec::new();
        let mut current_bytes = envelope_overhead;

        for entry in structured_lines {
            let entry_bytes = serde_json::to_vec(&entry)
                .context("Failed to measure log entry size")?
                .len()
                + 1; // separating comma
            if !current.is_empty() && current_bytes + entry_bytes > LOG_PUSH_MAX_BYTES {
                chunks.push(std::mem::take(&mut current));
                current_bytes = envelope_overhead;
            }
            current_bytes += entry_bytes;
            current.push(entry);
        }
        if !current.is_empty() {
            chunks.push(current);
        }

        let total = chunks.len();
        for (idx, chunk) in chunks.into_iter().enumerate() {
            self.send_log_chunk(job_id, step_name, chunk)
                .await
                .with_context(|| format!("chunk {} of {}", idx + 1, total))?;
        }

        Ok(())
    }

    /// Send one `push_logs` request (a chunk of already-structured entries).
    async fn send_log_chunk(
        &self,
        job_id: Uuid,
        step_name: &str,
        lines: Vec<serde_json::Value>,
    ) -> Result<()> {
        let url = format!("{}/worker/jobs/{}/logs", self.base_url, job_id);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "lines": lines, "step_name": step_name }))
            .send()
            .await
            .context("Failed to send logs request")?;

        Self::check_response(response, "Push logs").await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_with_default_timeouts() {
        let client = ServerClient::new("http://localhost:8080", "token", None, None);
        assert_eq!(&*client.base_url, "http://localhost:8080");
        assert_eq!(&*client.token, "token");
        assert_eq!(client.download_timeout, Duration::from_secs(600));
    }

    #[test]
    fn test_new_with_custom_timeouts() {
        let client = ServerClient::new("http://localhost:8080", "token", Some(5), Some(60));
        assert_eq!(&*client.base_url, "http://localhost:8080");
        assert_eq!(&*client.token, "token");
    }

    #[test]
    fn test_client_is_clone() {
        let client = ServerClient::new("http://localhost:8080", "token", None, None);
        let cloned = client.clone();
        assert_eq!(&*cloned.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_claim_response_missing_task_name_defaults_to_none() {
        // Simulates an older server that does not emit task_name or revision
        let json = serde_json::json!({
            "job_id": "abc-123",
            "workspace": "default",
            "step_name": "build",
            "action_name": "run",
            "action_type": "script",
            "runner": "local"
        });
        let resp: ClaimResponse = serde_json::from_value(json).unwrap();
        assert!(resp.task_name.is_none());
        assert!(resp.revision.is_none());
    }

    #[test]
    fn test_claim_response_with_task_name() {
        let json = serde_json::json!({
            "job_id": "abc-123",
            "workspace": "default",
            "task_name": "deploy-api",
            "step_name": "build",
            "action_name": "run",
            "action_type": "script",
            "runner": "local"
        });
        let resp: ClaimResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.task_name, Some("deploy-api".to_string()));
        assert!(resp.revision.is_none());
    }

    #[test]
    fn test_claim_response_null_task_name() {
        let json = serde_json::json!({
            "job_id": "abc-123",
            "workspace": "default",
            "task_name": null,
            "step_name": "build",
            "action_name": "run",
            "action_type": "script",
            "runner": "local"
        });
        let resp: ClaimResponse = serde_json::from_value(json).unwrap();
        assert!(resp.task_name.is_none());
        assert!(resp.revision.is_none());
    }

    #[test]
    fn test_claim_response_with_revision() {
        let json = serde_json::json!({
            "job_id": "abc-123",
            "workspace": "default",
            "task_name": "deploy-api",
            "step_name": "build",
            "action_name": "run",
            "action_type": "script",
            "runner": "local",
            "revision": "abc123"
        });
        let resp: ClaimResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.revision, Some("abc123".to_string()));
    }

    /// Security regression (prod 2026-09-02): `execute_step` is instrumented on the
    /// full `ClaimedStep`, so anything its `Debug` prints lands in every worker log
    /// line. Rendered secrets live in `action_spec.env`, resolved connection values
    /// in `input`, and prompts/state/MCP definitions can embed credentials too.
    /// `Debug` must never print any of them.
    #[test]
    fn test_claimed_step_debug_redacts_sensitive_fields() {
        const SENTINEL: &str = "SUPER-SECRET-VALUE-7f3a";
        let mut mcp = std::collections::HashMap::new();
        mcp.insert(
            "srv".to_string(),
            stroem_common::models::workflow::McpServerDef {
                transport: "stdio".to_string(),
                auth_token: Some(SENTINEL.to_string()),
                ..Default::default()
            },
        );
        let step = ClaimedStep {
            job_id: Uuid::new_v4(),
            workspace: "default".to_string(),
            task_name: "deploy".to_string(),
            step_name: "publish".to_string(),
            action_name: "publish".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(Secret::new(
                serde_json::json!({"env": {"DB_PASSWORD": SENTINEL}}),
            )),
            input: Some(Secret::new(
                serde_json::json!({"conn": {"password": SENTINEL}}),
            )),
            runner: Some("local".to_string()),
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: Some(Secret::new(format!("use {SENTINEL}"))),
            agent_system_prompt: Some(Secret::new(format!("key={SENTINEL}"))),
            mcp_servers: Some(Secret::new(mcp)),
            agent_state: Some(Secret::new(serde_json::json!({"tool_result": SENTINEL}))),
            agent_tool_tasks: Some(Secret::new(serde_json::json!({"t": SENTINEL}))),
            event_source_config: Some(Secret::new(serde_json::json!({"env": {"K": SENTINEL}}))),
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
            input_files: Vec::new(),
            inputs_dir: None,
        };

        let dbg = format!("{step:?}");
        assert!(
            !dbg.contains(SENTINEL),
            "Debug output leaked a secret: {dbg}"
        );
        // Identity fields must survive so logs stay correlatable.
        assert!(
            dbg.contains("publish"),
            "step identity missing from Debug: {dbg}"
        );
    }

    #[test]
    fn test_claimed_step_revision_field() {
        let step_with_revision = ClaimedStep {
            job_id: uuid::Uuid::nil(),
            workspace: "default".to_string(),
            task_name: "task".to_string(),
            step_name: "step".to_string(),
            action_name: "action".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            runner: None,
            timeout_secs: None,
            revision: Some("deadbeef".to_string()),
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
            event_source_config: None,
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
            input_files: Vec::new(),
            inputs_dir: None,
        };
        let json = serde_json::to_value(&step_with_revision).unwrap();
        assert_eq!(json["revision"], "deadbeef");

        let roundtripped: ClaimedStep = serde_json::from_value(json).unwrap();
        assert_eq!(roundtripped.revision, Some("deadbeef".to_string()));

        // Without revision field
        let step_without_revision = ClaimedStep {
            revision: None,
            ..step_with_revision
        };
        let json2 = serde_json::to_value(&step_without_revision).unwrap();
        let roundtripped2: ClaimedStep = serde_json::from_value(json2).unwrap();
        assert!(roundtripped2.revision.is_none());
    }

    #[test]
    fn test_claim_response_backward_compat_no_revision() {
        // Simulates a server version that predates revision pinning
        let json = serde_json::json!({
            "job_id": "abc-123",
            "workspace": "default",
            "task_name": "deploy",
            "step_name": "build",
            "action_name": "run",
            "action_type": "script",
            "runner": "local"
        });
        let resp: ClaimResponse = serde_json::from_value(json).unwrap();
        assert!(
            resp.revision.is_none(),
            "missing revision field must default to None for backward compat"
        );
    }

    // --- push_logs chunking (regression for the big-log 413, 2026-09-23) ---

    /// A batch too big for one request must be split into multiple requests, each within
    /// `LOG_PUSH_MAX_BYTES`, and the concatenated `lines` across requests must equal the
    /// input in order — this is what protects a fast step's final flush from a silent 413.
    #[tokio::test]
    async fn test_push_logs_chunks_large_batch_under_cap() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/worker/jobs/.+/logs"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let job_id = Uuid::new_v4();

        // ~1 KB lines; enough entries to build a ~3 MiB batch (> 3x the 1 MiB cap).
        let line_text = "x".repeat(1000);
        let n = 3200;
        let lines: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "timestamp": "2026-01-01T00:00:00Z",
                    "stream": "stdout",
                    "line": format!("{line_text}-{i}"),
                })
            })
            .collect();

        client
            .push_logs(job_id, "print", lines)
            .await
            .expect("push_logs should succeed across all chunks");

        let received = mock.received_requests().await.unwrap();
        assert!(
            received.len() >= 3,
            "expected at least 3 chunked requests for a ~3 MiB batch, got {}",
            received.len()
        );

        let mut all_lines: Vec<String> = Vec::new();
        for req in &received {
            assert!(
                req.body.len() <= LOG_PUSH_MAX_BYTES,
                "request body {} bytes exceeds cap {}",
                req.body.len(),
                LOG_PUSH_MAX_BYTES
            );
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(
                body["step_name"], "print",
                "every chunk must carry step_name"
            );
            for entry in body["lines"].as_array().unwrap() {
                all_lines.push(entry["line"].as_str().unwrap().to_string());
            }
        }

        let expected: Vec<String> = (0..n).map(|i| format!("{line_text}-{i}")).collect();
        assert_eq!(
            all_lines, expected,
            "concatenated chunk lines must equal the input, in order"
        );
    }

    /// A small batch is unchanged: exactly one request.
    #[tokio::test]
    async fn test_push_logs_small_batch_is_one_request() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/worker/jobs/.+/logs"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let lines = vec![serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "stream": "stdout",
            "line": "hello",
        })];

        client
            .push_logs(Uuid::new_v4(), "step", lines)
            .await
            .expect("a small batch must still succeed");
        // wiremock verifies `expect(1)` on drop.
    }

    /// A failure partway through must stop immediately (no further chunks sent) and the
    /// returned error must name which chunk failed.
    #[tokio::test]
    async fn test_push_logs_stops_at_first_failed_chunk() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Call 1 succeeds; call 2 fails. A would-be call 3 has no matching mock, so if
        // push_logs wrongly kept going after the failure, `received_requests` below would
        // show 3 entries instead of 2.
        Mock::given(method("POST"))
            .and(path_regex(r"/worker/jobs/.+/logs"))
            .respond_with(ResponseTemplate::new(200))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"/worker/jobs/.+/logs"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .up_to_n_times(1)
            .with_priority(2)
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let line_text = "x".repeat(1000);
        let n = 3200;
        let lines: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "timestamp": "2026-01-01T00:00:00Z",
                    "stream": "stdout",
                    "line": format!("{line_text}-{i}"),
                })
            })
            .collect();

        let result = client.push_logs(Uuid::new_v4(), "print", lines).await;
        let err = result.expect_err("second chunk failure must propagate");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("chunk 2 of"),
            "error should name the failing chunk, got: {msg}"
        );

        let received = mock.received_requests().await.unwrap();
        assert_eq!(
            received.len(),
            2,
            "must stop after the failing chunk and never send a third, got {} requests",
            received.len()
        );
    }

    /// A single entry larger than the cap is sent in a chunk of its own, isolated from its
    /// neighbours (the server may still reject it — tracked as a residual gap in TODO.md).
    #[tokio::test]
    async fn test_push_logs_oversized_entry_sent_alone() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/worker/jobs/.+/logs"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let huge_line = "y".repeat(LOG_PUSH_MAX_BYTES + 1024);
        let lines = vec![
            serde_json::json!({"timestamp": "t", "stream": "stdout", "line": "small-before"}),
            serde_json::json!({"timestamp": "t", "stream": "stdout", "line": huge_line.clone()}),
            serde_json::json!({"timestamp": "t", "stream": "stdout", "line": "small-after"}),
        ];

        client
            .push_logs(Uuid::new_v4(), "print", lines)
            .await
            .expect("push_logs should still succeed (server accepts this mock's oversized body)");

        let received = mock.received_requests().await.unwrap();
        assert_eq!(
            received.len(),
            3,
            "the oversized entry must split into its own chunk, got {} requests",
            received.len()
        );

        let bodies: Vec<serde_json::Value> = received
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let mid_lines = bodies[1]["lines"].as_array().unwrap();
        assert_eq!(
            mid_lines.len(),
            1,
            "the oversized entry's chunk must contain only itself"
        );
        assert_eq!(mid_lines[0]["line"].as_str().unwrap(), huge_line);
    }

    /// A step name that expands under JSON escaping must be counted by its
    /// escaped, quoted length, not its raw one — an underestimate here packs
    /// lines tightly enough that the real (escaped) envelope can push a
    /// request over `LOG_PUSH_MAX_BYTES` (C8/C9 regression: 64 `"`
    /// characters raw-len 64 but escaped-quoted-len 130).
    #[tokio::test]
    async fn test_push_logs_envelope_counts_escaped_step_name() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/worker/jobs/.+/logs"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let job_id = Uuid::new_v4();

        // 64 `"` characters: each escapes to `\"` (2 bytes), so the quoted
        // step name is 2 + 64*2 = 130 bytes — far more than its raw 64,
        // which is exactly what the old `step_name.len() + 32` missed.
        let step_name = "\"".repeat(64);

        // Small, uniform lines so the packer's greedy boundary lands within
        // a few bytes of the cap on every chunk but the last — any gap the
        // escaped-name fix leaves unaccounted for shows up as an
        // over-cap request.
        let line_text = "x".repeat(40);
        let n = 40_000;
        let lines: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "timestamp": "2026-01-01T00:00:00Z",
                    "stream": "stdout",
                    "line": format!("{line_text}-{i}"),
                })
            })
            .collect();

        client
            .push_logs(job_id, &step_name, lines)
            .await
            .expect("push_logs should succeed across all chunks");

        let received = mock.received_requests().await.unwrap();
        assert!(
            received.len() >= 2,
            "expected multiple chunks, got {}",
            received.len()
        );
        for req in &received {
            assert!(
                req.body.len() <= LOG_PUSH_MAX_BYTES,
                "request body {} bytes exceeds cap {} with an escaped step name",
                req.body.len(),
                LOG_PUSH_MAX_BYTES
            );
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["step_name"], step_name);
        }
    }

    #[tokio::test]
    async fn test_download_state_tarball_sends_job_id() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let job_id = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/worker/state/billing/nightly"))
            .and(query_param("job_id", job_id.to_string()))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/worker/global-state/billing"))
            .and(query_param("job_id", job_id.to_string()))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        assert!(client
            .download_state_tarball("billing", "nightly", Some(job_id))
            .await
            .unwrap()
            .is_none());
        assert!(client
            .download_global_state_tarball("billing", Some(job_id))
            .await
            .unwrap()
            .is_none());
        // wiremock verifies `expect(1)` on drop.
    }

    #[tokio::test]
    async fn test_download_state_tarball_without_job_id_sends_no_query() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/worker/state/billing/nightly"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&mock)
            .await;
        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        client
            .download_state_tarball("billing", "nightly", None)
            .await
            .unwrap();
        let reqs = mock.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.query(), None);
    }

    // --- file inputs (spec 2026-10-29-file-inputs § 5.2, § 5.4, § 6.1) ---

    fn input_file(name: &str, body: &[u8]) -> ClaimedInputFile {
        use sha2::{Digest, Sha256};
        ClaimedInputFile {
            file_id: stroem_common::file_field::new_file_id(),
            safe_name: name.to_string(),
            size_bytes: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
        }
    }

    fn input_file_url_path(job_id: Uuid, file_id: &str) -> String {
        format!("/worker/jobs/{job_id}/input-files/{file_id}")
    }

    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Every claim request announces a fresh absolute `inputs_dir`; the
    /// worker creates nothing until a claim returns files to place.
    #[tokio::test]
    async fn claim_request_sends_absolute_inputs_dir() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/worker/jobs/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "job_id": null
            })))
            .expect(2)
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let caps = vec!["script".to_string()];
        for _ in 0..2 {
            let claimed = client
                .claim_step(Uuid::new_v4(), &caps, &[], false)
                .await
                .unwrap();
            assert!(claimed.is_none());
        }

        let reqs = mock.received_requests().await.unwrap();
        let dirs: Vec<String> = reqs
            .iter()
            .map(|r| {
                let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                body["inputs_dir"]
                    .as_str()
                    .expect("every claim request must carry inputs_dir")
                    .to_string()
            })
            .collect();
        assert_eq!(dirs.len(), 2);
        for dir in &dirs {
            let p = std::path::Path::new(dir);
            assert!(p.is_absolute(), "inputs_dir must be absolute: {dir}");
            assert!(
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("stroem-inputs-"),
                "unexpected inputs_dir name: {dir}"
            );
            assert!(!p.exists(), "nothing is created before files are claimed");
        }
        assert_ne!(dirs[0], dirs[1], "each claim request needs a fresh dir");
    }

    #[tokio::test]
    async fn claimed_step_parses_input_files() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let file = input_file("data.csv", b"a,b\n1,2\n");
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/worker/jobs/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "job_id": Uuid::new_v4().to_string(),
                "workspace": "default",
                "task_name": "import",
                "step_name": "load",
                "action_name": "load",
                "action_type": "script",
                "runner": "local",
                "input_files": [file],
            })))
            .mount(&mock)
            .await;

        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let step = client
            .claim_step(Uuid::new_v4(), &["script".to_string()], &[], false)
            .await
            .unwrap()
            .expect("a step is claimed");
        assert_eq!(step.input_files, vec![file]);

        let reqs = mock.received_requests().await.unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let sent_dir = sent["inputs_dir"].as_str().unwrap();
        assert_eq!(
            step.inputs_dir.as_deref(),
            Some(sent_dir),
            "the step keeps the dir its claim request announced"
        );
        assert!(!std::path::Path::new(sent_dir).exists());

        // An older server sends no `input_files`; an older serialized step has neither field.
        let resp: ClaimResponse = serde_json::from_value(serde_json::json!({
            "job_id": "abc-123",
            "workspace": "default",
            "step_name": "build",
            "action_name": "run",
            "action_type": "script"
        }))
        .unwrap();
        assert!(resp.input_files.is_empty());
        let mut json = serde_json::to_value(&step).unwrap();
        json.as_object_mut().unwrap().remove("input_files");
        json.as_object_mut().unwrap().remove("inputs_dir");
        let old: ClaimedStep = serde_json::from_value(json).unwrap();
        assert!(old.input_files.is_empty());
        assert!(old.inputs_dir.is_none());
    }

    #[tokio::test]
    async fn download_input_file_verifies_size_and_sha() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let job_id = Uuid::new_v4();
        let body = b"hello";
        let good = input_file("report.csv", body);
        // The claim promises 6 bytes; the server sends 5.
        let short = ClaimedInputFile {
            size_bytes: 6,
            ..input_file("report.csv", body)
        };
        // The claim promises 3 bytes; the server sends 5 (stopped while streaming).
        let long = ClaimedInputFile {
            size_bytes: 3,
            ..input_file("report.csv", body)
        };
        let wrong_digest = ClaimedInputFile {
            sha256: input_file("x", b"world").sha256,
            ..input_file("report.csv", body)
        };
        for f in [&good, &short, &long, &wrong_digest] {
            Mock::given(method("GET"))
                .and(path(input_file_url_path(job_id, &f.file_id)))
                .and(header("Authorization", "Bearer t"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
                .expect(1)
                .mount(&mock)
                .await;
        }
        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let tmp = tempfile::tempdir().unwrap();

        let dir = tmp.path().join(&good.file_id);
        std::fs::create_dir(&dir).unwrap();
        let dest = dir.join(&good.safe_name);
        client
            .download_input_file(job_id, &good, &dest)
            .await
            .expect("a verified download succeeds");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(dir_entries(&dir), vec![good.safe_name.clone()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o444, "a placed file is read-only");
        }

        for (f, what) in [
            (&short, "size"),
            (&long, "size"),
            (&wrong_digest, "SHA-256"),
        ] {
            let dir = tmp.path().join(&f.file_id);
            std::fs::create_dir(&dir).unwrap();
            let err = client
                .download_input_file(job_id, f, &dir.join(&f.safe_name))
                .await
                .expect_err("a mismatch fails the download");
            let msg = format!("{err:#}");
            assert!(msg.contains(&f.file_id), "error names the file id: {msg}");
            assert!(msg.contains(what), "error says what mismatched: {msg}");
            assert!(
                !msg.contains("report.csv"),
                "error never names the file: {msg}"
            );
            assert!(
                dir_entries(&dir).is_empty(),
                "no file (not even a temp file) is left after a mismatch"
            );
        }
    }

    #[tokio::test]
    async fn download_input_file_404_is_not_retried() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let job_id = Uuid::new_v4();
        let file = input_file("in.bin", b"abc");
        Mock::given(method("GET"))
            .and(path(input_file_url_path(job_id, &file.file_id)))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .expect(1)
            .mount(&mock)
            .await;
        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let tmp = tempfile::tempdir().unwrap();

        let err = download_input_file_with_retry(
            &client,
            job_id,
            &file,
            &tmp.path().join(&file.safe_name),
            3,
            Duration::from_millis(1),
        )
        .await
        .expect_err("404 fails at once");
        assert!(err.downcast_ref::<InputUnavailable>().is_none());
        let msg = format!("{err:#}");
        assert!(msg.contains(&file.file_id) && msg.contains("404"), "{msg}");
        assert!(dir_entries(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn download_input_file_retries_503_then_ok() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let job_id = Uuid::new_v4();
        let body = b"payload";
        let file = input_file("in.bin", body);
        Mock::given(method("GET"))
            .and(path(input_file_url_path(job_id, &file.file_id)))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "5"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path(input_file_url_path(job_id, &file.file_id)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .with_priority(2)
            .mount(&mock)
            .await;
        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(&file.safe_name);

        download_input_file_with_retry(&client, job_id, &file, &dest, 3, Duration::from_millis(1))
            .await
            .expect("a 503 is retried");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(mock.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn download_input_file_gives_up_after_attempts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let job_id = Uuid::new_v4();
        let file = input_file("in.bin", b"abc");
        Mock::given(method("GET"))
            .and(path(input_file_url_path(job_id, &file.file_id)))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "5"))
            .expect(2)
            .mount(&mock)
            .await;
        let client = ServerClient::new(&mock.uri(), "t", Some(5), Some(30));
        let tmp = tempfile::tempdir().unwrap();

        let err = download_input_file_with_retry(
            &client,
            job_id,
            &file,
            &tmp.path().join(&file.safe_name),
            2,
            Duration::from_millis(1),
        )
        .await
        .expect_err("still 503 after the last attempt");
        assert!(err.downcast_ref::<InputUnavailable>().is_some());
        assert!(format!("{err:#}").contains(&file.file_id));
        assert!(dir_entries(tmp.path()).is_empty());
    }

    #[test]
    fn input_503_retry_matches_pinned_tarballs() {
        assert_eq!(INPUT_503_ATTEMPTS, 12);
        assert_eq!(INPUT_503_DELAY, Duration::from_secs(5));
    }
}
