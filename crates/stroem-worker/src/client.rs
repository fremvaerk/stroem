use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

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
    pub action_spec: Option<serde_json::Value>,
    pub input: Option<serde_json::Value>,
    pub runner: Option<String>,
    pub timeout_secs: Option<i32>,
    pub revision: Option<String>,
    /// Name of the provider to use for agent steps (e.g. "anthropic").
    #[serde(default)]
    pub agent_provider_name: Option<String>,
    /// Rendered prompt for agent steps.
    #[serde(default)]
    pub agent_prompt: Option<String>,
    /// Rendered system prompt for agent steps.
    #[serde(default)]
    pub agent_system_prompt: Option<String>,
    /// MCP server definitions for agent steps with MCP tools.
    #[serde(default)]
    pub mcp_servers:
        Option<std::collections::HashMap<String, stroem_common::models::workflow::McpServerDef>>,
    /// Persisted conversation state for resuming suspended agent steps.
    #[serde(default)]
    pub agent_state: Option<serde_json::Value>,
    /// Task tool metadata keyed by task name.
    #[serde(default)]
    pub agent_tool_tasks: Option<serde_json::Value>,
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
    pub action_spec: Option<serde_json::Value>,
    pub input: Option<serde_json::Value>,
    pub runner: Option<String>,
    pub timeout_secs: Option<i32>,
    pub revision: Option<String>,
    pub agent_provider_name: Option<String>,
    pub agent_prompt: Option<String>,
    pub agent_system_prompt: Option<String>,
    pub mcp_servers:
        Option<std::collections::HashMap<String, stroem_common::models::workflow::McpServerDef>>,
    pub agent_state: Option<serde_json::Value>,
    pub agent_tool_tasks: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct RegisterRequest {
    name: String,
    tags: Vec<String>,
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
    tags: Vec<String>,
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
        tags: &[String],
        version: Option<&str>,
    ) -> Result<Uuid> {
        let url = format!("{}/worker/register", self.base_url);
        let req = RegisterRequest {
            name: name.to_string(),
            tags: tags.to_vec(),
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
        tags: &[String],
    ) -> Result<Option<ClaimedStep>> {
        let url = format!("{}/worker/jobs/claim", self.base_url);
        let req = ClaimRequest {
            worker_id,
            tags: tags.to_vec(),
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

    /// Push log lines to the server
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

        let url = format!("{}/worker/jobs/{}/logs", self.base_url, job_id);

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

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "lines": structured_lines, "step_name": step_name }))
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
}
