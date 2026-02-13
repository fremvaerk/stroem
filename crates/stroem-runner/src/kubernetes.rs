use crate::traits::{LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner, RunnerMode};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::io::AsyncBufReadExt;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, LogParams, PostParams};
use kube::Client;
use std::collections::HashMap;

/// Kubernetes runner that executes commands inside Kubernetes pods
pub struct KubeRunner {
    namespace: String,
    server_url: String,
    worker_token: String,
    init_image: String,
}

impl KubeRunner {
    /// Create a new KubeRunner
    ///
    /// - `namespace`: Kubernetes namespace to create pods in
    /// - `server_url`: Strøm server URL for workspace tarball downloads
    /// - `worker_token`: Bearer token for authenticating with the server
    pub fn new(namespace: String, server_url: String, worker_token: String) -> Self {
        Self {
            namespace,
            server_url,
            worker_token,
            init_image: "curlimages/curl:latest".to_string(),
        }
    }

    /// Set custom init container image (default: curlimages/curl:latest)
    pub fn with_init_image(mut self, image: String) -> Self {
        self.init_image = image;
        self
    }

    /// Build a pod spec for executing a step
    fn build_pod_spec(
        &self,
        pod_name: &str,
        config: &RunConfig,
        workspace_name: &str,
        labels: HashMap<String, String>,
    ) -> Pod {
        let image = config
            .image
            .as_deref()
            .unwrap_or("alpine:latest")
            .to_string();

        let env: Vec<serde_json::Value> = config
            .env
            .iter()
            .map(|(k, v)| {
                serde_json::json!({
                    "name": k,
                    "value": v,
                })
            })
            .collect();

        match config.runner_mode {
            RunnerMode::WithWorkspace => {
                // Type 2: Shell in container with workspace via init container
                let cmd = if let Some(ref command) = config.cmd {
                    vec!["sh".to_string(), "-c".to_string(), command.clone()]
                } else if let Some(ref script) = config.script {
                    vec!["sh".to_string(), "-c".to_string(), script.clone()]
                } else {
                    vec!["echo".to_string(), "No command specified".to_string()]
                };

                let tarball_url = format!(
                    "{}/worker/workspace/{}.tar.gz",
                    self.server_url, workspace_name
                );

                let pod: Pod = serde_json::from_value(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {
                        "name": pod_name,
                        "labels": labels,
                    },
                    "spec": {
                        "restartPolicy": "Never",
                        "initContainers": [{
                            "name": "workspace-init",
                            "image": self.init_image,
                            "command": ["sh", "-c", format!(
                                "curl -sSf -H 'Authorization: Bearer {}' '{}' | tar xz -C /workspace",
                                self.worker_token, tarball_url
                            )],
                            "volumeMounts": [{
                                "name": "workspace",
                                "mountPath": "/workspace",
                            }],
                        }],
                        "containers": [{
                            "name": "step",
                            "image": image,
                            "command": cmd,
                            "env": env,
                            "workingDir": "/workspace",
                            "volumeMounts": [{
                                "name": "workspace",
                                "mountPath": "/workspace",
                            }],
                        }],
                        "volumes": [{
                            "name": "workspace",
                            "emptyDir": {},
                        }],
                    },
                }))
                .expect("Pod spec should be valid JSON");
                pod
            }
            RunnerMode::NoWorkspace => {
                // Type 1: Run user's prepared image, no init container, no workspace volume
                let mut container = serde_json::json!({
                    "name": "step",
                    "image": image,
                    "env": env,
                });

                // Set entrypoint if provided
                if let Some(ref ep) = config.entrypoint {
                    container["command"] = serde_json::json!(ep);
                }

                // Set cmd/command args
                if let Some(ref command) = config.command {
                    container["args"] = serde_json::json!(command);
                } else if let Some(ref cmd) = config.cmd {
                    // If entrypoint is set, pass cmd as args
                    if config.entrypoint.is_some() {
                        container["args"] = serde_json::json!([cmd]);
                    } else {
                        container["command"] = serde_json::json!(["sh", "-c", cmd]);
                    }
                }
                // If nothing is set, image default entrypoint/cmd runs

                let pod: Pod = serde_json::from_value(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {
                        "name": pod_name,
                        "labels": labels,
                    },
                    "spec": {
                        "restartPolicy": "Never",
                        "containers": [container],
                    },
                }))
                .expect("Pod spec should be valid JSON");
                pod
            }
        }
    }

    /// Generate a pod name from job_id and step_name
    fn pod_name(job_id: &str, step_name: &str) -> String {
        // Take first 8 chars of job_id for brevity
        let job_short = &job_id[..job_id.len().min(8)];
        // Sanitize step name: lowercase, replace non-alphanumeric with dash
        let step_sanitized: String = step_name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        let step_sanitized = step_sanitized.trim_matches('-');

        // K8s name max 63 chars, must be DNS-1123 label
        let name = format!("stroem-{}-{}", job_short, step_sanitized);
        name.chars().take(63).collect()
    }

    /// Extract workspace name from the workdir or env
    fn workspace_from_config(config: &RunConfig) -> String {
        // The workspace name is passed via env by the executor
        config
            .env
            .get("STROEM_WORKSPACE")
            .cloned()
            .unwrap_or_else(|| "default".to_string())
    }
}

