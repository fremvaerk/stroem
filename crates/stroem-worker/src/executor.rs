use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use stroem_runner::{LogCallback, LogLine, RunConfig, RunResult, Runner, RunnerMode, ShellRunner};

use crate::client::ClaimedStep;

/// Executes workflow steps using the appropriate runner
pub struct StepExecutor {
    shell_runner: ShellRunner,
    #[cfg(feature = "docker")]
    docker_runner: Option<stroem_runner::DockerRunner>,
    #[cfg(feature = "kubernetes")]
    kube_runner: Option<stroem_runner::KubeRunner>,
    /// Default runner image for Type 2 (shell-in-container) execution
    runner_image: Option<String>,
}

impl Default for StepExecutor {
    fn default() -> Self {
        Self {
            shell_runner: ShellRunner::new(),
            #[cfg(feature = "docker")]
            docker_runner: None,
            #[cfg(feature = "kubernetes")]
            kube_runner: None,
            runner_image: None,
        }
    }
}

impl StepExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the default runner image for Type 2 execution
    pub fn with_runner_image(mut self, image: String) -> Self {
        self.runner_image = Some(image);
        self
    }

    /// Set the Docker runner (requires `docker` feature)
    #[cfg(feature = "docker")]
    pub fn with_docker_runner(mut self, runner: stroem_runner::DockerRunner) -> Self {
        self.docker_runner = Some(runner);
        self
    }

    /// Set the Kubernetes runner (requires `kubernetes` feature)
    #[cfg(feature = "kubernetes")]
    pub fn with_kube_runner(mut self, runner: stroem_runner::KubeRunner) -> Self {
        self.kube_runner = Some(runner);
        self
    }

    /// Select the appropriate runner for a step based on (action_type, runner) dispatch
    fn select_runner(&self, step: &ClaimedStep) -> Result<&dyn Runner> {
        let runner_field = step.runner.as_deref().unwrap_or("local");
        match (step.action_type.as_str(), runner_field) {
            ("shell", "local") => Ok(&self.shell_runner),
            ("shell", "docker") | ("docker", _) => {
                #[cfg(feature = "docker")]
                {
                    self.docker_runner
                        .as_ref()
                        .map(|r| r as &dyn Runner)
                        .context("Docker runner not configured. Enable the 'docker' feature and configure docker settings in worker config.")
                }
                #[cfg(not(feature = "docker"))]
                {
                    anyhow::bail!("Docker runner not available. Build the worker with the 'docker' feature enabled.")
                }
            }
            ("shell", "pod") | ("pod", _) => {
                #[cfg(feature = "kubernetes")]
                {
                    self.kube_runner
                        .as_ref()
                        .map(|r| r as &dyn Runner)
                        .context("Kubernetes runner not configured. Enable the 'kubernetes' feature and configure kubernetes settings in worker config.")
                }
                #[cfg(not(feature = "kubernetes"))]
                {
                    anyhow::bail!("Kubernetes runner not available. Build the worker with the 'kubernetes' feature enabled.")
                }
            }
            (t, r) => anyhow::bail!("Unknown action type / runner combination: {t}/{r}"),
        }
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

        // Select and execute via appropriate runner
        let runner = self
            .select_runner(step)
            .context("Failed to select runner")?;
        let result = runner
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

        let runner_field = step.runner.as_deref().unwrap_or("local");
        let is_type2 = step.action_type == "shell"; // Type 2: shell with workspace
        let runner_mode = if is_type2 {
            RunnerMode::WithWorkspace
        } else {
            RunnerMode::NoWorkspace
        };

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

        // Extract entrypoint and command from action_spec
        let entrypoint = action_spec.get("entrypoint").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
        });

        let command = action_spec.get("command").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
        });

        // Type 2 (shell) requires cmd or script; Type 1 (docker/pod) allows empty (image defaults)
        if is_type2 && cmd.is_none() && script.is_none() {
            anyhow::bail!("Action spec must contain either 'cmd' or 'script'");
        }

        // Determine image: Type 2 uses runner_image, Type 1 uses action image
        let image = if is_type2 && runner_field != "local" {
            self.runner_image
                .clone()
                .or_else(|| step.action_image.clone())
        } else {
            step.action_image.clone()
        };

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

        // Inject Strøm metadata env vars (used by KubeRunner for pod naming/workspace)
        env.insert("STROEM_JOB_ID".to_string(), step.job_id.to_string());
        env.insert("STROEM_STEP_NAME".to_string(), step.step_name.clone());
        env.insert("STROEM_WORKSPACE".to_string(), step.workspace.clone());

        // Extract pod manifest overrides (raw JSON for deep-merge into pod spec)
        let pod_manifest_overrides = action_spec.get("manifest").cloned();

        Ok(RunConfig {
            cmd,
            script,
            env,
            workdir: workspace_dir.to_string(),
            action_type: step.action_type.clone(),
            image,
            runner_mode,
            runner_image: self.runner_image.clone(),
            entrypoint,
            command,
            pod_manifest_overrides,
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
            runner: None,
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
        // Only the 3 STROEM_* metadata env vars should be present
        assert_eq!(config.env.len(), 3);
        assert!(config.env.contains_key("STROEM_JOB_ID"));
        assert!(config.env.contains_key("STROEM_STEP_NAME"));
        assert!(config.env.contains_key("STROEM_WORKSPACE"));
    }

    #[test]
    fn test_build_run_config_empty_env() {
        let executor = StepExecutor::new();

        let step = test_step(Some(serde_json::json!({
            "cmd": "echo test",
            "env": {}
        })));

        let config = executor.build_run_config(&step, "/tmp/test").unwrap();
        // Only the 3 STROEM_* metadata env vars
        assert_eq!(config.env.len(), 3);
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
        // 2 string values + 3 STROEM_* metadata env vars
        assert_eq!(config.env.len(), 5);
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

    #[test]
    fn test_dispatch_shell_no_image() {
        let executor = StepExecutor::new();
        let step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        // Shell without image should resolve to shell runner
        let runner = executor.select_runner(&step);
        assert!(runner.is_ok());
    }

    #[test]
    fn test_dispatch_shell_with_docker_runner() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.runner = Some("docker".to_string());
        // Without docker feature/config, should fail gracefully
        let runner = executor.select_runner(&step);
        assert!(runner.is_err());
    }

    #[test]
    fn test_dispatch_docker_type() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.action_type = "docker".to_string();
        step.action_image = Some("alpine:latest".to_string());
        let runner = executor.select_runner(&step);
        assert!(runner.is_err());
    }

    #[test]
    fn test_dispatch_pod_type() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.action_type = "pod".to_string();
        step.action_image = Some("alpine:latest".to_string());
        let runner = executor.select_runner(&step);
        assert!(runner.is_err());
    }

    #[test]
    fn test_dispatch_unknown_type() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.action_type = "unknown".to_string();
        match executor.select_runner(&step) {
            Err(e) => assert!(
                e.to_string().contains("Unknown action type"),
                "Expected 'Unknown action type' error, got: {e}"
            ),
            Ok(_) => panic!("Expected error for unknown action type"),
        }
    }

    #[test]
    fn test_build_run_config_populates_action_type_and_image() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.action_type = "docker".to_string();
        step.action_image = Some("python:3.12".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.action_type, "docker");
        assert_eq!(config.image, Some("python:3.12".to_string()));
    }

    #[test]
    fn test_build_run_config_type1_no_cmd_allowed() {
        // Type 1 (docker/pod) doesn't require cmd/script — image defaults run
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({})));
        step.action_type = "docker".to_string();
        step.action_image = Some("company/migrations:v3".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.runner_mode, RunnerMode::NoWorkspace);
        assert!(config.cmd.is_none());
        assert!(config.script.is_none());
        assert_eq!(config.image, Some("company/migrations:v3".to_string()));
    }

    #[test]
    fn test_build_run_config_type2_with_runner_image() {
        let executor =
            StepExecutor::new().with_runner_image("ghcr.io/org/runner:latest".to_string());

        let mut step = test_step(Some(serde_json::json!({"cmd": "npm test"})));
        step.runner = Some("docker".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.runner_mode, RunnerMode::WithWorkspace);
        // Type 2 uses runner_image, not action_image
        assert_eq!(config.image, Some("ghcr.io/org/runner:latest".to_string()));
    }

    #[test]
    fn test_build_run_config_type1_entrypoint() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({
            "entrypoint": ["/app/run"],
            "command": ["--env", "prod"]
        })));
        step.action_type = "docker".to_string();
        step.action_image = Some("company/deploy:v3".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.entrypoint, Some(vec!["/app/run".to_string()]));
        assert_eq!(
            config.command,
            Some(vec!["--env".to_string(), "prod".to_string()])
        );
    }

    #[test]
    fn test_build_run_config_type2_fallback_to_action_image() {
        // When runner_image is not set, Type 2 should fall back to action_image
        let executor = StepExecutor::new(); // no runner_image
        let mut step = test_step(Some(serde_json::json!({"cmd": "npm test"})));
        step.runner = Some("docker".to_string());
        step.action_image = Some("node:20-alpine".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.runner_mode, RunnerMode::WithWorkspace);
        // Falls back to action_image when runner_image is not configured
        assert_eq!(config.image, Some("node:20-alpine".to_string()));
    }

    #[test]
    fn test_build_run_config_type2_runner_image_overrides_action_image() {
        // When runner_image IS set, it takes precedence over action_image for Type 2
        let executor =
            StepExecutor::new().with_runner_image("ghcr.io/org/runner:latest".to_string());
        let mut step = test_step(Some(serde_json::json!({"cmd": "npm test"})));
        step.runner = Some("docker".to_string());
        step.action_image = Some("node:20-alpine".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.image, Some("ghcr.io/org/runner:latest".to_string()));
    }

    #[test]
    fn test_build_run_config_type1_ignores_runner_image() {
        // Type 1 (docker) should use action_image, not runner_image
        let executor =
            StepExecutor::new().with_runner_image("ghcr.io/org/runner:latest".to_string());
        let mut step = test_step(Some(serde_json::json!({})));
        step.action_type = "docker".to_string();
        step.action_image = Some("company/app:v3".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.runner_mode, RunnerMode::NoWorkspace);
        assert_eq!(config.image, Some("company/app:v3".to_string()));
    }

    #[test]
    fn test_dispatch_shell_pod_runner() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.runner = Some("pod".to_string());
        // Without kubernetes feature/config, should fail gracefully
        let runner = executor.select_runner(&step);
        assert!(runner.is_err());
    }

    #[test]
    fn test_build_run_config_type2_local_no_image() {
        // Type 2 shell+local should not set image even if action_image is present
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.runner = None; // defaults to "local"
        step.action_image = Some("node:20".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.runner_mode, RunnerMode::WithWorkspace);
        // For shell+local, image comes from action_image directly (not runner_image logic)
        assert_eq!(config.image, Some("node:20".to_string()));
    }

    #[test]
    fn test_build_run_config_stroem_env_vars() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));
        step.workspace = "my-workspace".to_string();
        step.step_name = "deploy-step".to_string();

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(
            config.env.get("STROEM_STEP_NAME"),
            Some(&"deploy-step".to_string())
        );
        assert_eq!(
            config.env.get("STROEM_WORKSPACE"),
            Some(&"my-workspace".to_string())
        );
        assert!(config.env.contains_key("STROEM_JOB_ID"));
    }

    #[test]
    fn test_build_run_config_shell_local_mode() {
        let executor = StepExecutor::new();
        let step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert_eq!(config.runner_mode, RunnerMode::WithWorkspace);
        assert_eq!(config.action_type, "shell");
    }

    #[test]
    fn test_build_run_config_with_manifest_overrides() {
        let executor = StepExecutor::new();
        let mut step = test_step(Some(serde_json::json!({
            "cmd": "echo hi",
            "manifest": {
                "spec": {
                    "serviceAccountName": "my-sa",
                    "nodeSelector": {"gpu": "true"}
                }
            }
        })));
        step.action_type = "pod".to_string();
        step.action_image = Some("alpine:latest".to_string());

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert!(config.pod_manifest_overrides.is_some());
        let overrides = config.pod_manifest_overrides.unwrap();
        assert_eq!(
            overrides["spec"]["serviceAccountName"],
            serde_json::json!("my-sa")
        );
        assert_eq!(
            overrides["spec"]["nodeSelector"]["gpu"],
            serde_json::json!("true")
        );
    }

    #[test]
    fn test_build_run_config_without_manifest() {
        let executor = StepExecutor::new();
        let step = test_step(Some(serde_json::json!({"cmd": "echo hi"})));

        let config = executor.build_run_config(&step, "/workspace").unwrap();
        assert!(config.pod_manifest_overrides.is_none());
    }
}
