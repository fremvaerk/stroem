use crate::traits::{
    parse_output_line, LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner, RunnerMode,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures_util::io::AsyncBufReadExt;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, LogParams, PostParams};
use kube::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use stroem_common::constants::{DEFAULT_INIT_IMAGE, DEFAULT_RUNNER_IMAGE};

/// Kubernetes runner that executes commands inside Kubernetes pods
pub struct KubeRunner {
    namespace: String,
    server_url: String,
    worker_token: String,
    init_image: String,
    startup_configmap: Option<String>,
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
            init_image: DEFAULT_INIT_IMAGE.to_string(),
            startup_configmap: None,
        }
    }

    /// Set custom init container image (default: curlimages/curl:latest)
    pub fn with_init_image(mut self, image: String) -> Self {
        self.init_image = image;
        self
    }

    /// Set a ConfigMap containing startup scripts to mount at /etc/stroem/startup.d/
    pub fn with_startup_configmap(mut self, configmap: String) -> Self {
        self.startup_configmap = Some(configmap);
        self
    }

    /// Build the pod JSON for `RunnerMode::WithWorkspace` (Type 2: shell in pod with workspace).
    ///
    /// Returns raw `serde_json::Value` so the caller can apply the common tail logic
    /// (startup configmap injection, manifest overrides) before deserialising.
    fn build_pod_json_with_workspace(
        &self,
        pod_name: &str,
        config: &RunConfig,
        workspace_name: &str,
        labels: &HashMap<String, String>,
        image: &str,
        env: &[serde_json::Value],
    ) -> serde_json::Value {
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

        serde_json::json!({
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
                        "curl -sSf -H \"Authorization: Bearer $STROEM_WORKER_TOKEN\" '{}' | tar xz -C /workspace",
                        tarball_url
                    )],
                    "env": [{
                        "name": "STROEM_WORKER_TOKEN",
                        "value": self.worker_token,
                    }],
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
        })
    }

    /// Build the pod JSON for `RunnerMode::NoWorkspace` (Type 1: user's prepared image, no init
    /// container, no workspace volume).
    ///
    /// Returns raw `serde_json::Value` so the caller can apply the common tail logic
    /// (startup configmap injection, manifest overrides) before deserialising.
    fn build_pod_json_no_workspace(
        pod_name: &str,
        config: &RunConfig,
        labels: &HashMap<String, String>,
        image: &str,
        env: &[serde_json::Value],
    ) -> serde_json::Value {
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

        serde_json::json!({
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
        })
    }

    /// Convert the env map from `RunConfig` into the JSON array format expected by Kubernetes.
    fn build_env_array(env: &HashMap<String, String>) -> Vec<serde_json::Value> {
        env.iter()
            .map(|(k, v)| {
                serde_json::json!({
                    "name": k,
                    "value": v,
                })
            })
            .collect()
    }

    /// Inject the startup-scripts ConfigMap as a volume and mount it into the step container.
    ///
    /// Called only when `self.startup_configmap` is `Some`. Mutates `pod_json` in-place.
    fn inject_startup_configmap(pod_json: &mut serde_json::Value, cm_name: &str) {
        let startup_volume = serde_json::json!({
            "name": "startup-scripts",
            "configMap": {
                "name": cm_name,
                "defaultMode": 493  // 0755
            }
        });
        let startup_mount = serde_json::json!({
            "name": "startup-scripts",
            "mountPath": "/etc/stroem/startup.d",
            "readOnly": true
        });

        // Ensure volumes array exists and append
        if let Some(volumes) = pod_json["spec"]["volumes"].as_array_mut() {
            volumes.push(startup_volume);
        } else {
            pod_json["spec"]["volumes"] = serde_json::json!([startup_volume]);
        }

        // Add mount to step container
        if let Some(containers) = pod_json["spec"]["containers"].as_array_mut() {
            if let Some(step_container) = containers
                .iter_mut()
                .find(|c| c.get("name").and_then(|n| n.as_str()) == Some("step"))
            {
                if let Some(mounts) = step_container["volumeMounts"].as_array_mut() {
                    mounts.push(startup_mount);
                } else {
                    step_container["volumeMounts"] = serde_json::json!([startup_mount]);
                }
            }
        }
    }

    /// Build a pod spec for executing a step.
    ///
    /// Dispatches to [`build_pod_json_with_workspace`] or [`build_pod_json_no_workspace`]
    /// depending on `config.runner_mode`, then applies startup-script injection and
    /// pod manifest overrides as shared tail logic.
    fn build_pod_spec(
        &self,
        pod_name: &str,
        config: &RunConfig,
        workspace_name: &str,
        labels: HashMap<String, String>,
    ) -> Result<Pod> {
        let image = config
            .image
            .as_deref()
            .unwrap_or(DEFAULT_RUNNER_IMAGE)
            .to_string();

        let env = Self::build_env_array(&config.env);

        let mut pod_json = match config.runner_mode {
            RunnerMode::WithWorkspace => self.build_pod_json_with_workspace(
                pod_name,
                config,
                workspace_name,
                &labels,
                &image,
                &env,
            ),
            RunnerMode::NoWorkspace => {
                Self::build_pod_json_no_workspace(pod_name, config, &labels, &image, &env)
            }
        };

        // Inject startup scripts volume + mount if configured
        if let Some(ref cm_name) = self.startup_configmap {
            Self::inject_startup_configmap(&mut pod_json, cm_name);
        }

        // Validate overrides for dangerous fields before applying them
        if let Some(ref overrides) = config.pod_manifest_overrides {
            validate_pod_overrides(overrides).context("Pod manifest override validation failed")?;
            merge_json(&mut pod_json, overrides);
        }

        serde_json::from_value(pod_json)
            .context("Failed to build pod spec (manifest overrides may contain invalid fields)")
    }

    /// Sanitize a string for use in a Kubernetes DNS-1123 label:
    /// lowercase, replace non-ASCII-alphanumeric with dash, collapse consecutive dashes,
    /// trim leading/trailing dashes.
    fn sanitize_k8s_name(s: &str) -> String {
        let sanitized: String = s
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        // Collapse consecutive dashes (e.g. "my--task" → "my-task")
        let mut collapsed = String::with_capacity(sanitized.len());
        for c in sanitized.chars() {
            if c == '-' && collapsed.ends_with('-') {
                continue;
            }
            collapsed.push(c);
        }
        collapsed.trim_matches('-').to_string()
    }

    /// Generate a pod name from job_id, step_name, and task_name.
    /// Format: `stroem-{job_id_8chars}-{task}-{step}`, truncated to 63 chars.
    /// The job_id prefix is placed early to guarantee uniqueness even when
    /// long task/step names cause truncation.
    fn pod_name(job_id: &str, step_name: &str, task_name: &str) -> String {
        let job_short: String = job_id.chars().take(8).collect();
        let task_sanitized = Self::sanitize_k8s_name(task_name);
        let step_sanitized = Self::sanitize_k8s_name(step_name);

        // K8s name max 63 chars, must be DNS-1123 label
        let name = if task_sanitized.is_empty() {
            format!("stroem-{}-{}", job_short, step_sanitized)
        } else {
            format!("stroem-{}-{}-{}", job_short, task_sanitized, step_sanitized)
        };
        // Truncate to 63 chars, trim trailing dash if truncation left one
        let truncated: String = name.chars().take(63).collect();
        truncated.trim_end_matches('-').to_string()
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

/// Deep-merge two JSON values. For objects, keys are merged recursively.
/// For arrays of objects with "name" fields, elements are matched by name
/// and merged; unmatched entries are appended. Everything else is replaced.
fn merge_json(base: &mut Value, overrides: &Value) {
    match (base, overrides) {
        (Value::Object(base_map), Value::Object(over_map)) => {
            for (key, val) in over_map {
                let entry = base_map.entry(key.clone()).or_insert(Value::Null);
                merge_json(entry, val);
            }
        }
        (Value::Array(base_arr), Value::Array(over_arr))
            if is_named_array(base_arr) && is_named_array(over_arr) =>
        {
            for over_item in over_arr {
                if let Some(over_name) = over_item.get("name").and_then(|n| n.as_str()) {
                    if let Some(base_item) = base_arr
                        .iter_mut()
                        .find(|b| b.get("name").and_then(|n| n.as_str()) == Some(over_name))
                    {
                        merge_json(base_item, over_item);
                    } else {
                        base_arr.push(over_item.clone());
                    }
                }
            }
        }
        (base, overrides) => {
            *base = overrides.clone();
        }
    }
}

/// Returns true if every element in the array has a string "name" field.
fn is_named_array(arr: &[Value]) -> bool {
    !arr.is_empty()
        && arr
            .iter()
            .all(|item| item.get("name").and_then(|n| n.as_str()).is_some())
}

/// Validate pod manifest overrides for dangerous security fields before they are applied.
///
/// Rejects overrides that attempt to set:
/// - `spec.containers[*].securityContext.privileged: true`
/// - `spec.containers[*].securityContext.allowPrivilegeEscalation: true`
/// - `spec.initContainers[*].securityContext.privileged: true`
/// - `spec.initContainers[*].securityContext.allowPrivilegeEscalation: true`
/// - `spec.hostNetwork: true`
/// - `spec.hostPID: true`
/// - `spec.hostIPC: true`
/// - `spec.volumes[*].hostPath` (any value)
/// - `spec.containers[*].volumeMounts[*].mountPath: "/var/run/docker.sock"`
///
/// # Errors
///
/// Returns an error with a descriptive message for each rejected field.
///
/// # Examples
///
/// ```rust
/// # use serde_json::json;
/// # use stroem_runner::kubernetes::validate_pod_overrides;
/// // Safe override passes
/// let safe = json!({"spec": {"serviceAccountName": "my-sa"}});
/// assert!(validate_pod_overrides(&safe).is_ok());
///
/// // Privileged container is rejected
/// let dangerous = json!({"spec": {"containers": [{"name": "step", "securityContext": {"privileged": true}}]}});
/// assert!(validate_pod_overrides(&dangerous).is_err());
/// ```
pub fn validate_pod_overrides(overrides: &Value) -> Result<()> {
    // Null / non-object overrides are harmless
    let Some(spec) = overrides.get("spec") else {
        return Ok(());
    };

    // spec.hostNetwork
    if spec.get("hostNetwork").and_then(Value::as_bool) == Some(true) {
        bail!("Pod manifest override rejected: hostNetwork is not allowed");
    }

    // spec.hostPID
    if spec.get("hostPID").and_then(Value::as_bool) == Some(true) {
        bail!("Pod manifest override rejected: hostPID is not allowed");
    }

    // spec.hostIPC
    if spec.get("hostIPC").and_then(Value::as_bool) == Some(true) {
        bail!("Pod manifest override rejected: hostIPC is not allowed");
    }

    // spec.volumes[*].hostPath
    if let Some(volumes) = spec.get("volumes").and_then(Value::as_array) {
        for volume in volumes {
            if volume.get("hostPath").is_some() {
                bail!("Pod manifest override rejected: hostPath volumes are not allowed");
            }
        }
    }

    // Validate container security context and volume mounts for a named container list.
    let check_containers = |containers: &[Value], label: &str| -> Result<()> {
        for container in containers {
            // securityContext.privileged
            if container
                .get("securityContext")
                .and_then(|sc| sc.get("privileged"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                bail!(
                    "Pod manifest override rejected: privileged containers are not allowed ({})",
                    label
                );
            }

            // securityContext.allowPrivilegeEscalation
            if container
                .get("securityContext")
                .and_then(|sc| sc.get("allowPrivilegeEscalation"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                bail!(
                    "Pod manifest override rejected: allowPrivilegeEscalation is not allowed ({})",
                    label
                );
            }

            // volumeMounts[*].mountPath == "/var/run/docker.sock"
            if let Some(mounts) = container.get("volumeMounts").and_then(Value::as_array) {
                for mount in mounts {
                    if mount.get("mountPath").and_then(Value::as_str)
                        == Some("/var/run/docker.sock")
                    {
                        bail!(
                            "Pod manifest override rejected: mounting /var/run/docker.sock is not allowed ({})",
                            label
                        );
                    }
                }
            }
        }
        Ok(())
    };

    // spec.containers[*]
    if let Some(containers) = spec.get("containers").and_then(Value::as_array) {
        check_containers(containers, "containers")?;
    }

    // spec.initContainers[*]
    if let Some(init_containers) = spec.get("initContainers").and_then(Value::as_array) {
        check_containers(init_containers, "initContainers")?;
    }

    Ok(())
}

/// Result of classifying a Kubernetes pod phase.
enum PodPhaseStatus {
    /// Pod completed successfully (exit code 0).
    Succeeded,
    /// Pod failed. `exit_code` is extracted from container status, or defaults to 1.
    Failed { exit_code: i32 },
    /// The step container is running — ready for live log streaming.
    Running,
    /// Pod is still being scheduled / init containers running.
    Pending,
    /// A container is stuck in a waiting state with a terminal error (e.g. InvalidImageName).
    WaitingError { reason: String, message: String },
    /// An unrecognised phase string from the Kubernetes API.
    Unknown(String),
}

/// Waiting reasons that indicate a permanent failure (will never recover).
const TERMINAL_WAITING_REASONS: &[&str] = &[
    "InvalidImageName",
    "ErrImageNeverPull",
    "CreateContainerConfigError",
    "CreateContainerError",
];

/// Waiting reasons that indicate a transient failure. After enough retries
/// Kubernetes gives up and the pod stays Pending — treat as terminal.
const BACKOFF_WAITING_REASONS: &[&str] = &[
    "ImagePullBackOff",
    "ErrImagePull",
    "CrashLoopBackOff",
    "RegistryUnavailable",
];

/// Maximum time (seconds) to wait for a Pending pod with a backoff reason
/// before treating it as a permanent failure.
const BACKOFF_TIMEOUT_SECS: u64 = 300;

/// Classify a pod's phase and extract exit code from the step container status.
///
/// In addition to checking the pod phase, this inspects container waiting states
/// to detect image pull errors and other permanent failures that keep the pod in
/// `Pending` without ever transitioning to `Failed`.
fn classify_pod_status(pod: &Pod) -> Option<PodPhaseStatus> {
    let status = pod.status.as_ref()?;
    let phase = status.phase.as_deref()?;

    Some(match phase {
        "Succeeded" => PodPhaseStatus::Succeeded,
        "Failed" => {
            let mut exit_code = 1i32; // default for unresolvable failures
            if let Some(container_statuses) = &status.container_statuses {
                for cs in container_statuses {
                    if cs.name == "step" {
                        if let Some(terminated) =
                            cs.state.as_ref().and_then(|s| s.terminated.as_ref())
                        {
                            exit_code = terminated.exit_code;
                            if let Some(ref reason) = terminated.reason {
                                tracing::warn!("Step container terminated: reason={}", reason);
                            }
                        }
                    }
                }
            }
            PodPhaseStatus::Failed { exit_code }
        }
        "Running" => PodPhaseStatus::Running,
        "Pending" => {
            // Check both init_container_statuses and container_statuses for waiting errors.
            // Init containers are checked first since they run before the main containers.
            let all_statuses = status
                .init_container_statuses
                .iter()
                .flatten()
                .chain(status.container_statuses.iter().flatten());

            for cs in all_statuses {
                if let Some(waiting) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
                    let reason = waiting.reason.as_deref().unwrap_or("");
                    if TERMINAL_WAITING_REASONS.contains(&reason)
                        || BACKOFF_WAITING_REASONS.contains(&reason)
                    {
                        let message = waiting.message.clone().unwrap_or_else(|| {
                            format!("Container {} waiting: {}", cs.name, reason)
                        });
                        return Some(PodPhaseStatus::WaitingError {
                            reason: reason.to_string(),
                            message,
                        });
                    }
                }
            }
            PodPhaseStatus::Pending
        }
        other => PodPhaseStatus::Unknown(other.to_string()),
    })
}

/// Read log lines from an async stream, parse OUTPUT lines, and invoke the callback.
///
/// Note: The Kubernetes log API merges stdout and stderr into a single stream with no
/// per-line stream indicator. All lines are reported as Stdout — unlike DockerRunner
/// which can distinguish stderr via bollard's LogOutput type.
async fn drain_log_stream(
    stream: impl futures_util::io::AsyncRead + Unpin,
    log_callback: &Option<Arc<LogCallback>>,
) -> (Vec<String>, Vec<String>, Option<serde_json::Value>) {
    let mut stdout_lines = Vec::new();
    let stderr_lines = Vec::new();
    let mut parsed_output = None;

    let reader = futures_util::io::BufReader::new(stream);
    let mut lines_stream = reader.lines();
    while let Some(line_result) = futures_util::StreamExt::next(&mut lines_stream).await {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("Error reading log line: {:#}", e);
                break;
            }
        };

        if let Some(parsed) = parse_output_line(&line) {
            parsed_output = Some(parsed);
        }

        stdout_lines.push(line.clone());

        if let Some(ref cb) = log_callback {
            cb(LogLine {
                stream: LogStream::Stdout,
                line,
                timestamp: chrono::Utc::now(),
            });
        }
    }

    (stdout_lines, stderr_lines, parsed_output)
}

