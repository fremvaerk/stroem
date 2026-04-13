use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

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
    /// Script action: runs in a runner environment — bind-mount workspace, use runner_image
    WithWorkspace,
    /// Container action (docker/pod): runs user's prepared image as-is — no workspace mount
    NoWorkspace,
}

/// Configuration for a step execution
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Inline code to execute (from `script` YAML field for type: script, or `cmd` for docker/pod)
    pub cmd: Option<String>,
    /// Path to a script file (from `source` YAML field)
    pub script: Option<String>,
    /// Environment variables
    pub env: HashMap<String, String>,
    /// Working directory
    pub workdir: String,
    /// Action type: "script", "docker", or "pod"
    pub action_type: String,
    /// Container image (e.g. "python:3.12") — used by docker and pod runners
    pub image: Option<String>,
    /// Runner mode: WithWorkspace (script actions) or NoWorkspace (container actions)
    pub runner_mode: RunnerMode,
    /// Default runner image for script-in-container execution
    pub runner_image: Option<String>,
    /// Entrypoint override for docker/pod container actions
    pub entrypoint: Option<Vec<String>>,
    /// Command args for docker/pod container actions
    pub command: Option<Vec<String>>,
    /// Raw pod manifest overrides (deep-merged into generated pod JSON)
    pub pod_manifest_overrides: Option<serde_json::Value>,
    /// Script language (for type: script). Defaults to "shell" when absent.
    pub language: Option<String>,
    /// Dependencies to install before running the script.
    pub dependencies: Vec<String>,
    /// Override auto-detected interpreter binary.
    pub interpreter: Option<String>,
    /// CLI arguments to pass to the script (already Tera-rendered).
    pub args: Vec<String>,
    /// Path to previous state directory (mounted read-only at /state).
    pub state_dir: Option<String>,
    /// Path to new state output directory (mounted read-write at /state-out).
    pub state_out_dir: Option<String>,
    /// Path to global workspace state directory (mounted read-only at /global-state).
    pub global_state_dir: Option<String>,
    /// Path to global workspace state output directory (mounted read-write at /global-state-out).
    pub global_state_out_dir: Option<String>,
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
    /// The cancel_token can be used to signal cancellation — runners should
    /// kill the running process and return early when cancelled.
    async fn execute(
        &self,
        config: RunConfig,
        log_callback: Option<LogCallback>,
        cancel_token: CancellationToken,
    ) -> Result<RunResult>;
}

/// Parse an "OUTPUT:{json}" or "OUTPUT: {json}" line into a JSON value.
pub fn parse_output_line(line: &str) -> Option<serde_json::Value> {
    let json_str = line
        .strip_prefix("OUTPUT: ")
        .or_else(|| line.strip_prefix("OUTPUT:"))?;
    serde_json::from_str(json_str).ok()
}

/// Parse a "STATE:{json}" or "STATE: {json}" line into a JSON value.
pub fn parse_state_line(line: &str) -> Option<serde_json::Value> {
    let json_str = line
        .strip_prefix("STATE: ")
        .or_else(|| line.strip_prefix("STATE:"))?;
    serde_json::from_str(json_str).ok()
}

/// Parse a "GLOBAL_STATE:{json}" or "GLOBAL_STATE: {json}" line into a JSON value.
pub fn parse_global_state_line(line: &str) -> Option<serde_json::Value> {
    let json_str = line
        .strip_prefix("GLOBAL_STATE: ")
        .or_else(|| line.strip_prefix("GLOBAL_STATE:"))?;
    serde_json::from_str(json_str).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_output_line_with_space() {
        let result = parse_output_line(r#"OUTPUT: {"key": "value"}"#);
        assert_eq!(result, Some(json!({"key": "value"})));
    }

    #[test]
    fn test_parse_output_line_without_space() {
        let result = parse_output_line(r#"OUTPUT:{"data": [{"1": 1}]}"#);
        assert_eq!(result, Some(json!({"data": [{"1": 1}]})));
    }

    #[test]
    fn test_parse_output_line_no_match() {
        assert_eq!(parse_output_line("some random log line"), None);
    }

    #[test]
    fn test_parse_output_line_invalid_json() {
        assert_eq!(parse_output_line("OUTPUT: not json"), None);
        assert_eq!(parse_output_line("OUTPUT:not json"), None);
    }

    #[test]
    fn test_parse_state_line_with_space() {
        let result = parse_state_line(r#"STATE: {"cursor": "abc123"}"#);
        assert_eq!(result, Some(json!({"cursor": "abc123"})));
    }

    #[test]
    fn test_parse_state_line_without_space() {
        let result = parse_state_line(r#"STATE:{"count": 42}"#);
        assert_eq!(result, Some(json!({"count": 42})));
    }

    #[test]
    fn test_parse_state_line_no_match() {
        assert_eq!(parse_state_line("some random log line"), None);
        assert_eq!(parse_state_line("OUTPUT: {\"key\": 1}"), None);
    }

    #[test]
    fn test_parse_state_line_invalid_json() {
        assert_eq!(parse_state_line("STATE: not json"), None);
    }

    #[test]
    fn test_parse_global_state_line_with_space() {
        let result = parse_global_state_line(r#"GLOBAL_STATE: {"cursor": "abc123"}"#);
        assert_eq!(result, Some(json!({"cursor": "abc123"})));
    }

    #[test]
    fn test_parse_global_state_line_without_space() {
        let result = parse_global_state_line(r#"GLOBAL_STATE:{"count": 42}"#);
        assert_eq!(result, Some(json!({"count": 42})));
    }

    #[test]
    fn test_parse_global_state_line_no_match() {
        assert_eq!(parse_global_state_line("some random log line"), None);
        assert_eq!(parse_global_state_line("OUTPUT: {\"key\": 1}"), None);
        assert_eq!(parse_global_state_line("STATE: {\"key\": 1}"), None);
    }

    #[test]
    fn test_parse_global_state_line_invalid_json() {
        assert_eq!(parse_global_state_line("GLOBAL_STATE: not json"), None);
        assert_eq!(parse_global_state_line("GLOBAL_STATE:not json"), None);
    }
}
