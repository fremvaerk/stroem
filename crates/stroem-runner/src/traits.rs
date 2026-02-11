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
