use crate::script_exec;
use crate::traits::{
    parse_output_line, LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner,
};
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::sync::Arc;
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
                if !config.args.is_empty() {
                    c.arg("sh"); // $0 placeholder for sh -c positional args
                    for a in &config.args {
                        c.arg(a);
                    }
                }
                c
            } else if let Some(ref script) = config.script {
                let mut c = tokio::process::Command::new(script);
                for a in &config.args {
                    c.arg(a);
                }
                c
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
                &config.args,
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
                &config.args,
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

        // Wrap callback in Arc so both stream tasks can share it
        let log_callback = log_callback.map(Arc::new);

        // Read both streams concurrently, streaming logs inline
        let cb_stdout = log_callback.clone();
        let stdout_handle = tokio::spawn(async move {
            let mut lines = stdout_reader.lines();
            let mut collected = Vec::new();
            let mut output = None;

            while let Some(line) = lines.next_line().await.transpose() {
                match line {
                    Ok(line) => {
                        if let Some(ref cb) = cb_stdout {
                            cb(LogLine {
                                stream: LogStream::Stdout,
                                line: line.clone(),
                                timestamp: chrono::Utc::now(),
                            });
                        }

                        // Check for OUTPUT: prefix (borrow before move)
                        if let Some(parsed) = parse_output_line(&line) {
                            output = Some(parsed);
                        }
                        collected.push(line);
                    }
                    Err(e) => {
                        tracing::warn!("Error reading stdout line: {:#}", e);
                        break;
                    }
                }
            }

            (collected, output)
        });

        let cb_stderr = log_callback.clone();
        let stderr_handle = tokio::spawn(async move {
            let mut lines = stderr_reader.lines();
            let mut collected = Vec::new();

            while let Some(line) = lines.next_line().await.transpose() {
                match line {
                    Ok(line) => {
                        if let Some(ref cb) = cb_stderr {
                            cb(LogLine {
                                stream: LogStream::Stderr,
                                line: line.clone(),
                                timestamp: chrono::Utc::now(),
                            });
                        }
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

                // Logs already streamed inline by the reader tasks

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

                // Logs already streamed inline by the reader tasks

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
            args: vec![],
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
            args: vec![],
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

    #[tokio::test]
    async fn test_callback_streams_during_execution() {
        let runner = ShellRunner::new();
        let first_seen = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let first_seen_cb = first_seen.clone();

        let callback: LogCallback = Box::new(move |_line| {
            let mut guard = first_seen_cb.lock().unwrap();
            if guard.is_none() {
                *guard = Some(std::time::Instant::now());
            }
        });

        let start = std::time::Instant::now();
        // Print first line, then sleep 300ms before second line
        let config = shell_config(
            Some("echo first && sleep 0.3 && echo second"),
            None,
            HashMap::new(),
        );
        let result = runner
            .execute(config, Some(callback), CancellationToken::new())
            .await
            .unwrap();
        let total_elapsed = start.elapsed();

        assert_eq!(result.exit_code, 0);
        let first_at = first_seen.lock().unwrap().expect("callback never fired");
        // First callback must arrive well before the process exits
        assert!(
            first_at < start + total_elapsed / 2,
            "callback arrived too late ({:?} into {:?} run), not streaming",
            first_at.duration_since(start),
            total_elapsed
        );
    }

    #[tokio::test]
    async fn test_callback_stream_labels() {
        let runner = ShellRunner::new();
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<LogLine>::new()));
        let lines_clone = lines.clone();
        let callback: LogCallback = Box::new(move |line| {
            lines_clone.lock().unwrap().push(line);
        });

        let config = shell_config(
            Some("echo to_stdout && echo to_stderr >&2"),
            None,
            HashMap::new(),
        );
        runner
            .execute(config, Some(callback), CancellationToken::new())
            .await
            .unwrap();

        let captured = lines.lock().unwrap();
        assert_eq!(captured.len(), 2);

        let stdout_lines: Vec<_> = captured
            .iter()
            .filter(|l| l.stream == LogStream::Stdout)
            .collect();
        let stderr_lines: Vec<_> = captured
            .iter()
            .filter(|l| l.stream == LogStream::Stderr)
            .collect();

        assert_eq!(stdout_lines.len(), 1);
        assert_eq!(stderr_lines.len(), 1);
        assert!(stdout_lines[0].line.contains("to_stdout"));
        assert!(stderr_lines[0].line.contains("to_stderr"));
    }

    #[tokio::test]
    async fn test_cancellation_streams_partial_logs() {
        let runner = ShellRunner::new();
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<LogLine>::new()));
        let lines_clone = lines.clone();
        let callback: LogCallback = Box::new(move |line| {
            lines_clone.lock().unwrap().push(line);
        });

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let config = shell_config(Some("echo before_cancel && sleep 10"), None, HashMap::new());

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            token_clone.cancel();
        });

        let result = runner.execute(config, Some(callback), token).await.unwrap();

        assert_eq!(result.exit_code, -1);
        let captured = lines.lock().unwrap();
        assert!(
            captured.iter().any(|l| l.line.contains("before_cancel")),
            "partial log line missing after cancellation"
        );
    }

    #[tokio::test]
    async fn test_shell_inline_with_args() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = shell_config(Some("echo $1 $2"), None, HashMap::new());
        config.workdir = dir.path().to_str().unwrap().to_string();
        config.args = vec!["hello".to_string(), "world".to_string()];

        let runner = ShellRunner;
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);
        let stdout = String::from_utf8_lossy(result.stdout.as_bytes()).to_string();
        assert!(
            stdout.contains("hello world"),
            "expected args as $1 $2, got: {}",
            stdout
        );
    }

    #[tokio::test]
    async fn test_shell_source_with_args() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("test_args.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho \"arg1=$1 arg2=$2\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut config = shell_config(None, None, HashMap::new());
        config.workdir = dir.path().to_str().unwrap().to_string();
        config.script = Some(script_path.to_string_lossy().to_string());
        config.args = vec!["foo".to_string(), "bar".to_string()];

        let runner = ShellRunner;
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);
        let stdout = String::from_utf8_lossy(result.stdout.as_bytes()).to_string();
        assert!(
            stdout.contains("arg1=foo") && stdout.contains("arg2=bar"),
            "expected positional args, got: {}",
            stdout
        );
    }

    #[tokio::test]
    async fn test_shell_inline_empty_args_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = shell_config(Some("echo hello"), None, HashMap::new());
        config.workdir = dir.path().to_str().unwrap().to_string();
        config.args = vec![];

        let runner = ShellRunner;
        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.exit_code, 0);
        let stdout = String::from_utf8_lossy(result.stdout.as_bytes()).to_string();
        assert!(stdout.contains("hello"), "got: {}", stdout);
    }

    #[tokio::test]
    async fn test_cancellation_no_duplicate_callbacks() {
        let runner = ShellRunner::new();
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = call_count.clone();

        let callback: LogCallback = Box::new(move |_| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let config = shell_config(Some("echo once && sleep 10"), None, HashMap::new());

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            token_clone.cancel();
        });

        runner.execute(config, Some(callback), token).await.unwrap();

        let count = call_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            count, 1,
            "expected 1 callback, got {} — possible duplicate firing",
            count
        );
    }
}
