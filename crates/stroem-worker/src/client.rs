use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use stroem_common::secret::Secret;
use uuid::Uuid;

/// `/worker/jobs/{id}/logs` keeps axum's default 2 MiB request body limit; keep every push
/// comfortably under it so a fast step's buffered lines are never rejected with 413 and lost.
const LOG_PUSH_MAX_BYTES: usize = 1024 * 1024;

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
}

#[derive(Debug, Serialize)]
struct StepCompleteRequest {
    exit_code: i32,
    output: Option<serde_json::Value>,
    error: Option<String>,
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
        let req = ClaimRequest {
            worker_id,
            capabilities: capabilities.to_vec(),
            tags: tags.to_vec(),
            exclusive,
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

    /// Download the latest state tarball for a task.
    /// Returns `None` if no state exists (server returns 204 No Content).
    #[tracing::instrument(skip(self))]
    pub async fn download_state_tarball(
        &self,
        workspace: &str,
        task_name: &str,
    ) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/worker/state/{}/{}", self.base_url, workspace, task_name);
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

    /// Download the latest global workspace state tarball.
    /// Returns `None` if no global state exists (server returns 204 No Content).
    #[tracing::instrument(skip(self))]
    pub async fn download_global_state_tarball(&self, workspace: &str) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/worker/global-state/{}", self.base_url, workspace);
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

        // Envelope overhead: `{"lines":[...],"step_name":"..."}` plus the step name's bytes.
        // Slightly generous is fine — it only makes chunks a little smaller, never over cap.
        let envelope_overhead = step_name.len() + 32;

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
}