#[async_trait]
impl Runner for KubeRunner {
    async fn execute(
        &self,
        config: RunConfig,
        log_callback: Option<LogCallback>,
        cancel_token: CancellationToken,
    ) -> Result<RunResult> {
        let client = Client::try_default()
            .await
            .context("Failed to create Kubernetes client")?;

        let workspace_name = Self::workspace_from_config(&config);
        let job_id = config
            .env
            .get("STROEM_JOB_ID")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let task_name = config
            .env
            .get("STROEM_TASK_NAME")
            .cloned()
            .unwrap_or_default();
        let step_name = config
            .env
            .get("STROEM_STEP_NAME")
            .cloned()
            .unwrap_or_else(|| "step".to_string());

        let pod_name = Self::pod_name(&job_id, &step_name, &task_name);
        let task_label = Self::sanitize_k8s_name(&task_name);

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());
        labels.insert("stroem.io/job-id".to_string(), job_id.clone());
        labels.insert("stroem.io/step".to_string(), step_name.clone());
        if !task_label.is_empty() {
            labels.insert("stroem.io/task".to_string(), task_label);
        }

        let pod_spec = self
            .build_pod_spec(&pod_name, &config, &workspace_name, labels)
            .context("Failed to build pod spec")?;

        // Use namespace from pod spec (set via manifest override) or fall back to configured default
        let ns = pod_spec
            .metadata
            .namespace
            .as_deref()
            .unwrap_or(&self.namespace);
        let pods: Api<Pod> = Api::namespaced(client, ns);

