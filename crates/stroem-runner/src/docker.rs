use crate::script_exec;
use crate::traits::{
    parse_output_line, LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner, RunnerMode,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, CreateImageOptions, RemoveContainerOptions,
    StartContainerOptions, WaitContainerOptions,
};
use bollard::Docker;
use futures_util::StreamExt;
use stroem_common::language::ScriptLanguage;
use tokio_util::sync::CancellationToken;

use stroem_common::constants::DEFAULT_RUNNER_IMAGE;

/// Docker runner that executes commands inside Docker containers
pub struct DockerRunner {
    docker: Docker,
}

impl DockerRunner {
    /// Create a new DockerRunner connecting to the default Docker socket
    pub fn new() -> Result<Self> {
        let docker =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;
        Ok(Self { docker })
    }

    /// Create a new DockerRunner connecting to a specific Docker host
    pub fn with_host(host: &str) -> Result<Self> {
        let docker = Docker::connect_with_http_defaults()
            .with_context(|| format!("Failed to connect to Docker at {}", host))?;
        Ok(Self { docker })
    }

    /// Build container config from RunConfig
    fn build_container_config(config: &RunConfig) -> ContainerCreateBody {
        let env: Vec<String> = config.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

        match config.runner_mode {
            RunnerMode::WithWorkspace => {
                // Script action: shell in container with workspace bind-mount
                let image = config
                    .image
                    .as_deref()
                    .unwrap_or(DEFAULT_RUNNER_IMAGE)
                    .to_string();

                let lang = ScriptLanguage::from_str_opt(config.language.as_deref());
                let cmd = if let Some(ref command) = config.cmd {
                    if lang.is_shell()
                        && config.dependencies.is_empty()
                        && config.interpreter.is_none()
                    {
                        vec!["sh".to_string(), "-c".to_string(), command.clone()]
                    } else {
                        script_exec::build_container_script_cmd(
                            command,
                            lang,
                            &config.dependencies,
                            config.interpreter.as_deref(),
                        )
                    }
                } else if let Some(ref script) = config.script {
                    // `script` is already an absolute path resolved by the executor
                    // (e.g. "/workspace/actions/hello.py").
                    if lang.is_shell()
                        && config.dependencies.is_empty()
                        && config.interpreter.is_none()
                    {
                        vec!["sh".to_string(), "-c".to_string(), script.clone()]
                    } else {
                        // Non-shell language or deps/interpreter specified: invoke the correct
                        // interpreter on the file rather than running it with sh.
                        script_exec::build_container_file_cmd(
                            script,
                            lang,
                            &config.dependencies,
                            config.interpreter.as_deref(),
                        )
                    }
                } else {
                    vec!["echo".to_string(), "No command specified".to_string()]
                };

                let mut binds = Vec::new();
                if !config.workdir.is_empty() {
                    binds.push(format!("{}:/workspace:ro", config.workdir));
                }
                // Mount startup scripts (if present on Docker host, otherwise empty dir)
                binds.push("/etc/stroem/startup.d:/etc/stroem/startup.d:ro".to_string());

                ContainerCreateBody {
                    image: Some(image),
                    cmd: Some(cmd),
                    env: Some(env),
                    working_dir: Some("/workspace".to_string()),
                    host_config: Some(HostConfig {
                        binds: Some(binds),
                        ..Default::default()
                    }),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                }
            }
            RunnerMode::NoWorkspace => {
                // Container action: run user's prepared image as-is
                let image = config
                    .image
                    .as_deref()
                    .unwrap_or(DEFAULT_RUNNER_IMAGE)
                    .to_string();

                // Use entrypoint if set, otherwise let Docker image defaults apply
                let entrypoint = config.entrypoint.clone();

                // Determine cmd: explicit command > cmd > image defaults (None)
                let lang = ScriptLanguage::from_str_opt(config.language.as_deref());
                let cmd = if let Some(ref command) = config.command {
                    Some(command.clone())
                } else {
                    config.cmd.as_ref().map(|c| {
                        if lang.is_shell()
                            && config.dependencies.is_empty()
                            && config.interpreter.is_none()
                        {
                            vec!["sh".to_string(), "-c".to_string(), c.clone()]
                        } else {
                            script_exec::build_container_script_cmd(
                                c,
                                lang,
                                &config.dependencies,
                                config.interpreter.as_deref(),
                            )
                        }
                    })
                };

                ContainerCreateBody {
                    image: Some(image),
                    entrypoint,
                    cmd,
                    env: Some(env),
                    // No workspace bind mount, no workdir override
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                }
            }
        }
    }
}