#[async_trait]
impl Runner for KubeRunner {
    async fn execute(
        &self,
        config: RunConfig,
        log_callback: Option<LogCallback>,
    ) -> Result<RunResult> {
        let client = Client::try_default()
            .await
            .context("Failed to create Kubernetes client")?;
        let pods: Api<Pod> = Api::namespaced(client, &self.namespace);

        let workspace_name = Self::workspace_from_config(&config);
        let job_id = config
            .env
            .get("STROEM_JOB_ID")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let step_name = config
            .env
            .get("STROEM_STEP_NAME")
            .cloned()
            .unwrap_or_else(|| "step".to_string());

        let pod_name = Self::pod_name(&job_id, &step_name);

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());
        labels.insert("stroem.io/job-id".to_string(), job_id.clone());
        labels.insert("stroem.io/step".to_string(), step_name.clone());

        let pod_spec = self.build_pod_spec(&pod_name, &config, &workspace_name, labels);

        // Create the pod
        tracing::info!("Creating pod: {}", pod_name);
        pods.create(&PostParams::default(), &pod_spec)
            .await
            .context("Failed to create pod")?;

        // Wait for pod to reach terminal state
        let mut exit_code = -1i32;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let pod = pods
                .get(&pod_name)
                .await
                .context("Failed to get pod status")?;

