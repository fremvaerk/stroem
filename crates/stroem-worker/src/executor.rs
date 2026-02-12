use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use stroem_runner::{LogCallback, LogLine, RunConfig, RunResult, Runner, ShellRunner};

use crate::client::ClaimedStep;

/// Executes workflow steps using the appropriate runner
pub struct StepExecutor {
    runner: ShellRunner,
}

impl Default for StepExecutor {
    fn default() -> Self {
        Self {
            runner: ShellRunner::new(),
        }
    }
}

impl StepExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute a claimed step and return the result.
    /// `workspace_dir` is the path to the extracted workspace for this step's workspace.
    #[tracing::instrument(skip(self, log_buffer))]
    pub async fn execute_step(
        &self,
        step: &ClaimedStep,
        workspace_dir: &str,
        log_buffer: Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> Result<RunResult> {
        tracing::info!(
            "Executing step '{}' for job {}",
            step.step_name,
            step.job_id
        );

        // Build RunConfig from the action spec
        let mut config = self
            .build_run_config(step, workspace_dir)
            .context("Failed to build run config")?;

        // Resolve ref+ secret references in env vars via vals CLI
        crate::secrets::resolve_secrets(&mut config.env)
            .await
            .context("Failed to resolve secrets in env vars")?;

        // Create log callback that pushes to log buffer
        let log_callback: LogCallback = Box::new(move |line: LogLine| {
            if let Ok(mut buffer) = log_buffer.lock() {
                buffer.push(serde_json::json!({
                    "stream": line.stream,
                    "line": line.line,
                    "timestamp": line.timestamp,
                }));
            }
        });

        // Execute via runner
        let result = self
            .runner
            .execute(config, Some(log_callback))
            .await
            .context("Failed to execute step")?;

        tracing::info!(
            "Step '{}' completed with exit code {}",
            step.step_name,
            result.exit_code
        );

        Ok(result)
    }

    /// Build a RunConfig from a ClaimedStep
    fn build_run_config(&self, step: &ClaimedStep, workspace_dir: &str) -> Result<RunConfig> {
        let action_spec = step
            .action_spec
            .as_ref()
            .context("Missing action_spec in claimed step")?;

        // Extract cmd or script from action_spec
        let cmd = action_spec
            .get("cmd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let script = action_spec.get("script").and_then(|v| v.as_str()).map(|s| {
            let p = std::path::Path::new(s);
            if p.is_relative() {
                std::path::Path::new(workspace_dir)
                    .join(s)
                    .to_string_lossy()
                    .to_string()
            } else {
                s.to_string()
            }
        });

        if cmd.is_none() && script.is_none() {
            anyhow::bail!("Action spec must contain either 'cmd' or 'script'");
        }

        // Extract env from action_spec
        let mut env = HashMap::new();
        if let Some(env_obj) = action_spec.get("env") {
            if let Some(env_map) = env_obj.as_object() {
                for (key, value) in env_map {
                    if let Some(val_str) = value.as_str() {
                        env.insert(key.clone(), val_str.to_string());
                    }
                }
            }
        }

        Ok(RunConfig {
            cmd,
            script,
            env,
            workdir: workspace_dir.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_step(action_spec: Option<serde_json::Value>) -> ClaimedStep {
        ClaimedStep {
            job_id: Uuid::new_v4(),
            workspace: "default".to_string(),
            step_name: "test-step".to_string(),
            action_name: "test-action".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec,
            input: None,
        }
    }

    #[test]
    fn test_build_run_config_with_cmd() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo hello",
            "env": {
                "FOO": "bar",
                "BAZ": "qux"
            }
        })));

        let config = executor.build_run_config(&step, "/tmp/test").unwrap();
        assert_eq!(config.cmd, Some("echo hello".to_string()));
        assert_eq!(config.script, None);
        assert_eq!(config.env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(config.env.get("BAZ"), Some(&"qux".to_string()));
        assert_eq!(config.workdir, "/tmp/test");
    }

    #[test]
    fn test_build_run_config_with_script() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "script": "/path/to/script.sh"
        })));

        let config = executor.build_run_config(&step, "/tmp/test").unwrap();
        assert_eq!(config.cmd, None);
        // Absolute paths are kept as-is
        assert_eq!(config.script, Some("/path/to/script.sh".to_string()));
        assert_eq!(config.workdir, "/tmp/test");
    }

    #[test]
    fn test_build_run_config_with_relative_script() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "script": "actions/deploy.sh"
        })));

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.cmd, None);
        // Relative paths are resolved against workspace_dir
        assert_eq!(
            config.script,
            Some("/workspace/actions/deploy.sh".to_string())
        );
    }

    #[test]
    fn test_build_run_config_without_cmd_or_script() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "env": {
                "FOO": "bar"
            }
        })));

        let result = executor.build_run_config(&step, "/tmp/test");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must contain either 'cmd' or 'script'"));
    }

    #[test]
    fn test_build_run_config_missing_action_spec() {
        let executor = StepExecutor::new();

        let step = test_step(None);

        let result = executor.build_run_config(&step, "/tmp/test");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing action_spec"));
    }

    #[test]
    fn test_build_run_config_both_cmd_and_script() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo hello",
            "script": "run.sh"
        })));

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        // Both should be present — the runner decides which to use
        assert_eq!(config.cmd, Some("echo hello".to_string()));
        assert_eq!(config.script, Some("/workspace/run.sh".to_string()));
    }

    #[test]
    fn test_build_run_config_no_env_field() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo test"
        })));

        let config = executor.build_run_config(&step, "/tmp/test").unwrap();
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_build_run_config_empty_env() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo test",
            "env": {}
        })));

        let config = executor.build_run_config(&step, "/tmp/test").unwrap();
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_build_run_config_env_skips_non_string_values() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo test",
            "env": {
                "STR_VAL": "hello",
                "NUM_VAL": 42,
                "BOOL_VAL": true,
                "NULL_VAL": null,
                "ANOTHER_STR": "world"
            }
        })));

        let config = executor.build_run_config(&step, "/tmp/test").unwrap();
        // Only string values should be included
        assert_eq!(config.env.len(), 2);
        assert_eq!(config.env.get("STR_VAL"), Some(&"hello".to_string()));
        assert_eq!(config.env.get("ANOTHER_STR"), Some(&"world".to_string()));
    }

    #[test]
    fn test_build_run_config_empty_action_spec_object() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({})));

        let result = executor.build_run_config(&step, "/tmp/test");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must contain either 'cmd' or 'script'"));
    }

    #[test]
    fn test_build_run_config_workdir_set_correctly() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "cmd": "ls"
        })));

        // Different workspace dirs
        for dir in &["/workspace/default", "/tmp/stroem/ws1", "/var/data"] {
            let config = executor.build_run_config(&step, dir).unwrap();
            assert_eq!(config.workdir, *dir);
        }
    }

    #[test]
    fn test_build_run_config_relative_script_nested() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "script": "actions/deploy/run.sh"
        })));

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(
            config.script,
            Some("/workspace/actions/deploy/run.sh".to_string())
        );
    }

    #[tokio::test]
    async fn test_execute_step_simple_echo() {
        let executor = StepExecutor::new();
        let log_buffer = Arc::new(Mutex::new(Vec::new()));

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo hello-from-test"
        })));

        let result = executor
            .execute_step(&step, "/tmp", log_buffer.clone())
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);

        // Verify log buffer captured output
        let logs = log_buffer.lock().unwrap();
        let has_output = logs.iter().any(|l| {
            l.get("line")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("hello-from-test"))
                .unwrap_or(false)
        });
        assert!(has_output, "Log buffer should contain the echo output");
    }

    #[tokio::test]
    async fn test_execute_step_nonzero_exit() {
        let executor = StepExecutor::new();
        let log_buffer = Arc::new(Mutex::new(Vec::new()));

        let step = test_step(Some(serde_json::json!({
            "cmd": "exit 42"
        })));

        let result = executor
            .execute_step(&step, "/tmp", log_buffer)
            .await
            .unwrap();

        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn test_execute_step_env_vars_passed() {
        let executor = StepExecutor::new();
        let log_buffer = Arc::new(Mutex::new(Vec::new()));

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo $TEST_VAR_XYZ",
            "env": {
                "TEST_VAR_XYZ": "test-value-123"
            }
        })));

        let result = executor
            .execute_step(&step, "/tmp", log_buffer.clone())
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);

        let logs = log_buffer.lock().unwrap();
        let has_env_output = logs.iter().any(|l| {
            l.get("line")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("test-value-123"))
                .unwrap_or(false)
        });
        assert!(has_env_output, "Output should contain the env var value");
    }

    #[tokio::test]
    async fn test_execute_step_stderr_captured() {
        let executor = StepExecutor::new();
        let log_buffer = Arc::new(Mutex::new(Vec::new()));

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo error-output >&2"
        })));

        let result = executor
            .execute_step(&step, "/tmp", log_buffer.clone())
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);

        let logs = log_buffer.lock().unwrap();
        let has_stderr = logs.iter().any(|l| {
            l.get("stream")
                .and_then(|v| v.as_str())
                .map(|s| s == "stderr")
                .unwrap_or(false)
                && l.get("line")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("error-output"))
                    .unwrap_or(false)
        });
        assert!(
            has_stderr,
            "Should capture stderr output with correct stream tag"
        );
    }
}