#[async_trait]
impl Runner for DockerRunner {
    async fn execute(
        &self,
        config: RunConfig,
        log_callback: Option<LogCallback>,
        cancel_token: CancellationToken,
    ) -> Result<RunResult> {
        let image = config
            .image
            .as_deref()
            .unwrap_or(DEFAULT_RUNNER_IMAGE)
            .to_string();

        // Pull image
        tracing::info!("Pulling image: {}", image);
        let mut pull_stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: Some(image.clone()),
                ..Default::default()
            }),
            None,
            None,
        );
        while let Some(result) = pull_stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(status) = info.status {
                        tracing::debug!("Pull: {}", status);
                    }
                }
                Err(e) => {
                    tracing::warn!("Image pull warning: {:#}", e);
                }
            }
        }

        // Create container
        let container_config = Self::build_container_config(&config);
        let container = self
            .docker
            .create_container(None::<CreateContainerOptions>, container_config)
            .await
            .context("Failed to create container")?;
        let container_id = container.id.clone();

        tracing::info!("Created container: {}", &container_id[..12]);

        // Start container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
            .context("Failed to start container")?;

        // Attach to stdout/stderr
        let attach_opts = AttachContainerOptions {
            stdout: true,
            stderr: true,
            stream: true,
            logs: true,
            ..Default::default()
        };

        let mut output = self
            .docker
            .attach_container(&container_id, Some(attach_opts))
            .await
            .context("Failed to attach to container")?;

        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();
        let mut parsed_output = None;

        // Helper closure to stop and remove a container on cancellation
        let stop_and_remove = |docker: &Docker, id: &str| {
            let docker = docker.clone();
            let id = id.to_string();
            async move {
                if let Err(e) = docker.stop_container(&id, None).await {
                    tracing::warn!("Failed to stop container {} on cancel: {:#}", &id[..12], e);
                }
                let remove_opts = RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                };
                if let Err(e) = docker.remove_container(&id, Some(remove_opts)).await {
                    tracing::warn!(
                        "Failed to remove container {} on cancel: {:#}",
                        &id[..12],
                        e
                    );
                }
            }
        };

        // Stream logs, with cancellation support
        loop {
            tokio::select! {
                biased;

                () = cancel_token.cancelled() => {
                    tracing::info!("Cancellation requested — stopping container {}", &container_id[..12]);
                    stop_and_remove(&self.docker, &container_id).await;
                    return Ok(RunResult {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "Job cancelled".to_string(),
                        output: None,
                    });
                }

                chunk = output.output.next() => {
                    let Some(chunk) = chunk else { break };
                    match chunk {
                        Ok(log_output) => {
                            let line = log_output.to_string();
                            let line = line.trim_end_matches('\n').to_string();

                            if line.is_empty() {
                                continue;
                            }

                            let stream = match log_output {
                                LogOutput::StdErr { .. } => LogStream::Stderr,
                                _ => LogStream::Stdout,
                            };

                            match stream {
                                LogStream::Stdout => {
                                    // Check for OUTPUT: prefix
                                    if let Some(parsed) = parse_output_line(&line) {
                                        parsed_output = Some(parsed);
                                    }
                                    stdout_lines.push(line.clone());
                                }
                                LogStream::Stderr => {
                                    stderr_lines.push(line.clone());
                                }
                            }

                            if let Some(ref callback) = log_callback {
                                callback(LogLine {
                                    stream,
                                    line,
                                    timestamp: chrono::Utc::now(),
                                });
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Error reading container output: {:#}", e);
                            break;
                        }
                    }
                }
            }
        }

        // Wait for container to finish
        let mut wait_stream = self
            .docker
            .wait_container(&container_id, None::<WaitContainerOptions>);

        let mut exit_code = -1i32;
        loop {
            tokio::select! {
                biased;

                () = cancel_token.cancelled() => {
                    tracing::info!("Cancellation requested during wait — stopping container {}", &container_id[..12]);
                    stop_and_remove(&self.docker, &container_id).await;
                    return Ok(RunResult {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "Job cancelled".to_string(),
                        output: None,
                    });
                }

                result = wait_stream.next() => {
                    let Some(result) = result else { break };
                    match result {
                        Ok(response) => {
                            exit_code = response.status_code as i32;
                        }
                        Err(e) => {
                            tracing::warn!("Error waiting for container: {:#}", e);
                        }
                    }
                }
            }
        }

        // Remove container
        let remove_opts = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };
        if let Err(e) = self
            .docker
            .remove_container(&container_id, Some(remove_opts))
            .await
        {
            tracing::warn!(
                "Failed to remove container {}: {:#}",
                &container_id[..12],
                e
            );
        }

        Ok(RunResult {
            exit_code,
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

    #[test]
    fn test_build_container_config() {
        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: {
                let mut env = HashMap::new();
                env.insert("FOO".to_string(), "bar".to_string());
                env
            },
            workdir: "/tmp/workspace".to_string(),
            action_type: "docker".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);

        assert_eq!(container_config.image, Some("python:3.12".to_string()));
        assert_eq!(
            container_config.cmd,
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo hello".to_string()
            ])
        );

        let env = container_config.env.unwrap();
        assert!(env.contains(&"FOO=bar".to_string()));

        let host_config = container_config.host_config.unwrap();
        let binds = host_config.binds.unwrap();
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0], "/tmp/workspace:/workspace:ro");
        assert_eq!(binds[1], "/etc/stroem/startup.d:/etc/stroem/startup.d:ro");

        assert_eq!(container_config.working_dir, Some("/workspace".to_string()));
    }

    #[test]
    fn test_build_container_config_default_image() {
        let config = RunConfig {
            cmd: Some("ls".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: None,
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        assert_eq!(
            container_config.image,
            Some("ghcr.io/fremvaerk/stroem-runner:latest".to_string())
        );
    }

    #[test]
    fn test_build_container_config_with_script() {
        let config = RunConfig {
            cmd: None,
            script: Some("/workspace/deploy.sh".to_string()),
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("ubuntu:22.04".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        assert_eq!(
            container_config.cmd,
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "/workspace/deploy.sh".to_string()
            ])
        );
    }

    #[test]
    fn test_build_container_config_with_python_script() {
        // Non-shell script files must be invoked with the correct interpreter,
        // not wrapped in `sh -c <path>` which would fail for .py files.
        let config = RunConfig {
            cmd: None,
            script: Some("/workspace/actions/hello.py".to_string()),
            env: HashMap::new(),
            workdir: "/workspace".to_string(),
            action_type: "docker".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: Some("python".to_string()),
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let cmd = container_config.cmd.unwrap();
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Must use interpreter detection, NOT sh -c <path>
        assert!(
            cmd[2].contains("command -v uv") || cmd[2].contains("command -v python"),
            "expected interpreter detection chain, got: {}",
            cmd[2]
        );
        assert!(
            cmd[2].contains("/workspace/actions/hello.py"),
            "must reference the script file, got: {}",
            cmd[2]
        );
        // Must NOT just execute the file as a shell script
        assert_ne!(
            cmd[2], "/workspace/actions/hello.py",
            "must not use bare sh -c <path> for non-shell scripts"
        );
    }

    #[test]
    fn test_build_container_config_with_python_script_and_deps() {
        let config = RunConfig {
            cmd: None,
            script: Some("/workspace/actions/fetch.py".to_string()),
            env: HashMap::new(),
            workdir: "/workspace".to_string(),
            action_type: "docker".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: Some("python".to_string()),
            dependencies: vec!["requests".to_string()],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let cmd = container_config.cmd.unwrap();
        assert!(
            cmd[2].contains("/workspace/actions/fetch.py"),
            "must reference the script file"
        );
        assert!(cmd[2].contains("requests"), "must include the dependency");
    }

    #[test]
    fn test_build_container_config_with_python_script_interpreter_override() {
        let config = RunConfig {
            cmd: None,
            script: Some("/workspace/actions/run.py".to_string()),
            env: HashMap::new(),
            workdir: "/workspace".to_string(),
            action_type: "docker".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: Some("python".to_string()),
            dependencies: vec![],
            interpreter: Some("python3.12".to_string()),
        };

        let container_config = DockerRunner::build_container_config(&config);
        let cmd = container_config.cmd.unwrap();
        assert!(
            cmd[2].contains("python3.12 /workspace/actions/run.py"),
            "must use the interpreter override: {}",
            cmd[2]
        );
    }

    #[test]
    fn test_build_container_config_startup_bind_mount() {
        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp/workspace".to_string(),
            action_type: "docker".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let binds = container_config.host_config.unwrap().binds.unwrap();
        assert!(
            binds
                .iter()
                .any(|b| b == "/etc/stroem/startup.d:/etc/stroem/startup.d:ro"),
            "WithWorkspace mode should always include startup.d bind mount"
        );
    }

    #[test]
    fn test_build_container_config_no_workspace() {
        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: {
                let mut env = HashMap::new();
                env.insert("FOO".to_string(), "bar".to_string());
                env
            },
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);

        assert_eq!(container_config.image, Some("python:3.12".to_string()));
        // Without entrypoint, cmd gets wrapped in sh -c
        assert_eq!(
            container_config.cmd,
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo hello".to_string()
            ])
        );

        // No workspace bind mount in NoWorkspace mode
        assert!(container_config.host_config.is_none());
        // No workdir override
        assert!(container_config.working_dir.is_none());
    }

    #[test]
    fn test_build_container_config_no_workspace_with_entrypoint() {
        let config = RunConfig {
            cmd: None,
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("company/deploy:v3".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: Some(vec!["/app/run".to_string()]),
            command: Some(vec!["--env".to_string(), "prod".to_string()]),
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);

        assert_eq!(
            container_config.entrypoint,
            Some(vec!["/app/run".to_string()])
        );
        assert_eq!(
            container_config.cmd,
            Some(vec!["--env".to_string(), "prod".to_string()])
        );
    }

    #[test]
    fn test_build_container_config_no_workspace_image_defaults() {
        // Container action (docker) with no cmd, no entrypoint — image defaults should apply
        let config = RunConfig {
            cmd: None,
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("company/migrations:v3".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);

        assert_eq!(
            container_config.image,
            Some("company/migrations:v3".to_string())
        );
        // No entrypoint or cmd overrides — image defaults apply
        assert!(container_config.entrypoint.is_none());
        assert!(container_config.cmd.is_none());
    }

    #[test]
    fn test_build_container_config_env_formatting() {
        // Verify multiple env vars are formatted as KEY=VALUE strings
        let mut env = HashMap::new();
        env.insert("KEY_A".to_string(), "value_a".to_string());
        env.insert("KEY_B".to_string(), "value=with=equals".to_string());
        let config = RunConfig {
            cmd: Some("env".to_string()),
            script: None,
            env,
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let env_vec = container_config.env.unwrap();
        assert!(env_vec.contains(&"KEY_A=value_a".to_string()));
        assert!(env_vec.contains(&"KEY_B=value=with=equals".to_string()));
    }

    #[test]
    fn test_build_container_config_empty_workdir_no_bind() {
        // Empty workdir string should produce no workspace bind mount
        let config = RunConfig {
            cmd: Some("ls".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: String::new(),
            action_type: "script".to_string(),
            image: None,
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let binds = container_config.host_config.unwrap().binds.unwrap();
        // Only the startup.d bind should be present — no workspace bind for empty workdir
        assert!(!binds.iter().any(|b| b.contains(":/workspace:ro")));
        assert!(binds
            .iter()
            .any(|b| b == "/etc/stroem/startup.d:/etc/stroem/startup.d:ro"));
    }

    #[test]
    fn test_build_container_config_no_workspace_env_present() {
        // NoWorkspace mode should still pass env vars through
        let mut env = HashMap::new();
        env.insert("TOKEN".to_string(), "secret123".to_string());
        let config = RunConfig {
            cmd: None,
            script: None,
            env,
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let env_vec = container_config.env.unwrap();
        assert!(env_vec.contains(&"TOKEN=secret123".to_string()));
        // No workspace bind mounts in NoWorkspace mode
        assert!(container_config.host_config.is_none());
    }

    #[test]
    fn test_build_container_config_with_python_cmd() {
        // WithWorkspace mode: language=python, cmd set → must route through
        // build_container_script_cmd (heredoc) rather than plain `sh -c`.
        let config = RunConfig {
            cmd: Some("print('hello')".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp/workspace".to_string(),
            action_type: "shell".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: Some("python".to_string()),
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let cmd = container_config.cmd.unwrap();
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Must use heredoc (build_container_script_cmd path), not plain `sh -c <cmd>`.
        assert!(
            cmd[2].contains("STROEM_EOF_"),
            "python cmd must use heredoc; got: {}",
            cmd[2]
        );
        assert!(
            cmd[2].contains("print('hello')"),
            "heredoc must embed the cmd content; got: {}",
            cmd[2]
        );
        // Must probe for a Python interpreter
        assert!(
            cmd[2].contains("command -v uv") || cmd[2].contains("command -v python"),
            "must include interpreter detection; got: {}",
            cmd[2]
        );
    }

    #[test]
    fn test_build_container_config_no_workspace_with_python_cmd() {
        // NoWorkspace mode: language=python, cmd set → must also route through
        // build_container_script_cmd, not the plain `sh -c` path.
        let config = RunConfig {
            cmd: Some("print('hello from nows')".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: Some("python".to_string()),
            dependencies: vec![],
            interpreter: None,
        };

        let container_config = DockerRunner::build_container_config(&config);
        let cmd = container_config.cmd.unwrap();
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Must use heredoc path (build_container_script_cmd), not plain `sh -c <cmd>`.
        assert!(
            cmd[2].contains("STROEM_EOF_"),
            "python cmd in NoWorkspace must use heredoc; got: {}",
            cmd[2]
        );
        assert!(
            cmd[2].contains("print('hello from nows')"),
            "heredoc must embed the cmd content; got: {}",
            cmd[2]
        );
        // No workspace bind mount in NoWorkspace mode
        assert!(container_config.host_config.is_none());
    }

    /// Integration test: requires Docker daemon running.
    /// Run with: cargo test -p stroem-runner --features docker -- --ignored test_docker_echo
    #[tokio::test]
    #[ignore]
    async fn test_docker_echo() {
        let runner = DockerRunner::new().expect("Docker must be available");
        let config = RunConfig {
            cmd: Some("echo hello-from-docker".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello-from-docker"));
    }

    /// Integration test: Docker cancellation kills the container.
    /// Run with: cargo test -p stroem-runner --features docker -- --ignored test_docker_cancellation
    #[tokio::test]
    #[ignore]
    async fn test_docker_cancellation() {
        let runner = DockerRunner::new().expect("Docker must be available");
        let config = RunConfig {
            cmd: Some("sleep 60".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "docker".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
        };

        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel after 500ms
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            token_clone.cancel();
        });

        let result = runner.execute(config, None, token).await.unwrap();
        assert_eq!(result.exit_code, -1);
        assert!(result.stderr.contains("Job cancelled"));
    }
}