        // Create the pod
        tracing::info!("Creating pod: {} in namespace: {}", pod_name, ns);
        pods.create(&PostParams::default(), &pod_spec)
            .await
            .context("Failed to create pod")?;

        // Phase 1: Wait for step container to start or pod to reach terminal state.
        // We need the container to be Running before we can start a follow log stream.
        let mut exit_code = -1i32;
        let mut pod_terminal = false;
        let mut backoff_first_seen: Option<std::time::Instant> = None;
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("Cancellation requested, deleting pod {}", pod_name);
                    if let Err(e) = pods.delete(&pod_name, &DeleteParams::default()).await {
                        tracing::warn!("Failed to delete pod {} on cancel: {:#}", pod_name, e);
                    }
                    return Ok(RunResult {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "Job cancelled".to_string(),
                        output: None,
                    });
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            }

            let pod = pods
                .get(&pod_name)
                .await
                .context("Failed to get pod status")?;

            match classify_pod_status(&pod) {
                Some(PodPhaseStatus::Succeeded) => {
                    exit_code = 0;
                    pod_terminal = true;
                    break;
                }
                Some(PodPhaseStatus::Failed { exit_code: code }) => {
                    exit_code = code;
                    pod_terminal = true;
                    break;
                }
                Some(PodPhaseStatus::Running) => {
                    tracing::debug!("Pod {} is Running, starting live log stream", pod_name);
                    break;
                }
                Some(PodPhaseStatus::WaitingError { reason, message }) => {
                    if TERMINAL_WAITING_REASONS.contains(&reason.as_str()) {
                        // Immediate terminal errors — no point waiting
                        tracing::error!(
                            "Pod {} has terminal container error: {} — {}",
                            pod_name,
                            reason,
                            message
                        );
                        if let Some(ref cb) = log_callback {
                            let _ = cb(LogLine {
                                stream: LogStream::Stderr,
                                line: format!("Container error: {} — {}", reason, message),
                                timestamp: chrono::Utc::now(),
                            });
                        }
                        exit_code = 1;
                        pod_terminal = true;
                        break;
                    }
                    // Backoff errors — wait up to BACKOFF_TIMEOUT_SECS
                    let first_seen = backoff_first_seen.get_or_insert_with(std::time::Instant::now);
                    let elapsed = first_seen.elapsed().as_secs();
                    if elapsed >= BACKOFF_TIMEOUT_SECS {
                        tracing::error!(
                            "Pod {} stuck in {} for {}s, giving up: {}",
                            pod_name,
                            reason,
                            elapsed,
                            message
                        );
                        if let Some(ref cb) = log_callback {
                            let _ = cb(LogLine {
                                stream: LogStream::Stderr,
                                line: format!(
                                    "Container error: {} after {}s — {}",
                                    reason, elapsed, message
                                ),
                                timestamp: chrono::Utc::now(),
                            });
                        }
                        exit_code = 1;
                        pod_terminal = true;
                        break;
                    }
                    tracing::warn!(
                        "Pod {} waiting: {} ({}s elapsed) — {}",
                        pod_name,
                        reason,
                        elapsed,
                        message
                    );
                }
                Some(PodPhaseStatus::Pending) => {
                    backoff_first_seen = None; // Reset if no longer in error state
                    tracing::debug!("Pod {} is Pending", pod_name);
                }
                Some(PodPhaseStatus::Unknown(ref phase)) => {
                    tracing::warn!("Pod {} in unexpected phase: {}", pod_name, phase);
                }
                None => {
                    tracing::debug!("Pod {} has no phase yet", pod_name);
                }
            }
        }

        // Phase 2: Stream logs — live (follow) if container is running, or post-mortem if already terminal.
        let log_callback = log_callback.map(Arc::new);
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();
        let mut parsed_output = None;

        if pod_terminal {
            // Pod reached terminal state before Running (e.g. init container failure).
            // Fetch logs without follow — same as previous behavior for this edge case.
            let log_params = LogParams {
                container: Some("step".to_string()),
                ..Default::default()
            };
            match pods.log_stream(&pod_name, &log_params).await {
                Ok(stream) => {
                    let (stdout, stderr, output) = drain_log_stream(stream, &log_callback).await;
                    stdout_lines = stdout;
                    stderr_lines = stderr;
                    parsed_output = output;
                }
                Err(e) => {
                    tracing::warn!("Failed to stream pod logs: {:#}", e);
                    stderr_lines.push(format!("Failed to retrieve logs: {:#}", e));
                }
            }
        } else {
            // Container is running — stream logs live while polling for terminal state.
            // The follow stream ends naturally when the container exits.
            let log_params = LogParams {
                follow: true,
                container: Some("step".to_string()),
                ..Default::default()
            };

            let pods_log = pods.clone();
            let pod_name_log = pod_name.clone();
            let log_cb = log_callback.clone();
            let log_task = tokio::spawn(async move {
                match pods_log.log_stream(&pod_name_log, &log_params).await {
                    Ok(stream) => drain_log_stream(stream, &log_cb).await,
                    Err(e) => {
                        tracing::warn!("Failed to stream pod logs: {:#}", e);
                        (
                            Vec::new(),
                            vec![format!("Failed to retrieve logs: {:#}", e)],
                            None,
                        )
                    }
                }
            });
            let log_abort = log_task.abort_handle();

            // Poll for terminal state concurrently while logs stream.
            // Use explicit match instead of ? to ensure cleanup on error.
            let poll_error: Option<anyhow::Error> = loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        tracing::info!("Cancellation requested, deleting pod {}", pod_name);
                        log_abort.abort();
                        if let Err(e) = pods.delete(&pod_name, &DeleteParams::default()).await {
                            tracing::warn!("Failed to delete pod {} on cancel: {:#}", pod_name, e);
                        }
                        return Ok(RunResult {
                            exit_code: -1,
                            stdout: String::new(),
                            stderr: "Job cancelled".to_string(),
                            output: None,
                        });
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }

                let pod = match pods.get(&pod_name).await {
                    Ok(p) => p,
                    Err(e) => {
                        log_abort.abort();
                        break Some(anyhow::Error::from(e).context("Failed to get pod status"));
                    }
                };

                match classify_pod_status(&pod) {
                    Some(PodPhaseStatus::Succeeded) => {
                        exit_code = 0;
                        break None;
                    }
                    Some(PodPhaseStatus::Failed { exit_code: code }) => {
                        exit_code = code;
                        break None;
                    }
                    Some(PodPhaseStatus::WaitingError { reason, message }) => {
                        tracing::error!(
                            "Pod {} has container error during log streaming: {} — {}",
                            pod_name,
                            reason,
                            message
                        );
                        exit_code = 1;
                        break None;
                    }
                    Some(PodPhaseStatus::Unknown(ref phase)) => {
                        tracing::warn!("Pod {} in unexpected phase: {}", pod_name, phase);
                    }
                    _ => {
                        tracing::debug!("Pod {} still running", pod_name);
                    }
                }
            };

            // Wait for log stream to finish draining (with timeout as safety net).
            match tokio::time::timeout(std::time::Duration::from_secs(10), log_task).await {
                Ok(Ok((stdout, stderr, output))) => {
                    stdout_lines = stdout;
                    stderr_lines = stderr;
                    parsed_output = output;
                }
                Ok(Err(e)) => {
                    tracing::warn!("Log streaming task failed: {:#}", e);
                }
                Err(_) => {
                    log_abort.abort();
                    tracing::warn!(
                        "Timed out waiting for log stream to drain for pod {}",
                        pod_name
                    );
                }
            }

            // Always clean up the pod before propagating any poll error.
            if let Err(e) = pods.delete(&pod_name, &DeleteParams::default()).await {
                tracing::warn!("Failed to delete pod {}: {:#}", pod_name, e);
            }

            if let Some(err) = poll_error {
                return Err(err);
            }

            return Ok(RunResult {
                exit_code,
                stdout: stdout_lines.join("\n"),
                stderr: stderr_lines.join("\n"),
                output: parsed_output,
            });
        }

        // Phase 3: Cleanup — delete pod and return result (post-mortem path only).
        if let Err(e) = pods.delete(&pod_name, &DeleteParams::default()).await {
            tracing::warn!("Failed to delete pod {}: {:#}", pod_name, e);
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
    fn test_pod_name_with_task() {
        // Format: stroem-{job_short}-{task}-{step}
        let name = KubeRunner::pod_name("abcdef12-3456-7890", "my-step-name", "deploy-api");
        assert_eq!(name, "stroem-abcdef12-deploy-api-my-step-name");
        assert!(name.len() <= 63);
    }

    #[test]
    fn test_pod_name_without_task() {
        // Format falls back to: stroem-{job_short}-{step}
        let name = KubeRunner::pod_name("abcdef12-3456-7890", "my-step-name", "");
        assert_eq!(name, "stroem-abcdef12-my-step-name");
        assert!(name.len() <= 63);
    }

    #[test]
    fn test_pod_name_sanitization() {
        let name = KubeRunner::pod_name("12345678", "Step With Spaces!", "My Task");
        assert_eq!(name, "stroem-12345678-my-task-step-with-spaces");
    }

    #[test]
    fn test_pod_name_truncation() {
        let long_step = "a".repeat(100);
        let name = KubeRunner::pod_name("12345678", &long_step, "my-task");
        assert!(name.len() <= 63);
        // job_short is placed early so it's never truncated
        assert!(name.starts_with("stroem-12345678-"));
    }

    #[test]
    fn test_pod_name_truncation_no_trailing_dash() {
        let step = format!("{}-x", "a".repeat(50));
        let name = KubeRunner::pod_name("12345678", &step, "task");
        assert!(name.len() <= 63);
        assert!(!name.ends_with('-'));
    }

    #[test]
    fn test_pod_name_truncation_preserves_job_short() {
        // Even with very long task + step, job_short is always present
        let long_task = "a".repeat(40);
        let long_step = "b".repeat(40);
        let name = KubeRunner::pod_name("abcd1234", &long_step, &long_task);
        assert!(name.len() <= 63);
        assert!(name.starts_with("stroem-abcd1234-"));
        assert!(!name.ends_with('-'));
    }

    #[test]
    fn test_pod_name_unicode_replaced_with_dash() {
        // Unicode chars like ë, â are not ASCII alphanumeric → replaced with dash
        let name = KubeRunner::pod_name("12345678", "step", "dëploy-tâsk");
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "Pod name contains non-ASCII characters: {}",
            name
        );
        // d-ploy-t-sk after sanitization (ë→-, â→-), then dash collapse
        assert_eq!(name, "stroem-12345678-d-ploy-t-sk-step");
    }

    #[test]
    fn test_pod_name_task_all_special_chars() {
        // Task that sanitizes entirely to empty → falls back to no-task format
        let name = KubeRunner::pod_name("12345678", "deploy", "!@#$");
        assert_eq!(name, "stroem-12345678-deploy");
    }

    #[test]
    fn test_pod_name_task_only_dashes() {
        // "---" → sanitize → trim → "" → no-task format
        let name = KubeRunner::pod_name("12345678", "build", "---");
        assert_eq!(name, "stroem-12345678-build");
    }

    #[test]
    fn test_pod_name_consecutive_dashes_collapsed() {
        // "my--task" after sanitization has consecutive dashes → collapsed
        let name = KubeRunner::pod_name("12345678", "step", "my--task");
        assert_eq!(name, "stroem-12345678-my-task-step");
    }

    #[test]
    fn test_pod_name_dot_separator_in_library_task() {
        // Library tasks use dot separator: "common.slack-notify" → "common-slack-notify"
        let name = KubeRunner::pod_name("12345678", "notify", "common.slack-notify");
        assert_eq!(name, "stroem-12345678-common-slack-notify-notify");
    }

    #[test]
    fn test_sanitize_k8s_name_basic() {
        assert_eq!(KubeRunner::sanitize_k8s_name("deploy-api"), "deploy-api");
        assert_eq!(KubeRunner::sanitize_k8s_name("My Task"), "my-task");
        assert_eq!(KubeRunner::sanitize_k8s_name(""), "");
    }

    #[test]
    fn test_sanitize_k8s_name_unicode() {
        // Non-ASCII alphanumeric chars are replaced with dash then trimmed
        assert_eq!(KubeRunner::sanitize_k8s_name("café"), "caf");
        assert_eq!(KubeRunner::sanitize_k8s_name("dëploy"), "d-ploy");
        assert_eq!(KubeRunner::sanitize_k8s_name("über-task"), "ber-task");
    }

    #[test]
    fn test_sanitize_k8s_name_collapses_dashes() {
        assert_eq!(KubeRunner::sanitize_k8s_name("a--b"), "a-b");
        assert_eq!(KubeRunner::sanitize_k8s_name("a!!!b"), "a-b");
        assert_eq!(KubeRunner::sanitize_k8s_name("---"), "");
    }

    #[test]
    fn test_sanitize_k8s_name_all_special() {
        assert_eq!(KubeRunner::sanitize_k8s_name("!@#$%"), "");
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
            pod_manifest_overrides: None,
        };

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());
        labels.insert("stroem.io/job-id".to_string(), "test-job".to_string());
        labels.insert("stroem.io/step".to_string(), "test-step".to_string());
        labels.insert("stroem.io/task".to_string(), "test-task".to_string());

        let pod = runner
            .build_pod_spec("test-pod", &config, "default", labels)
            .unwrap();

        // Verify metadata
        let metadata = pod.metadata;
        assert_eq!(metadata.name, Some("test-pod".to_string()));
        let labels = metadata.labels.unwrap();
        assert_eq!(labels.get("app"), Some(&"stroem-step".to_string()));
        assert_eq!(
            labels.get("stroem.io/job-id"),
            Some(&"test-job".to_string())
        );
        assert_eq!(labels.get("stroem.io/task"), Some(&"test-task".to_string()));

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
            pod_manifest_overrides: None,
        };

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());

        let pod = runner
            .build_pod_spec("test-pod-nows", &config, "default", labels)
            .unwrap();

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
            pod_manifest_overrides: None,
        };

        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());

        let pod = runner
            .build_pod_spec("test-pod-ep", &config, "default", labels)
            .unwrap();

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
            pod_manifest_overrides: None,
        };

        let result = runner
            .execute(config, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello-from-kube"));
    }

    #[test]
    fn test_worker_token_in_env_not_command() {
        let runner = KubeRunner::new(
            "stroem".to_string(),
            "http://stroem-server:8080".to_string(),
            "super-secret-token".to_string(),
        );

        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
        };

        let pod_json = runner.build_pod_json_with_workspace(
            "test-pod",
            &config,
            "default",
            &HashMap::new(),
            "alpine:latest",
            &[],
        );

        let init_container = &pod_json["spec"]["initContainers"][0];

        // Token must NOT appear in the command string
        let command = init_container["command"].to_string();
        assert!(
            !command.contains("super-secret-token"),
            "Worker token should not be embedded in command string, got: {}",
            command
        );
        // Command should reference the env var
        assert!(
            command.contains("$STROEM_WORKER_TOKEN"),
            "Command should reference $STROEM_WORKER_TOKEN env var"
        );

        // Token must be in the env array
        let env = init_container["env"]
            .as_array()
            .expect("env should be an array");
        let token_env = env
            .iter()
            .find(|e| e["name"] == "STROEM_WORKER_TOKEN")
            .expect("STROEM_WORKER_TOKEN env var should exist");
        assert_eq!(token_env["value"], "super-secret-token");
    }

    // --- merge_json tests ---

    #[test]
    fn test_merge_json_objects() {
        let mut base = serde_json::json!({"a": 1, "b": {"x": 10}});
        let over = serde_json::json!({"b": {"y": 20}, "c": 3});
        merge_json(&mut base, &over);
        assert_eq!(
            base,
            serde_json::json!({"a": 1, "b": {"x": 10, "y": 20}, "c": 3})
        );
    }

    #[test]
    fn test_merge_json_named_arrays() {
        let mut base = serde_json::json!([
            {"name": "a", "value": 1},
            {"name": "b", "value": 2}
        ]);
        let over = serde_json::json!([
            {"name": "b", "value": 99, "extra": true},
            {"name": "c", "value": 3}
        ]);
        merge_json(&mut base, &over);
        let arr = base.as_array().unwrap();
        assert_eq!(arr.len(), 3); // a, b (merged), c (appended)
        assert_eq!(arr[1]["value"], 99);
        assert_eq!(arr[1]["extra"], true);
        assert_eq!(arr[2]["name"], "c");
    }

    #[test]
    fn test_merge_json_replaces_non_named_arrays() {
        let mut base = serde_json::json!([1, 2, 3]);
        let over = serde_json::json!([4, 5]);
        merge_json(&mut base, &over);
        assert_eq!(base, serde_json::json!([4, 5]));
    }

    #[test]
    fn test_merge_json_scalars_replaced() {
        let mut base = serde_json::json!("old");
        let over = serde_json::json!("new");
        merge_json(&mut base, &over);
        assert_eq!(base, serde_json::json!("new"));
    }

    // --- build_pod_spec with overrides tests ---

    fn make_runner() -> KubeRunner {
        KubeRunner::new(
            "stroem".to_string(),
            "http://stroem-server:8080".to_string(),
            "test-token".to_string(),
        )
    }

    fn make_pod_config(overrides: Option<serde_json::Value>) -> RunConfig {
        RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: overrides,
        }
    }

    fn make_labels() -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert("app".to_string(), "stroem-step".to_string());
        labels
    }

    #[test]
    fn test_build_pod_spec_with_service_account() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "serviceAccountName": "my-sa"
            }
        })));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        assert_eq!(spec.service_account_name, Some("my-sa".to_string()));
        // Base fields preserved
        assert_eq!(spec.restart_policy, Some("Never".to_string()));
    }

    #[test]
    fn test_build_pod_spec_with_annotations() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "metadata": {
                "annotations": {
                    "iam.amazonaws.com/role": "my-role"
                }
            }
        })));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let annotations = pod.metadata.annotations.unwrap();
        assert_eq!(
            annotations.get("iam.amazonaws.com/role"),
            Some(&"my-role".to_string())
        );
        // Labels preserved
        assert_eq!(
            pod.metadata.labels.unwrap().get("app"),
            Some(&"stroem-step".to_string())
        );
    }

    #[test]
    fn test_build_pod_spec_with_node_selector() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "nodeSelector": {
                    "gpu": "true"
                }
            }
        })));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        let node_selector = spec.node_selector.unwrap();
        assert_eq!(node_selector.get("gpu"), Some(&"true".to_string()));
    }

    #[test]
    fn test_build_pod_spec_with_resource_limits() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "containers": [{
                    "name": "step",
                    "resources": {
                        "requests": {"memory": "256Mi", "cpu": "500m"},
                        "limits": {"memory": "512Mi"}
                    }
                }]
            }
        })));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert_eq!(container.name, "step");
        // Image should still be set from base
        assert_eq!(container.image, Some("alpine:latest".to_string()));
        // Resources should be merged
        let resources = container.resources.as_ref().unwrap();
        let requests = resources.requests.as_ref().unwrap();
        assert!(requests.contains_key("memory"));
        assert!(requests.contains_key("cpu"));
    }

    #[test]
    fn test_build_pod_spec_with_sidecar() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "containers": [{
                    "name": "sidecar",
                    "image": "envoyproxy/envoy:v1.28"
                }]
            }
        })));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        // Should have both step and sidecar containers
        assert_eq!(spec.containers.len(), 2);
        let names: Vec<&str> = spec.containers.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"step"));
        assert!(names.contains(&"sidecar"));
    }

    #[test]
    fn test_build_pod_spec_overrides_preserve_base() {
        let runner = make_runner();
        // WithWorkspace mode to test init container preservation
        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: Some(serde_json::json!({
                "spec": {
                    "serviceAccountName": "my-sa"
                }
            })),
        };
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        // Init container still exists
        let init_containers = spec.init_containers.unwrap();
        assert_eq!(init_containers.len(), 1);
        assert_eq!(init_containers[0].name, "workspace-init");
        // Labels still present
        assert!(pod.metadata.labels.unwrap().contains_key("app"));
        // Override applied
        assert_eq!(spec.service_account_name, Some("my-sa".to_string()));
        // restartPolicy preserved
        assert_eq!(spec.restart_policy, Some("Never".to_string()));
    }

    #[test]
    fn test_build_pod_spec_invalid_overrides_returns_error() {
        let runner = make_runner();
        // restartPolicy expects a string, not a number — Pod deserialization should fail
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "restartPolicy": 42
            }
        })));
        let result = runner.build_pod_spec("test-pod", &config, "default", make_labels());
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("manifest overrides"));
    }

    #[test]
    fn test_build_pod_spec_empty_overrides_noop() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({})));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy, Some("Never".to_string()));
        assert_eq!(spec.containers[0].name, "step");
    }

    #[test]
    fn test_build_pod_spec_with_tolerations() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "tolerations": [{
                    "key": "gpu",
                    "operator": "Exists",
                    "effect": "NoSchedule"
                }]
            }
        })));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        let tolerations = spec.tolerations.unwrap();
        assert_eq!(tolerations.len(), 1);
        assert_eq!(tolerations[0].key, Some("gpu".to_string()));
        assert_eq!(tolerations[0].operator, Some("Exists".to_string()));
        assert_eq!(tolerations[0].effect, Some("NoSchedule".to_string()));
    }

    #[test]
    fn test_build_pod_spec_none_overrides_noop() {
        let runner = make_runner();
        let config = make_pod_config(None);
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy, Some("Never".to_string()));
    }

    // --- startup configmap tests ---

    #[test]
    fn test_build_pod_spec_with_startup_configmap_no_workspace() {
        let runner = make_runner().with_startup_configmap("stroem-runner-startup".to_string());
        let config = make_pod_config(None);
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();

        // Should have startup-scripts volume
        let volumes = spec.volumes.unwrap();
        let startup_vol = volumes.iter().find(|v| v.name == "startup-scripts");
        assert!(startup_vol.is_some(), "Should have startup-scripts volume");
        let cm = startup_vol.unwrap().config_map.as_ref().unwrap();
        assert_eq!(cm.name, "stroem-runner-startup");
        assert_eq!(cm.default_mode, Some(493)); // 0755

        // Step container should have the mount
        let container = &spec.containers[0];
        let mounts = container.volume_mounts.as_ref().unwrap();
        let startup_mount = mounts.iter().find(|m| m.name == "startup-scripts");
        assert!(startup_mount.is_some(), "Should have startup-scripts mount");
        assert_eq!(startup_mount.unwrap().mount_path, "/etc/stroem/startup.d");
        assert_eq!(startup_mount.unwrap().read_only, Some(true));
    }

    #[test]
    fn test_build_pod_spec_with_startup_configmap_with_workspace() {
        let runner = make_runner().with_startup_configmap("stroem-runner-startup".to_string());
        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::WithWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
        };
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();

        // Should have both workspace and startup-scripts volumes
        let volumes = spec.volumes.unwrap();
        assert!(volumes.iter().any(|v| v.name == "workspace"));
        assert!(volumes.iter().any(|v| v.name == "startup-scripts"));

        // Step container should have both mounts
        let container = &spec.containers[0];
        let mounts = container.volume_mounts.as_ref().unwrap();
        assert!(mounts.iter().any(|m| m.name == "workspace"));
        assert!(mounts.iter().any(|m| m.name == "startup-scripts"));
    }

    #[test]
    fn test_build_pod_spec_startup_configmap_with_manifest_overrides() {
        // Verify that a manifest override with its own volumes and volumeMounts
        // does not displace the startup-scripts volume/mount injected before the merge.
        let runner = make_runner().with_startup_configmap("stroem-runner-startup".to_string());
        let config = RunConfig {
            cmd: Some("echo hello".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "/tmp".to_string(),
            action_type: "pod".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: Some(serde_json::json!({
                "spec": {
                    "serviceAccountName": "my-sa",
                    "volumes": [{
                        "name": "extra-secret",
                        "secret": { "secretName": "my-secret" }
                    }],
                    "containers": [{
                        "name": "step",
                        "volumeMounts": [{
                            "name": "extra-secret",
                            "mountPath": "/run/secrets"
                        }]
                    }]
                }
            })),
        };
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();

        // Both volumes must be present
        let volumes = spec.volumes.unwrap();
        assert!(
            volumes.iter().any(|v| v.name == "startup-scripts"),
            "startup-scripts volume was lost after manifest override merge"
        );
        assert!(
            volumes.iter().any(|v| v.name == "extra-secret"),
            "extra-secret volume from manifest override is missing"
        );

        // Both mounts must be on the step container
        let container = &spec.containers[0];
        let mounts = container.volume_mounts.as_ref().unwrap();
        assert!(
            mounts.iter().any(|m| m.name == "startup-scripts"),
            "startup-scripts mount was lost after manifest override merge"
        );
        assert!(
            mounts.iter().any(|m| m.name == "extra-secret"),
            "extra-secret mount from manifest override is missing"
        );

        // Non-volume overrides still applied
        assert_eq!(spec.service_account_name, Some("my-sa".to_string()));
    }

    #[test]
    fn test_build_pod_spec_without_startup_configmap() {
        let runner = make_runner(); // no startup configmap
        let config = make_pod_config(None);
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        let spec = pod.spec.unwrap();

        // No volumes in NoWorkspace mode without startup configmap
        assert!(spec.volumes.is_none() || spec.volumes.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_merge_json_nested_named_arrays() {
        // Merging volumeMounts within a container matched by name
        let mut base = serde_json::json!([{
            "name": "step",
            "volumeMounts": [
                {"name": "workspace", "mountPath": "/workspace"}
            ]
        }]);
        let over = serde_json::json!([{
            "name": "step",
            "volumeMounts": [
                {"name": "workspace", "readOnly": true},
                {"name": "secrets", "mountPath": "/secrets"}
            ]
        }]);
        merge_json(&mut base, &over);
        let container = &base.as_array().unwrap()[0];
        let mounts = container["volumeMounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 2);
        // workspace mount merged — has both mountPath and readOnly
        assert_eq!(mounts[0]["mountPath"], "/workspace");
        assert_eq!(mounts[0]["readOnly"], true);
        // secrets mount appended
        assert_eq!(mounts[1]["name"], "secrets");
    }

    #[test]
    fn test_merge_json_mixed_array_types() {
        // Base is named array, override is not — should replace entirely
        let mut base = serde_json::json!([{"name": "a", "value": 1}]);
        let over = serde_json::json!([1, 2, 3]);
        merge_json(&mut base, &over);
        assert_eq!(base, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn test_build_pod_spec_namespace_override() {
        let runner = make_runner(); // configured with "stroem" namespace
        let config = make_pod_config(Some(serde_json::json!({
            "metadata": {
                "namespace": "production"
            }
        })));
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        // Manifest override should set namespace on the pod metadata
        assert_eq!(pod.metadata.namespace.as_deref(), Some("production"),);
    }

    #[test]
    fn test_build_pod_spec_no_namespace_override() {
        let runner = make_runner();
        let config = make_pod_config(None);
        let pod = runner
            .build_pod_spec("test-pod", &config, "default", make_labels())
            .unwrap();
        // Without manifest override, namespace is not set on the pod
        // (Kubernetes API sets it from the URL namespace)
        assert!(pod.metadata.namespace.is_none());
    }

    // --- validate_pod_overrides tests ---

    #[test]
    fn test_validate_pod_overrides_empty_passes() {
        assert!(validate_pod_overrides(&serde_json::json!({})).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_null_passes() {
        assert!(validate_pod_overrides(&serde_json::Value::Null).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_no_spec_passes() {
        let v = serde_json::json!({"metadata": {"annotations": {"x": "y"}}});
        assert!(validate_pod_overrides(&v).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_safe_overrides_pass() {
        // resources, labels, annotations, service account, node selector, tolerations
        let v = serde_json::json!({
            "metadata": {
                "annotations": {"iam.amazonaws.com/role": "my-role"}
            },
            "spec": {
                "serviceAccountName": "my-sa",
                "nodeSelector": {"gpu": "true"},
                "tolerations": [{"key": "gpu", "operator": "Exists"}],
                "containers": [{
                    "name": "step",
                    "resources": {
                        "limits": {"memory": "512Mi"},
                        "requests": {"cpu": "250m"}
                    }
                }]
            }
        });
        assert!(validate_pod_overrides(&v).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_host_network_rejected() {
        let v = serde_json::json!({"spec": {"hostNetwork": true}});
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("hostNetwork"),
            "Error should mention hostNetwork: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_host_network_false_passes() {
        let v = serde_json::json!({"spec": {"hostNetwork": false}});
        assert!(validate_pod_overrides(&v).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_host_pid_rejected() {
        let v = serde_json::json!({"spec": {"hostPID": true}});
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("hostPID"),
            "Error should mention hostPID: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_host_ipc_rejected() {
        let v = serde_json::json!({"spec": {"hostIPC": true}});
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("hostIPC"),
            "Error should mention hostIPC: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_host_path_volume_rejected() {
        let v = serde_json::json!({
            "spec": {
                "volumes": [{"name": "host-vol", "hostPath": {"path": "/tmp"}}]
            }
        });
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("hostPath"),
            "Error should mention hostPath: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_empty_dir_volume_passes() {
        let v = serde_json::json!({
            "spec": {
                "volumes": [{"name": "tmp", "emptyDir": {}}]
            }
        });
        assert!(validate_pod_overrides(&v).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_secret_volume_passes() {
        let v = serde_json::json!({
            "spec": {
                "volumes": [{"name": "my-secret", "secret": {"secretName": "db-creds"}}]
            }
        });
        assert!(validate_pod_overrides(&v).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_privileged_container_rejected() {
        let v = serde_json::json!({
            "spec": {
                "containers": [{
                    "name": "step",
                    "securityContext": {"privileged": true}
                }]
            }
        });
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("privileged"),
            "Error should mention privileged: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_privileged_false_passes() {
        let v = serde_json::json!({
            "spec": {
                "containers": [{
                    "name": "step",
                    "securityContext": {"privileged": false}
                }]
            }
        });
        assert!(validate_pod_overrides(&v).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_allow_privilege_escalation_container_rejected() {
        let v = serde_json::json!({
            "spec": {
                "containers": [{
                    "name": "step",
                    "securityContext": {"allowPrivilegeEscalation": true}
                }]
            }
        });
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("allowPrivilegeEscalation"),
            "Error should mention allowPrivilegeEscalation: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_privileged_init_container_rejected() {
        let v = serde_json::json!({
            "spec": {
                "initContainers": [{
                    "name": "setup",
                    "securityContext": {"privileged": true}
                }]
            }
        });
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("privileged"),
            "Error should mention privileged: {}",
            err
        );
        assert!(
            err.to_string().contains("initContainers"),
            "Error should mention initContainers: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_allow_privilege_escalation_init_container_rejected() {
        let v = serde_json::json!({
            "spec": {
                "initContainers": [{
                    "name": "setup",
                    "securityContext": {"allowPrivilegeEscalation": true}
                }]
            }
        });
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("allowPrivilegeEscalation"),
            "Error should mention allowPrivilegeEscalation: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_docker_sock_mount_rejected() {
        let v = serde_json::json!({
            "spec": {
                "containers": [{
                    "name": "step",
                    "volumeMounts": [{
                        "name": "docker-sock",
                        "mountPath": "/var/run/docker.sock"
                    }]
                }]
            }
        });
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("/var/run/docker.sock"),
            "Error should mention the socket path: {}",
            err
        );
    }

    #[test]
    fn test_validate_pod_overrides_other_mount_path_passes() {
        let v = serde_json::json!({
            "spec": {
                "containers": [{
                    "name": "step",
                    "volumeMounts": [{
                        "name": "secrets",
                        "mountPath": "/run/secrets"
                    }]
                }]
            }
        });
        assert!(validate_pod_overrides(&v).is_ok());
    }

    #[test]
    fn test_validate_pod_overrides_multiple_containers_all_checked() {
        // Second container (sidecar) has privileged: true — should be caught
        let v = serde_json::json!({
            "spec": {
                "containers": [
                    {"name": "step", "securityContext": {"privileged": false}},
                    {"name": "sidecar", "securityContext": {"privileged": true}}
                ]
            }
        });
        let err = validate_pod_overrides(&v).unwrap_err();
        assert!(err.to_string().contains("privileged"));
    }

    #[test]
    fn test_build_pod_spec_privileged_override_rejected() {
        // Verify validate_pod_overrides is called inside build_pod_spec
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "containers": [{"name": "step", "securityContext": {"privileged": true}}]
            }
        })));
        let result = runner.build_pod_spec("test-pod", &config, "default", make_labels());
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("privileged"),
            "Expected 'privileged' in: {}",
            msg
        );
    }

    #[test]
    fn test_build_pod_spec_host_network_override_rejected() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {"hostNetwork": true}
        })));
        let result = runner.build_pod_spec("test-pod", &config, "default", make_labels());
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("hostNetwork"),
            "Expected 'hostNetwork' in: {}",
            msg
        );
    }

    #[test]
    fn test_build_pod_spec_host_path_override_rejected() {
        let runner = make_runner();
        let config = make_pod_config(Some(serde_json::json!({
            "spec": {
                "volumes": [{"name": "host", "hostPath": {"path": "/etc"}}]
            }
        })));
        let result = runner.build_pod_spec("test-pod", &config, "default", make_labels());
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("hostPath"), "Expected 'hostPath' in: {}", msg);
    }

    // --- classify_pod_status tests ---

    fn make_pod_with_phase(phase: &str) -> Pod {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "test" },
            "spec": { "containers": [{ "name": "step", "image": "alpine" }] },
            "status": { "phase": phase }
        }))
        .unwrap()
    }

    fn make_pod_failed_with_exit(exit_code: i32, reason: Option<&str>) -> Pod {
        let mut terminated = serde_json::json!({ "exitCode": exit_code });
        if let Some(r) = reason {
            terminated["reason"] = serde_json::json!(r);
        }
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "test" },
            "spec": { "containers": [{ "name": "step", "image": "alpine" }] },
            "status": {
                "phase": "Failed",
                "containerStatuses": [{
                    "name": "step",
                    "image": "alpine",
                    "imageID": "",
                    "ready": false,
                    "restartCount": 0,
                    "state": { "terminated": terminated }
                }]
            }
        }))
        .unwrap()
    }

    #[test]
    fn test_classify_succeeded() {
        let pod = make_pod_with_phase("Succeeded");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Succeeded)
        ));
    }

    #[test]
    fn test_classify_running() {
        let pod = make_pod_with_phase("Running");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Running)
        ));
    }

    #[test]
    fn test_classify_pending() {
        let pod = make_pod_with_phase("Pending");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Pending)
        ));
    }

    #[test]
    fn test_classify_unknown_phase() {
        let pod = make_pod_with_phase("Unknown");
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::Unknown(phase)) => assert_eq!(phase, "Unknown"),
            other => panic!("Expected Unknown, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_classify_novel_phase() {
        let pod = make_pod_with_phase("Terminating");
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::Unknown(phase)) => assert_eq!(phase, "Terminating"),
            other => panic!("Expected Unknown, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_classify_failed_with_exit_code() {
        let pod = make_pod_failed_with_exit(137, Some("OOMKilled"));
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::Failed { exit_code }) => assert_eq!(exit_code, 137),
            other => panic!("Expected Failed, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_classify_failed_no_container_status_defaults_to_1() {
        // Pod failed but no container statuses — e.g. scheduling failure
        let pod = make_pod_with_phase("Failed");
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::Failed { exit_code }) => assert_eq!(exit_code, 1),
            other => panic!("Expected Failed, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_classify_failed_exit_code_zero() {
        // Unusual: pod "Failed" but container exit code 0 (init container failure)
        let pod = make_pod_failed_with_exit(0, None);
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::Failed { exit_code }) => assert_eq!(exit_code, 0),
            other => panic!("Expected Failed, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn test_classify_no_status() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "test" },
            "spec": { "containers": [{ "name": "step", "image": "alpine" }] }
        }))
        .unwrap();
        assert!(classify_pod_status(&pod).is_none());
    }

    // --- drain_log_stream tests ---

    #[tokio::test]
    async fn test_drain_log_stream_normal_lines() {
        let data = b"line one\nline two\nline three\n";
        let cursor = futures_util::io::Cursor::new(data.to_vec());
        let (stdout, stderr, output) = drain_log_stream(cursor, &None).await;
        assert_eq!(stdout, vec!["line one", "line two", "line three"]);
        assert!(stderr.is_empty());
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn test_drain_log_stream_output_parsing() {
        let data = b"hello\nOUTPUT: {\"key\": \"value\"}\ndone\n";
        let cursor = futures_util::io::Cursor::new(data.to_vec());
        let (stdout, _stderr, output) = drain_log_stream(cursor, &None).await;
        assert_eq!(stdout.len(), 3);
        assert_eq!(output, Some(serde_json::json!({"key": "value"})));
    }

    #[tokio::test]
    async fn test_drain_log_stream_last_output_wins() {
        let data = b"OUTPUT: {\"a\": 1}\nOUTPUT: {\"b\": 2}\n";
        let cursor = futures_util::io::Cursor::new(data.to_vec());
        let (_stdout, _stderr, output) = drain_log_stream(cursor, &None).await;
        assert_eq!(output, Some(serde_json::json!({"b": 2})));
    }

    #[tokio::test]
    async fn test_drain_log_stream_empty() {
        let cursor = futures_util::io::Cursor::new(Vec::<u8>::new());
        let (stdout, stderr, output) = drain_log_stream(cursor, &None).await;
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert!(output.is_none());
    }

    #[tokio::test]
    async fn test_drain_log_stream_callback_invoked() {
        let data = b"first\nsecond\n";
        let cursor = futures_util::io::Cursor::new(data.to_vec());

        let lines_received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let lines_clone = lines_received.clone();
        let callback: Arc<LogCallback> = Arc::new(Box::new(move |log_line: LogLine| {
            lines_clone.lock().unwrap().push(log_line.line);
        }));
        let cb = Some(callback);

        let (stdout, _stderr, _output) = drain_log_stream(cursor, &cb).await;
        assert_eq!(stdout, vec!["first", "second"]);

        let received = lines_received.lock().unwrap();
        assert_eq!(*received, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn test_drain_log_stream_callback_gets_stdout_stream() {
        let data = b"hello\n";
        let cursor = futures_util::io::Cursor::new(data.to_vec());

        let streams = Arc::new(std::sync::Mutex::new(Vec::new()));
        let streams_clone = streams.clone();
        let callback: Arc<LogCallback> = Arc::new(Box::new(move |log_line: LogLine| {
            streams_clone.lock().unwrap().push(log_line.stream);
        }));
        let cb = Some(callback);

        drain_log_stream(cursor, &cb).await;

        let received = streams.lock().unwrap();
        assert_eq!(*received, vec![LogStream::Stdout]);
    }

    #[tokio::test]
    async fn test_drain_log_stream_invalid_output_json_ignored() {
        let data = b"OUTPUT: not-json\nreal line\n";
        let cursor = futures_util::io::Cursor::new(data.to_vec());
        let (stdout, _stderr, output) = drain_log_stream(cursor, &None).await;
        assert_eq!(stdout.len(), 2);
        assert!(output.is_none()); // invalid JSON is silently ignored
    }

    // --- classify_pod_status tests ---

    fn make_pod(phase: &str) -> Pod {
        use k8s_openapi::api::core::v1::PodStatus;
        Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                ..Default::default()
            }),
        }
    }

    fn make_pod_with_waiting(phase: &str, container_name: &str, reason: &str) -> Pod {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus,
        };
        Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    name: container_name.to_string(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some(reason.to_string()),
                            message: Some(format!("Error: {}", reason)),
                        }),
                        ..Default::default()
                    }),
                    image: String::new(),
                    image_id: String::new(),
                    ready: false,
                    restart_count: 0,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_classify_pod_status_succeeded() {
        let pod = make_pod("Succeeded");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Succeeded)
        ));
    }

    #[test]
    fn test_classify_pod_status_failed() {
        let pod = make_pod("Failed");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Failed { exit_code: 1 })
        ));
    }

    #[test]
    fn test_classify_pod_status_running() {
        let pod = make_pod("Running");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Running)
        ));
    }

    #[test]
    fn test_classify_pod_status_pending_no_waiting() {
        let pod = make_pod("Pending");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Pending)
        ));
    }

    #[test]
    fn test_classify_pod_status_pending_with_invalid_image_name() {
        let pod = make_pod_with_waiting("Pending", "step", "InvalidImageName");
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::WaitingError { reason, message }) => {
                assert_eq!(reason, "InvalidImageName");
                assert!(message.contains("InvalidImageName"));
            }
            other => panic!(
                "Expected WaitingError, got {:?}",
                other.map(|_| "something else")
            ),
        }
    }

    #[test]
    fn test_classify_pod_status_pending_with_image_pull_backoff() {
        let pod = make_pod_with_waiting("Pending", "step", "ImagePullBackOff");
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::WaitingError { reason, .. }) => {
                assert_eq!(reason, "ImagePullBackOff");
            }
            other => panic!(
                "Expected WaitingError, got {:?}",
                other.map(|_| "something else")
            ),
        }
    }

    #[test]
    fn test_classify_pod_status_pending_with_err_image_never_pull() {
        let pod = make_pod_with_waiting("Pending", "step", "ErrImageNeverPull");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::WaitingError { .. })
        ));
    }

    #[test]
    fn test_classify_pod_status_pending_with_create_container_error() {
        let pod = make_pod_with_waiting("Pending", "step", "CreateContainerConfigError");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::WaitingError { .. })
        ));
    }

    #[test]
    fn test_classify_pod_status_pending_with_normal_waiting() {
        // ContainerCreating is a normal waiting reason — should still be Pending
        let pod = make_pod_with_waiting("Pending", "step", "ContainerCreating");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Pending)
        ));
    }

    #[test]
    fn test_classify_pod_status_no_status() {
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: None,
        };
        assert!(classify_pod_status(&pod).is_none());
    }

    #[test]
    fn test_classify_pod_status_unknown_phase() {
        let pod = make_pod("SomeWeirdPhase");
        assert!(matches!(
            classify_pod_status(&pod),
            Some(PodPhaseStatus::Unknown(ref s)) if s == "SomeWeirdPhase"
        ));
    }

    /// Integration test: Kubernetes cancellation deletes the pod.
    /// Run with: cargo test -p stroem-runner --features kubernetes -- --ignored test_kube_cancellation
    #[tokio::test]
    #[ignore]
    async fn test_kube_cancellation() {
        let runner = KubeRunner::new(
            "default".to_string(),
            "http://localhost:8080".to_string(),
            "test-token".to_string(),
        );
        let config = RunConfig {
            cmd: Some("sleep 60".to_string()),
            script: None,
            env: HashMap::new(),
            workdir: "".to_string(),
            action_type: "pod".to_string(),
            image: Some("alpine:latest".to_string()),
            runner_mode: RunnerMode::NoWorkspace,
            runner_image: None,
            entrypoint: None,
            command: None,
            pod_manifest_overrides: None,
        };

        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel after 2s (K8s pods take longer to start)
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            token_clone.cancel();
        });

        let result = runner.execute(config, None, token).await.unwrap();
        assert_eq!(result.exit_code, -1);
        assert!(result.stderr.contains("Job cancelled"));
    }

    #[test]
    fn test_classify_pod_status_init_container_waiting_error() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus,
        };
        let pod = Pod {
            metadata: Default::default(),
            spec: None,
            status: Some(PodStatus {
                phase: Some("Pending".to_string()),
                init_container_statuses: Some(vec![ContainerStatus {
                    name: "workspace-init".to_string(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some("InvalidImageName".to_string()),
                            message: Some("bad init image".to_string()),
                        }),
                        ..Default::default()
                    }),
                    image: String::new(),
                    image_id: String::new(),
                    ready: false,
                    restart_count: 0,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        };
        match classify_pod_status(&pod) {
            Some(PodPhaseStatus::WaitingError { reason, message }) => {
                assert_eq!(reason, "InvalidImageName");
                assert_eq!(message, "bad init image");
            }
            other => panic!(
                "Expected WaitingError, got {:?}",
                other.map(|_| "something else")
            ),
        }
    }
}
