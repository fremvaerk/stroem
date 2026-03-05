use crate::script_exec;
use crate::traits::{
    parse_output_line, LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner,
};
use anyhow::{bail, Result};
use async_trait::async_trait;
use stroem_common::language::ScriptLanguage;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

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
        cancel_token: CancellationToken,
    ) -> Result<RunResult> {
        let lang = ScriptLanguage::from_str_opt(config.language.as_deref());

        // Track temp script file for cleanup
        let mut temp_script_path: Option<std::path::PathBuf> = None;

        // Build command
        let mut cmd = if lang.is_shell() {
            // Shell language — use the original sh -c path
            if let Some(ref command) = config.cmd {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command);
                c
            } else if let Some(ref script) = config.script {
                tokio::process::Command::new(script)
            } else {
                anyhow::bail!("RunConfig must have either cmd or script");
            }
        } else if let Some(ref inline_cmd) = config.cmd {
            // Non-shell language with inline cmd — write to temp file, execute with interpreter
            let workdir = std::path::Path::new(&config.workdir);
            let script_path = script_exec::write_temp_script(workdir, inline_cmd, lang)?;
            // Register for cleanup immediately so any subsequent failure cleans up the file.
            temp_script_path = Some(script_path.clone());

            let (binary, args) = script_exec::build_script_command(
                lang,
                &script_path,
                &config.dependencies,
                config.interpreter.as_deref(),
            )?;

            // Handle non-uv deps that need a prefix install command.
            // Reuse `binary` from build_script_command to avoid a second PATH probe.
            if !config.dependencies.is_empty() {
                if let Some(install_cmd) =
                    script_exec::build_dep_install_prefix(lang, &binary, &config.dependencies)
                {
                    // Run dep install as a separate process first
                    let install_status = tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(&install_cmd)
                        .current_dir(&config.workdir)
                        .envs(&config.env)
                        .status()
                        .await;

                    match install_status {
                        Ok(status) if !status.success() => {
                            if let Some(ref path) = temp_script_path {
                                script_exec::cleanup_temp_script(path);
                            }
                            bail!(
                                "Dependency installation failed (exit code {}): {}",
                                status.code().unwrap_or(-1),
                                install_cmd
                            );
                        }
                        Err(e) => {
                            if let Some(ref path) = temp_script_path {
                                script_exec::cleanup_temp_script(path);
                            }
                            bail!(
                                "Failed to run dependency installation command '{}': {}",
                                install_cmd,
                                e
                            );
                        }
                        _ => {}
                    }
                }
            }

            let mut c = tokio::process::Command::new(&binary);
            for arg in &args {
                c.arg(arg);
            }
            c
        } else if let Some(ref script) = config.script {
            // Non-shell language with script file path — execute with interpreter
            let script_path = std::path::Path::new(script);
            let (binary, args) = script_exec::build_script_command(
                lang,
                script_path,
                &config.dependencies,
                config.interpreter.as_deref(),
            )?;

            let mut c = tokio::process::Command::new(&binary);
            for arg in &args {
                c.arg(arg);
            }
            c
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
                        if let Some(parsed) = parse_output_line(&line) {
                            output = Some(parsed);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Error reading stdout line: {:#}", e);
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
                        tracing::warn!("Error reading stderr line: {:#}", e);
                        break;
                    }
                }
            }

            collected
        });

        // Race stream reading against cancellation. Stream handles are kept
        // separate from `child` so we can kill the process on cancel and then
        // still collect partial output from the readers (they EOF when the
        // child's pipes close).
        let streams_future = async {
            let (stdout_result, stderr_result) = tokio::join!(stdout_handle, stderr_handle);
            let (stdout_lines, parsed_output) = stdout_result?;
            let stderr_lines = stderr_result?;
            Ok::<_, anyhow::Error>((stdout_lines, stderr_lines, parsed_output))
        };
        tokio::pin!(streams_future);

        enum Outcome {
            Cancelled,
            Streams(anyhow::Result<(Vec<String>, Vec<String>, Option<serde_json::Value>)>),
        }

        let outcome = tokio::select! {
            _ = cancel_token.cancelled() => Outcome::Cancelled,
            result = &mut streams_future => Outcome::Streams(result),
        };

        let result = match outcome {
            Outcome::Cancelled => {
                // Kill the child process — this causes the stream readers to EOF
                if let Err(e) = child.kill().await {
                    tracing::warn!("Failed to kill child process on cancel: {:#}", e);
                }
                // Wait briefly for stream tasks to finish collecting partial output
                let result =
                    tokio::time::timeout(std::time::Duration::from_millis(500), streams_future)
                        .await;

                let (stdout_lines, stderr_lines, parsed_output) = match result {
                    Ok(Ok((out, err, parsed))) => (out, err, parsed),
                    _ => (Vec::new(), Vec::new(), None),
                };

                // Deliver partial logs via callback
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

                Ok(RunResult {
                    exit_code: -1,
                    stdout: stdout_lines.join("\n"),
                    stderr: if stderr_lines.is_empty() {
                        "Job cancelled".to_string()
                    } else {
                        format!("{}\nJob cancelled", stderr_lines.join("\n"))
                    },
                    output: parsed_output,
                })
            }
            Outcome::Streams(result) => {
                let (stdout_lines, stderr_lines, parsed_output) = result?;

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
        };

        // Clean up temp script file if we created one
        if let Some(ref path) = temp_script_path {
            script_exec::cleanup_temp_script(path);
        }

        result
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
            action_type: "script".to_string(),
            image: None,
            runner_mode: crate::RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        }
    }

    #[tokio::test]
    async fn test_simple_echo() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("echo hello world"), None, HashMap::new());
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello world"));
        assert!(result.output.is_none());
    }

    #[tokio::test]
    async fn test_exit_code() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("exit 42"), None, HashMap::new());
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn test_env_vars() {
        let runner = ShellRunner::new();
        let mut env = HashMap::new();
        env.insert("MY_VAR".to_string(), "my_value".to_string());
        let config = shell_config(Some("echo $MY_VAR"), None, env);
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
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
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        let output = result.output.unwrap();
        assert_eq!(output["greeting"], "hello");
    }

    #[tokio::test]
    async fn test_stderr_capture() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("echo error >&2"), None, HashMap::new());
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
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
        let result = runner
            .execute(config, Some(callback), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        let captured = lines.lock().unwrap();
        assert!(captured.len() >= 2);
    }

    #[tokio::test]
    async fn test_working_directory() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("pwd"), None, HashMap::new());
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
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
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert!(result.stdout.contains("line1"));
        assert!(result.stdout.contains("line2"));
        assert!(result.stdout.contains("line3"));
    }

    #[tokio::test]
    async fn test_no_cmd_or_script_errors() {
        let runner = ShellRunner::new();
        let config = shell_config(None, None, HashMap::new());
        let result = runner.execute(config, None, CancellationToken::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cancellation_kills_process() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("sleep 60"), None, HashMap::new());
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel after 100ms
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            token_clone.cancel();
        });

        let result = runner.execute(config, None, token).await.unwrap();
        assert_eq!(result.exit_code, -1);
        assert_eq!(result.stderr, "Job cancelled");
    }

    #[tokio::test]
    async fn test_cancellation_token_not_cancelled_runs_normally() {
        let runner = ShellRunner::new();
        let config = shell_config(Some("echo still-running"), None, HashMap::new());
        // Token exists but is never cancelled
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("still-running"));
    }

    #[tokio::test]
    async fn test_nonexistent_workdir() {
        let runner = ShellRunner::new();
        let config = RunConfig {
            cmd: Some("echo hi".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/nonexistent/path/12345".to_string(),
            action_type: "script".to_string(),
            image: None,
            runner_mode: crate::RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };
        // spawn will fail because the working directory doesn't exist, or return non-zero
        let result = runner.execute(config, None, CancellationToken::new()).await;
        match result {
            Err(_) => {} // IO error from spawn
            Ok(run_result) => assert_ne!(run_result.exit_code, 0),
        }
    }

    #[tokio::test]
    async fn test_invalid_command_binary() {
        let runner = ShellRunner::new();
        // sh -c with an unknown binary returns exit code 127
        let config = shell_config(
            Some("this_binary_does_not_exist_12345"),
            None,
            HashMap::new(),
        );
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert_ne!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn test_script_file_not_found() {
        let runner = ShellRunner::new();
        // script mode directly spawns the path as a binary, so spawn fails
        let config = shell_config(None, Some("/tmp/no_such_script_12345.sh"), HashMap::new());
        let result = runner.execute(config, None, CancellationToken::new()).await;
        assert!(
            result.is_err(),
            "expected error when script file is missing"
        );
    }
}
