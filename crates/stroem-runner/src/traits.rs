use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Result of running a step
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Parsed output from stdout lines matching "OUTPUT: {json}"
    pub output: Option<serde_json::Value>,
}

impl RunResult {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Whether a runner should mount workspace files or run the image standalone
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerMode {
    /// Type 2: Shell in a runner environment — bind-mount workspace, use runner_image
    WithWorkspace,
    /// Type 1: Run user's prepared image as-is — no workspace mount
    NoWorkspace,
}

/// Configuration for a step execution
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The command to execute (for shell type)
    pub cmd: Option<String>,
    /// The script path (for shell type, alternative to cmd)
    pub script: Option<String>,
    /// Environment variables
    pub env: HashMap<String, String>,
    /// Working directory
    pub workdir: String,
    /// Action type: "shell", "docker", or "pod"
    pub action_type: String,
    /// Container image (e.g. "python:3.12") — used by docker and pod runners
    pub image: Option<String>,
    /// Runner mode: WithWorkspace (Type 2) or NoWorkspace (Type 1)
    pub runner_mode: RunnerMode,
    /// Default runner image for Type 2 shell-in-container execution
    pub runner_image: Option<String>,
    /// Entrypoint override for Type 1 docker/pod
    pub entrypoint: Option<Vec<String>>,
    /// Command args for Type 1 docker/pod
    pub command: Option<Vec<String>>,
}

/// A callback for receiving log lines as they're produced
pub type LogCallback = Box<dyn Fn(LogLine) + Send + Sync>;

/// A single log line from the runner
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub stream: LogStream,
    pub line: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[async_trait]
pub trait Runner: Send + Sync {
    /// Execute a command and return the result.
    /// The log_callback receives log lines in real-time as they're produced.
    async fn execute(
        &self,
        config: RunConfig,
        log_callback: Option<LogCallback>,
    ) -> Result<RunResult>;
}