            if let Some(status) = &pod.status {
                if let Some(phase) = &status.phase {
                    match phase.as_str() {
                        "Succeeded" => {
                            exit_code = 0;
                            break;
                        }
                        "Failed" => {
                            // Try to get exit code from container status
                            if let Some(container_statuses) = &status.container_statuses {
                                for cs in container_statuses {
                                    if cs.name == "step" {
                                        if let Some(terminated) =
                                            cs.state.as_ref().and_then(|s| s.terminated.as_ref())
                                        {
                                            exit_code = terminated.exit_code;
                                        }
                                    }
                                }
                            }
                            break;
                        }
                        "Running" | "Pending" => {
                            tracing::debug!("Pod {} is {}", pod_name, phase);
                        }
                        _ => {
                            tracing::warn!("Pod {} in unexpected phase: {}", pod_name, phase);
                        }
                    }
                }
            }
        }

        // Stream logs from the step container
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();
        let mut parsed_output = None;

        let log_params = LogParams {
            container: Some("step".to_string()),
            ..Default::default()
        };

        match pods.log_stream(&pod_name, &log_params).await {
            Ok(stream) => {
                let reader = futures_util::io::BufReader::new(stream);
                let mut lines_stream = reader.lines();
                while let Some(line_result) = futures_util::StreamExt::next(&mut lines_stream).await
                {
                    let line = match line_result {
                        Ok(l) => l,
                        Err(e) => {
                            tracing::warn!("Error reading log line: {}", e);
                            break;
                        }
                    };

                    // Check for OUTPUT: prefix
                    if let Some(json_str) = line.strip_prefix("OUTPUT: ") {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                            parsed_output = Some(parsed);
                        }
                    }

                    stdout_lines.push(line.clone());

                    if let Some(ref callback) = log_callback {
                        callback(LogLine {
                            stream: LogStream::Stdout,
                            line,
                            timestamp: chrono::Utc::now(),
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to stream pod logs: {}", e);
                stderr_lines.push(format!("Failed to retrieve logs: {}", e));
            }
        }

        // Delete the pod
        if let Err(e) = pods.delete(&pod_name, &DeleteParams::default()).await {
            tracing::warn!("Failed to delete pod {}: {}", pod_name, e);
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

    #[test]
    fn test_pod_name() {
        let name = KubeRunner::pod_name("abcdef12-3456-7890", "my-step-name");
        assert_eq!(name, "stroem-abcdef12-my-step-name");
        assert!(name.len() <= 63);
    }

    #[test]
    fn test_pod_name_sanitization() {
        let name = KubeRunner::pod_name("12345678", "Step With Spaces!");
        assert_eq!(name, "stroem-12345678-step-with-spaces-");
    }

    #[test]
    fn test_pod_name_truncation() {
        let long_step = "a".repeat(100);
        let name = KubeRunner::pod_name("12345678", &long_step);
        assert!(name.len() <= 63);
    }

    #[test]
    fn test_build_pod_spec() {
        let runner = KubeRunner::new(
            "stroem".to_string(),
            "http://stroem-server:8080".to_string(),
            "test-token".to_string(),
        );

        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());

        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env,
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("python:3.12".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
        };

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());
        labels.insert("stroem.io/job-id".to_string(), "test-job".to_string());
        labels.insert("stroem.io/step".to_string(), "test-step".to_string());

        let pod = runner.build_pod_spec("test-pod", &config, "default", labels);

        // Verify metadata
        let metadata = pod.metadata;
        assert_eq!(metadata.name, Some("test-pod".to_string()));
        let labels = metadata.labels.unwrap();
        assert_eq!(labels.get("app"), Some(&"stroem-step".to_string()));
        assert_eq!(
            labels.get("stroem.io/job-id"),
            Some(&"test-job".to_string())
        );

        // Verify spec
        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy, Some("Never".to_string()));

        // Verify init container
        let init_containers = spec.init_containers.unwrap();
        assert_eq!(init_containers.len(), 1);
        assert_eq!(init_containers[0].name, "workspace-init");
        assert_eq!(
            init_containers[0].image,
            Some("curlimages/curl:latest".to_string())
        );

        // Verify main container
        let containers = spec.containers;
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].name, "step");
        assert_eq!(containers[0].image, Some("python:3.12".to_string()));
        assert_eq!(containers[0].working_dir, Some("/workspace".to_string()));

        // Verify volumes
        let volumes = spec.volumes.unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].name, "workspace");
    }

    #[test]
    fn test_build_pod_spec_no_workspace() {
        let runner = KubeRunner::new(
            "stroem".to_string(),
            "http://stroem-server:8080".to_string(),
            "test-token".to_string(),
        );

        let config = RunConfig {
            cmd: Some("deploy --env production".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("company/deploy:v3".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
        };

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());

        let pod = runner.build_pod_spec("test-pod-nows", &config, "default", labels);

        let spec = pod.spec.unwrap();

        // No init containers in NoWorkspace mode
        assert!(
            spec.init_containers.is_none() || spec.init_containers.as_ref().unwrap().is_empty()
        );

        // No volumes
        assert!(spec.volumes.is_none() || spec.volumes.as_ref().unwrap().is_empty());

        // Main container uses the action image
        assert_eq!(containers_image(&spec.containers), "company/deploy:v3");

        // No workspace workdir
        assert!(spec.containers[0].working_dir.is_none());
    }

    #[test]
    fn test_build_pod_spec_no_workspace_with_entrypoint() {
        let runner = KubeRunner::new(
            "stroem".to_string(),
            "http://stroem-server:8080".to_string(),
            "test-token".to_string(),
        );

        let config = RunConfig {
            cmd: None,
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("company/tool:latest".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: Some(vec!["/app/run".to_string()]),
            command: Some(vec!["--verbose".to_string()]),
        };

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());

        let pod = runner.build_pod_spec("test-pod-ep", &config, "default", labels);

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];

        // Entrypoint maps to command, command maps to args
        assert_eq!(container.command, Some(vec!["/app/run".to_string()]));
        assert_eq!(container.args, Some(vec!["--verbose".to_string()]));
    }

    fn containers_image(containers: &[k8s_openapi::api::core::v1::Container]) -> &str {
        containers[0].image.as_deref().unwrap_or("")
    }

    /// Integration test: requires a Kubernetes cluster.
    /// Run with: cargo test -p stroem-runner --features kubernetes -- --ignored test_kube_echo
    #[tokio::test]
    #[ignore]
    async fn test_kube_echo() {
        let runner = KubeRunner::new(
            "default".to_string(),
            "http://localhost:8080".to_string(),
            "test-token".to_string(),
        );

        let mut env = HashMap::new();
        env.insert("STROEM_JOB_ID".to_string(), "test12345678".to_string());
        env.insert("STROEM_STEP_NAME".to_string(), "echo-test".to_string());
        env.insert("STROEM_WORKSPACE".to_string(), "default".to_string());

        let config = RunConfig {
            cmd: Some("echo hello-from-kube".to_string()),
            script: None,
            env,
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
        };

        let result = runner.execute(config, None).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello-from-kube"));
    }
}
