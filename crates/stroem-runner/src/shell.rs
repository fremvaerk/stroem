use crate::traits::{LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner};
use anyhow::Result;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Shell runner that executes commands using the system shell
pub struct ShellRunner;

impl ShellRunner {
    pub fn new() -> Self {
        ShellRunner
    }
}

impl Default for ShellRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Runner for ShellRunner {
    async fn execute(
        &self,
        config: RunConfig,
        log_callback: Option<LogCallback>,
    ) -> Result<RunResult> {
        // Build command
        let mut cmd = if let Some(ref command) = config.cmd {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command);
            c
        } else if let Some(ref script) = config.script {
            tokio::process::Command::new(script)
        } else {
            anyhow::bail!("RunConfig must have either cmd or script");
        };

        // Set env and workdir
        cmd.envs(&config.env);
        cmd.current_dir(&config.workdir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;

        // Take stdout and stderr handles
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stderr"))?;

        // Create readers
        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        // Read both streams concurrently
        let stdout_handle = tokio::spawn(async move {
            let mut lines = stdout_reader.lines();
            let mut collected = Vec::new();
            let mut output = None;

            while let Some(line) = lines.next_line().await.transpose() {
                match line {
                    Ok(line) => {
                        collected.push(line.clone());

                        // Check for OUTPUT: prefix
                        if let Some(json_str) = line.strip_prefix("OUTPUT: ") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str)
                            {
                                output = Some(parsed);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Error reading stdout line: {}", e);
                        break;
                    }
                }
            }

            (collected, output)
        });

        let stderr_handle = tokio::spawn(async move {
            let mut lines = stderr_reader.lines();
            let mut collected = Vec::new();

            while let Some(line) = lines.next_line().await.transpose() {
                match line {
                    Ok(line) => {
                        collected.push(line);
                    }
                    Err(e) => {
                        tracing::warn!("Error reading stderr line: {}", e);
                        break;
                    }
                }
            }

            collected
        });

        // Wait for both streams to complete
        let (stdout_result, stderr_result) = tokio::join!(stdout_handle, stderr_handle);

        // Unpack results
        let (stdout_lines, parsed_output) = stdout_result?;
        let stderr_lines = stderr_result?;

        // Call log_callback for all collected lines if provided
        if let Some(ref callback) = log_callback {
            for line in &stdout_lines {
                callback(LogLine {
                    stream: LogStream::Stdout,
                    line: line.clone(),
                    timestamp: chrono::Utc::now(),
                });
            }
            for line in &stderr_lines {
                callback(LogLine {
                    stream: LogStream::Stderr,
                    line: line.clone(),
                    timestamp: chrono::Utc::now(),
                });
            }
        }

        // Wait for the process to complete
        let status = child.wait().await?;

        Ok(RunResult {
            exit_code: status.code().unwrap_or(-1),
            stdout: stdout_lines.join("\n"),
            stderr: stderr_lines.join("\n"),
            output: parsed_output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn shell_config(
        cmd: Option<&str>,
        script: Option<&str>,
        env: HashMap<String, String>,
    ) -> RunConfig {
        RunConfig {
            cmd: cmd.map(|s| s.to_string()),
            script: script.map(|s| s.to_string()),
            env,
            workdir: "/tmp".to_string(),
            action_type: "shell".to_string(),
            image: None,
            runner_mode: crate::RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
        }
    }

    #[tokio::test]
    async fn test_simple_echo() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("echo hello world"), None, HashMap::new());
        let result = runner.execute(config, None).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello world"));
        assert!(result.output.is_none());
    }

    #[tokio::test]
    async fn test_exit_code() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("exit 42"), None, HashMap::new());
        let result = runner.execute(config, None).await.unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn test_env_vars() {
        let runner = ShellRunner::new();
        let mut env = HashMap::new();
        env.insert("MY_VAR".to_string(), "my_value".to_string());
        let config = shell_config(Some("echo $MY_VAR"), None, env);
        let result = runner.execute(config, None).await.unwrap();
        assert!(result.stdout.contains("my_value"));
    }

    #[tokio::test]
    async fn test_output_parsing() {
        let runner = ShellRunner::new();
        let config = shell_config(
            Some(r#"echo "some log line" && echo 'OUTPUT: {"greeting": "hello"}'"#),
            None,
            HashMap::new(),
        );
        let result = runner.execute(config, None).await.unwrap();
        assert_eq!(result.exit_code, 0);
        let output = result.output.unwrap();
        assert_eq!(output["greeting"], "hello");
    }

    #[tokio::test]
    async fn test_stderr_capture() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("echo error >&2"), None, HashMap::new());
        let result = runner.execute(config, None).await.unwrap();
        assert!(result.stderr.contains("error"));
    }

    #[tokio::test]
    async fn test_log_callback() {
        let runner = ShellRunner::new();
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let lines_clone = lines.clone();
        let callback: LogCallback = Box::new(move |line| {
            lines_clone.lock().unwrap().push(line);
        });
        let config = shell_config(
            Some("echo line1 && echo line2 && echo error >&2"),
            None,
            HashMap::new(),
        );
        let result = runner.execute(config, Some(callback)).await.unwrap();
        assert_eq!(result.exit_code, 0);
        let captured = lines.lock().unwrap();
        assert!(captured.len() >= 2);
    }

    #[tokio::test]
    async fn test_working_directory() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("pwd"), None, HashMap::new());
        let result = runner.execute(config, None).await.unwrap();
        assert!(result.stdout.trim().contains("tmp"));
    }

    #[tokio::test]
    async fn test_multiline_output() {
        let runner = ShellRunner::new();
        let config = shell_config(
            Some("echo line1 && echo line2 && echo line3"),
            None,
            HashMap::new(),
        );
        let result = runner.execute(config, None).await.unwrap();
        assert!(result.stdout.contains("line1"));
        assert!(result.stdout.contains("line2"));
        assert!(result.stdout.contains("line3"));
    }

    #[tokio::test]
    async fn test_no_cmd_or_script_errors() {
        let runner = ShellRunner::new();
        let config = shell_config(None, None, HashMap::new());
        let result = runner.execute(config, None).await;
        assert!(result.is_err());
    }
}
