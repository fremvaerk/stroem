use crate::traits::{LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner, RunnerMode};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::container::{
    AttachContainerOptions, Config, CreateContainerOptions, RemoveContainerOptions,
    StartContainerOptions, WaitContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use futures_util::StreamExt;

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
            .context(format!("Failed to connect to Docker at {}", host))?;
        Ok(Self { docker })
    }

    /// Build container config from RunConfig
    fn build_container_config(config: &RunConfig) -> Config<String> {
        let env: Vec<String> = config.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

        match config.runner_mode {
            RunnerMode::WithWorkspace => {
                // Type 2: Shell in container with workspace bind-mount
                let image = config
                    .image
                    .as_deref()
                    .unwrap_or("alpine:latest")
                    .to_string();

                let cmd = if let Some(ref command) = config.cmd {
                    vec!["sh".to_string(), "-c".to_string(), command.clone()]
                } else if let Some(ref script) = config.script {
                    vec!["sh".to_string(), "-c".to_string(), script.clone()]
                } else {
                    vec!["echo".to_string(), "No command specified".to_string()]
                };

                let mut binds = Vec::new();
                if !config.workdir.is_empty() {
                    binds.push(format!("{}:/workspace:ro", config.workdir));
                }

                Config {
                    image: Some(image),
                    cmd: Some(cmd),
                    env: Some(env),
                    working_dir: Some("/workspace".to_string()),
                    host_config: Some(bollard::models::HostConfig {
                        binds: Some(binds),
                        ..Default::default()
                    }),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                }
            }
            RunnerMode::NoWorkspace => {
                // Type 1: Run user's prepared image as-is
                let image = config
                    .image
                    .as_deref()
                    .unwrap_or("alpine:latest")
                    .to_string();

                // Use entrypoint if set, otherwise let Docker image defaults apply
                let entrypoint = config.entrypoint.clone();

                // Determine cmd: explicit command > cmd > image defaults (None)
                let cmd = if let Some(ref command) = config.command {
                    Some(command.clone())
                } else {
                    config
                        .cmd
                        .as_ref()
                        .map(|c| vec!["sh".to_string(), "-c".to_string(), c.clone()])
                };

                Config {
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
    ) -> Result<RunResult> {
        let image = config
            .image
            .as_deref()
            .unwrap_or("alpine:latest")
            .to_string();

        // Pull image
        tracing::info!("Pulling image: {}", image);
        let mut pull_stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: image.clone(),
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
                    tracing::warn!("Image pull warning: {}", e);
                }
            }
        }

        // Create container
        let container_config = Self::build_container_config(&config);
        let container = self
            .docker
            .create_container(None::<CreateContainerOptions<String>>, container_config)
            .await
            .context("Failed to create container")?;
        let container_id = container.id.clone();

        tracing::info!("Created container: {}", &container_id[..12]);

        // Start container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
            .context("Failed to start container")?;

        // Attach to stdout/stderr
        let attach_opts = AttachContainerOptions::<String> {
            stdout: Some(true),
            stderr: Some(true),
            stream: Some(true),
            logs: Some(true),
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

        while let Some(chunk) = output.output.next().await {
            match chunk {
                Ok(log_output) => {
                    let line = log_output.to_string();
                    let line = line.trim_end_matches('\n').to_string();

                    if line.is_empty() {
                        continue;
                    }

                    let stream = match log_output {
                        bollard::container::LogOutput::StdErr { .. } => LogStream::Stderr,
                        _ => LogStream::Stdout,
                    };

                    match stream {
                        LogStream::Stdout => {
                            // Check for OUTPUT: prefix
                            if let Some(json_str) = line.strip_prefix("OUTPUT: ") {
                                if let Ok(parsed) =
                                    serde_json::from_str::<serde_json::Value>(json_str)
                                {
                                    parsed_output = Some(parsed);
                                }
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
                    tracing::warn!("Error reading container output: {}", e);
                    break;
                }
            }
        }

        // Wait for container to finish
        let mut wait_stream = self
            .docker
            .wait_container(&container_id, None::<WaitContainerOptions<String>>);

        let mut exit_code = -1i32;
        while let Some(result) = wait_stream.next().await {
            match result {
                Ok(response) => {
                    exit_code = response.status_code as i32;
                }
                Err(e) => {
                    tracing::warn!("Error waiting for container: {}", e);
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
            tracing::warn!("Failed to remove container {}: {}", &container_id[..12], e);
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
        assert_eq!(binds, vec!["/tmp/workspace:/workspace:ro"]);

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
        };

        let container_config = DockerRunner::build_container_config(&config);
        assert_eq!(container_config.image, Some("alpine:latest".to_string()));
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
        // Type 1 with no cmd, no entrypoint — image defaults should apply
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
        };

        let result = runner.execute(config, None).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello-from-docker"));
    }
}
