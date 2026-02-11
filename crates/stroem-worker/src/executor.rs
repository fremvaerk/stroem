use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use stroem_runner::{LogCallback, LogLine, RunConfig, RunResult, Runner, ShellRunner};

use crate::client::ClaimedStep;

/// Executes workflow steps using the appropriate runner
pub struct StepExecutor {
    runner: ShellRunner,
    workspace_dir: String,
}

impl StepExecutor {
    pub fn new(workspace_dir: &str) -> Self {
        Self {
            runner: ShellRunner::new(),
            workspace_dir: workspace_dir.to_string(),
        }
    }

    /// Execute a claimed step and return the result
    #[tracing::instrument(skip(self, log_buffer))]
    pub async fn execute_step(
        &self,
        step: &ClaimedStep,
        log_buffer: Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> Result<RunResult> {
        tracing::info!(
            "Executing step '{}' for job {}",
            step.step_name,
            step.job_id
        );

        // Build RunConfig from the action spec
        let mut config = self
            .build_run_config(step)
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
    fn build_run_config(&self, step: &ClaimedStep) -> Result<RunConfig> {
        let action_spec = step
            .action_spec
            .as_ref()
            .context("Missing action_spec in claimed step")?;

        // Extract cmd or script from action_spec
        let cmd = action_spec
            .get("cmd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let script = action_spec
            .get("script")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

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
            workdir: self.workspace_dir.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_build_run_config_with_cmd() {
        let executor = StepExecutor::new("/tmp/test");

        let step = ClaimedStep {
            job_id: Uuid::new_v4(),
            step_name: "test-step".to_string(),
            action_name: "test-action".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({
                "cmd": "echo hello",
                "env": {
                    "FOO": "bar",
                    "BAZ": "qux"
                }
            })),
            input: None,
        };

        let config = executor.build_run_config(&step).unwrap();
        assert_eq!(config.cmd, Some("echo hello".to_string()));
        assert_eq!(config.script, None);
        assert_eq!(config.env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(config.env.get("BAZ"), Some(&"qux".to_string()));
        assert_eq!(config.workdir, "/tmp/test");
    }

    #[test]
    fn test_build_run_config_with_script() {
        let executor = StepExecutor::new("/tmp/test");

        let step = ClaimedStep {
            job_id: Uuid::new_v4(),
            step_name: "test-step".to_string(),
            action_name: "test-action".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({
                "script": "/path/to/script.sh"
            })),
            input: None,
        };

        let config = executor.build_run_config(&step).unwrap();
        assert_eq!(config.cmd, None);
        assert_eq!(config.script, Some("/path/to/script.sh".to_string()));
        assert_eq!(config.workdir, "/tmp/test");
    }

    #[test]
    fn test_build_run_config_without_cmd_or_script() {
        let executor = StepExecutor::new("/tmp/test");

        let step = ClaimedStep {
            job_id: Uuid::new_v4(),
            step_name: "test-step".to_string(),
            action_name: "test-action".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({
                "env": {
                    "FOO": "bar"
                }
            })),
            input: None,
        };

        let result = executor.build_run_config(&step);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must contain either 'cmd' or 'script'"));
    }

    #[test]
    fn test_build_run_config_missing_action_spec() {
        let executor = StepExecutor::new("/tmp/test");

        let step = ClaimedStep {
            job_id: Uuid::new_v4(),
            step_name: "test-step".to_string(),
            action_name: "test-action".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
        };

        let result = executor.build_run_config(&step);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing action_spec"));
    }
}
