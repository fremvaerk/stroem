use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// HTTP client for communicating with the Strøm server
#[derive(Clone)]
pub struct ServerClient {
    client: reqwest::Client,
    base_url: Arc<str>,
    token: Arc<str>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimedStep {
    pub job_id: Uuid,
    pub step_name: String,
    pub action_name: String,
    pub action_type: String,
    pub action_image: Option<String>,
    pub action_spec: Option<serde_json::Value>,
    pub input: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct RegisterRequest {
    name: String,
    capabilities: Vec<String>,
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
}

#[derive(Debug, Serialize)]
struct StepCompleteRequest {
    exit_code: i32,
    output: Option<serde_json::Value>,
    error: Option<String>,
}

impl ServerClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: Arc::from(base_url),
            token: Arc::from(token),
        }
    }

    /// Register this worker with the server
    #[tracing::instrument(skip(self))]
    pub async fn register(&self, name: &str, capabilities: &[String]) -> Result<Uuid> {
        let url = format!("{}/worker/register", self.base_url);
        let req = RegisterRequest {
            name: name.to_string(),
            capabilities: capabilities.to_vec(),
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .context("Failed to send register request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read body".to_string());
            anyhow::bail!("Register failed with status {}: {}", status, body);
        }

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

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read body".to_string());
            anyhow::bail!("Heartbeat failed with status {}: {}", status, body);
        }

        Ok(())
    }

    /// Attempt to claim a step to execute
    #[tracing::instrument(skip(self))]
    pub async fn claim_step(
        &self,
        worker_id: Uuid,
        capabilities: &[String],
    ) -> Result<Option<ClaimedStep>> {
        let url = format!("{}/worker/jobs/claim", self.base_url);
        let req = ClaimRequest {
            worker_id,
            capabilities: capabilities.to_vec(),
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&req)
            .send()
            .await
            .context("Failed to send claim request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read body".to_string());
            anyhow::bail!("Claim failed with status {}: {}", status, body);
        }

        // Check if we got a 204 No Content (no work available)
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }

        let step: ClaimedStep = response
            .json()
            .await
            .context("Failed to parse claimed step response")?;
        Ok(Some(step))
    }

    /// Report that a step has started
    #[tracing::instrument(skip(self))]
    pub async fn report_step_start(&self, job_id: Uuid, step_name: &str) -> Result<()> {
        let url = format!(
            "{}/worker/jobs/{}/steps/{}/start",
            self.base_url, job_id, step_name
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .context("Failed to send step start request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read body".to_string());
            anyhow::bail!("Step start failed with status {}: {}", status, body);
        }

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

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read body".to_string());
            anyhow::bail!("Step complete failed with status {}: {}", status, body);
        }

        Ok(())
    }

    /// Push log lines to the server
    #[tracing::instrument(skip(self, lines))]
    pub async fn push_logs(&self, job_id: Uuid, lines: Vec<serde_json::Value>) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }

        let url = format!("{}/worker/jobs/{}/logs", self.base_url, job_id);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&lines)
            .send()
            .await
            .context("Failed to send logs request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read body".to_string());
            anyhow::bail!("Push logs failed with status {}: {}", status, body);
        }

        Ok(())
    }
}
