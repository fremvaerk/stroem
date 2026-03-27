use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::client::{ClaimedStep, ServerClient};

/// The maximum restart delay (seconds) regardless of consecutive failures.
const MAX_BACKOFF_SECS: u64 = 300;

/// Configuration parsed from a claimed event_source step's `action_spec`.
#[derive(Debug, Clone)]
struct EventSourceConfig {
    workspace: String,
    target_task: String,
    script: Option<String>,
    image: Option<String>,
    /// Runner type: "local", "docker", or "pod".
    runner: String,
    language: Option<String>,
    dependencies: Vec<String>,
    interpreter: Option<String>,
    env: HashMap<String, String>,
    /// Default input values, overridden by each stdout JSON payload.
    input_defaults: HashMap<String, serde_json::Value>,
    /// Restart policy string: "always", "on_failure", or "never".
    restart_policy: String,
    /// Base backoff delay in seconds (doubles on consecutive failure, capped at 300s).
    backoff_secs: u64,
    /// Maximum concurrent in-flight jobs before pausing stdout reading.
    /// Parsed from config; reserved for future throttling implementation.
    #[allow(dead_code)]
    max_in_flight: Option<u32>,
    /// Raw pod manifest overrides (pod runner only).
    manifest: Option<serde_json::Value>,
}

impl EventSourceConfig {
    fn from_step(step: &ClaimedStep) -> Result<Self> {
        let spec = step
            .action_spec
            .as_ref()
            .context("event_source step missing action_spec")?;

        let runner = spec
            .get("runner")
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string();

        let script = spec
            .get("script")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let image = spec
            .get("image")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let language = spec
            .get("language")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let interpreter = spec
            .get("interpreter")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let dependencies: Vec<String> = spec
            .get("dependencies")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let env: HashMap<String, String> = spec
            .get("env")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let input_defaults: HashMap<String, serde_json::Value> = spec
            .get("input_defaults")
            .and_then(|v| v.as_object())
            .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_else(|| {
                // Fall back to the step's own input field as defaults
                step.input
                    .as_ref()
                    .and_then(|v| v.as_object())
                    .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default()
            });

        let restart_policy = spec
            .get("restart_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("always")
            .to_string();

        let backoff_secs = spec
            .get("backoff_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(5);

        let max_in_flight = spec
            .get("max_in_flight")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);

        let target_task = spec
            .get("target_task")
            .and_then(|v| v.as_str())
            .context("event_source action_spec missing target_task")?
            .to_string();

        let manifest = spec.get("manifest").cloned();

        Ok(Self {
            workspace: step.workspace.clone(),
            target_task,
            script,
            image,
            runner,
            language,
            dependencies,
            interpreter,
            env,
            input_defaults,
            restart_policy,
            backoff_secs,
            max_in_flight,
            manifest,
        })
    }
}

/// Outcome of a single process lifecycle.
#[derive(Debug)]
enum ProcessOutcome {
    /// Process exited with code 0.
    ExitSuccess,
    /// Process exited with a non-zero exit code.
    ExitFailure(i32),
    /// The step was cancelled (global or job-level cancellation).
    Cancelled,
    /// The process failed to spawn or encountered a fatal setup error.
    SpawnError(String),
}

/// Returns true when the supervision loop should restart the process after an exit.
fn should_restart(policy: &str, success: bool) -> bool {
    match policy {
        "never" => false,
        "on_failure" => !success,
        _ => true, // "always" and any unknown policy default to always-restart
    }
}

/// Deep-merge two JSON objects. Values in `overrides` win over `base`.
/// Only operates on object values; non-objects are replaced wholesale.
fn merge_input(
    base: &HashMap<String, serde_json::Value>,
    overrides: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = serde_json::Map::new();
    for (k, v) in base {
        merged.insert(k.clone(), v.clone());
    }
    if let Some(obj) = overrides.as_object() {
        for (k, v) in obj {
            merged.insert(k.clone(), v.clone());
        }
    }
    serde_json::Value::Object(merged)
}

/// Read stdout lines from a reader, parse each as JSON, and emit to the server.
///
/// Non-JSON lines are logged as warnings and skipped. Returns when EOF is reached
/// or the cancel token is triggered.
async fn process_stdout_lines(
    reader: impl tokio::io::AsyncRead + Unpin,
    client: &ServerClient,
    config: &EventSourceConfig,
    cancel: &CancellationToken,
    source_id: &str,
) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = tokio::select! {
            result = lines.next_line() => {
                match result {
                    Ok(Some(line)) => line,
                    Ok(None) => return, // EOF
                    Err(e) => {
                        tracing::warn!("event_source stdout read error: {}", e);
                        return;
                    }
                }
            }
            _ = cancel.cancelled() => return,
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let payload: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "event_source malformed JSON on stdout (skipped): {}: {}",
                    e,
                    line
                );
                continue;
            }
        };

        // Merge defaults with the emitted payload (stdout data wins)
        let merged = merge_input(&config.input_defaults, &payload);

        tracing::debug!(
            source_id,
            target_task = %config.target_task,
            "event_source emitting job"
        );

        if let Err(e) = client
            .emit_event_source(&config.workspace, &config.target_task, merged, source_id)
            .await
        {
            tracing::error!("event_source emit failed: {:#}", e);
        }
    }
}

/// Run the event source process using the local (shell) runner.
async fn run_local_process(
    config: &EventSourceConfig,
    client: &ServerClient,
    cancel: &CancellationToken,
    source_id: &str,
) -> ProcessOutcome {
    use stroem_common::language::ScriptLanguage;
    use stroem_runner::script_exec;

    let Some(ref script_content) = config.script else {
        return ProcessOutcome::SpawnError(
            "event_source with runner=local requires a script field".to_string(),
        );
    };

    let temp_dir: tempfile::TempDir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return ProcessOutcome::SpawnError(format!("Failed to create temp dir: {:#}", e)),
    };

    let lang = ScriptLanguage::from_str_opt(config.language.as_deref());

    let script_path = match script_exec::write_temp_script(temp_dir.path(), script_content, lang) {
        Ok(p) => p,
        Err(e) => {
            return ProcessOutcome::SpawnError(format!("Failed to write temp script: {:#}", e))
        }
    };

    let (binary, args) = match script_exec::build_script_command(
        lang,
        &script_path,
        &config.dependencies,
        config.interpreter.as_deref(),
        &[], // no CLI args for event sources
    ) {
        Ok(cmd) => cmd,
        Err(e) => {
            return ProcessOutcome::SpawnError(format!("Failed to build script command: {:#}", e))
        }
    };

    // Install dependencies if needed (blocking-style, before spawning)
    if !config.dependencies.is_empty() {
        if let Some(install_cmd) =
            script_exec::build_dep_install_prefix(lang, &binary, &config.dependencies)
        {
            let status = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&install_cmd)
                .current_dir(temp_dir.path())
                .envs(&config.env)
                .status()
                .await;

            match status {
                Ok(s) if !s.success() => {
                    return ProcessOutcome::SpawnError(format!(
                        "Dependency installation failed (exit {}): {}",
                        s.code().unwrap_or(-1),
                        install_cmd
                    ));
                }
                Err(e) => {
                    return ProcessOutcome::SpawnError(format!(
                        "Failed to run dep install '{}': {:#}",
                        install_cmd, e
                    ));
                }
                _ => {}
            }
        }
    }

    let mut child = match tokio::process::Command::new(&binary)
        .args(&args)
        .envs(&config.env)
        .current_dir(temp_dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ProcessOutcome::SpawnError(format!("Failed to spawn process: {:#}", e)),
    };

    let stdout = child.stdout.take().expect("stdout must be piped");
    let stderr = child.stderr.take().expect("stderr must be piped");

    // Stream stderr to tracing in the background
    let stderr_cancel = cancel.clone();
    let stderr_handle = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            let line = tokio::select! {
                result = lines.next_line() => match result {
                    Ok(Some(l)) => l,
                    _ => break,
                },
                _ = stderr_cancel.cancelled() => break,
            };
            tracing::debug!("event_source stderr: {}", line);
        }
    });

    // Process stdout: parse JSON, emit events
    process_stdout_lines(stdout, client, config, cancel, source_id).await;

    // Wait for the child process to exit (or for cancellation)
    let outcome = tokio::select! {
        result = child.wait() => {
            match result {
                Ok(status) => {
                    if status.success() {
                        ProcessOutcome::ExitSuccess
                    } else {
                        ProcessOutcome::ExitFailure(status.code().unwrap_or(-1))
                    }
                }
                Err(e) => ProcessOutcome::SpawnError(format!("Error waiting for child: {:#}", e)),
            }
        }
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            ProcessOutcome::Cancelled
        }
    };

    stderr_handle.abort();
    outcome
}

/// Run the event source process using Docker.
#[cfg(feature = "docker")]
async fn run_docker_process(
    config: &EventSourceConfig,
    client: &ServerClient,
    cancel: &CancellationToken,
    source_id: &str,
) -> ProcessOutcome {
    use bollard::container::LogOutput;
    use bollard::models::ContainerCreateBody;
    use bollard::query_parameters::{
        AttachContainerOptions, CreateContainerOptions, CreateImageOptions, RemoveContainerOptions,
        StartContainerOptions,
    };
    use bollard::Docker;
    use futures_util::StreamExt;
    use stroem_common::constants::DEFAULT_RUNNER_IMAGE;
    use stroem_common::language::ScriptLanguage;
    use stroem_runner::script_exec;

    let docker = match Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            return ProcessOutcome::SpawnError(format!("Failed to connect to Docker: {:#}", e))
        }
    };

    let image = config
        .image
        .as_deref()
        .unwrap_or(DEFAULT_RUNNER_IMAGE)
        .to_string();

    // Pull image
    tracing::info!("event_source pulling image: {}", image);
    let mut pull_stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: Some(image.clone()),
            ..Default::default()
        }),
        None,
        None,
    );
    while let Some(result) = pull_stream.next().await {
        if let Err(e) = result {
            tracing::warn!("event_source image pull warning: {:#}", e);
        }
    }

    // Build container cmd — if script content is provided, inline it; otherwise use image defaults
    let lang = ScriptLanguage::from_str_opt(config.language.as_deref());
    let cmd: Option<Vec<String>> = config.script.as_ref().map(|inline| {
        script_exec::build_container_script_cmd(
            inline,
            lang,
            &config.dependencies,
            config.interpreter.as_deref(),
            &[], // no CLI args for event sources
        )
    });

    let env_vec: Vec<String> = config.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let container_config = ContainerCreateBody {
        image: Some(image.clone()),
        cmd,
        env: Some(env_vec),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        ..Default::default()
    };

    let container = match docker
        .create_container(None::<CreateContainerOptions>, container_config)
        .await
        .context("Failed to create container")
    {
        Ok(c) => c,
        Err(e) => return ProcessOutcome::SpawnError(format!("{:#}", e)),
    };
    let container_id = container.id.clone();
    tracing::info!(
        "event_source created container: {}",
        &container_id[..12.min(container_id.len())]
    );

    // Helper: stop and force-remove container
    let stop_remove = {
        let docker = docker.clone();
        let id = container_id.clone();
        move || {
            let docker = docker.clone();
            let id = id.clone();
            async move {
                let _ = docker.stop_container(&id, None).await;
                let _ = docker
                    .remove_container(
                        &id,
                        Some(RemoveContainerOptions {
                            force: true,
                            ..Default::default()
                        }),
                    )
                    .await;
            }
        }
    };

    if let Err(e) = docker
        .start_container(&container_id, None::<StartContainerOptions>)
        .await
    {
        stop_remove().await;
        return ProcessOutcome::SpawnError(format!("Failed to start container: {:#}", e));
    }

    let attach_opts = AttachContainerOptions {
        stdout: true,
        stderr: true,
        stream: true,
        logs: true,
        ..Default::default()
    };

    let mut output = match docker
        .attach_container(&container_id, Some(attach_opts))
        .await
        .context("Failed to attach to container")
    {
        Ok(o) => o,
        Err(e) => {
            stop_remove().await;
            return ProcessOutcome::SpawnError(format!("{:#}", e));
        }
    };

    // Read container output. We process stdout JSON lines; stderr goes to tracing.
    loop {
        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                stop_remove().await;
                return ProcessOutcome::Cancelled;
            }

            chunk = output.output.next() => {
                let Some(chunk) = chunk else { break };
                match chunk {
                    Ok(log_output) => {
                        let line = log_output.to_string();
                        let line = line.trim_end_matches('\n');
                        if line.is_empty() {
                            continue;
                        }
                        match log_output {
                            LogOutput::StdErr { .. } => {
                                tracing::debug!("event_source docker stderr: {}", line);
                            }
                            _ => {
                                // Stdout: attempt to parse and emit
                                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(line) {
                                    let merged = merge_input(&config.input_defaults, &payload);
                                    if let Err(e) = client
                                        .emit_event_source(
                                            &config.workspace,
                                            &config.target_task,
                                            merged,
                                            source_id,
                                        )
                                        .await
                                    {
                                        tracing::error!("event_source emit failed: {:#}", e);
                                    }
                                } else if !line.trim().is_empty() {
                                    tracing::warn!(
                                        "event_source malformed JSON on stdout (skipped): {}",
                                        line
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("event_source container output error: {:#}", e);
                        break;
                    }
                }
            }
        }
    }

    // Wait for container exit to get exit code
    use bollard::query_parameters::WaitContainerOptions;
    let mut exit_code: i32 = -1;
    let mut wait_stream = docker.wait_container(&container_id, None::<WaitContainerOptions>);
    loop {
        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                stop_remove().await;
                return ProcessOutcome::Cancelled;
            }

            result = wait_stream.next() => {
                let Some(result) = result else { break };
                match result {
                    Ok(response) => {
                        exit_code = response.status_code as i32;
                    }
                    Err(e) => {
                        tracing::warn!("event_source wait_container error: {:#}", e);
                        break;
                    }
                }
            }
        }
    }

    stop_remove().await;

    if exit_code == 0 {
        ProcessOutcome::ExitSuccess
    } else {
        ProcessOutcome::ExitFailure(exit_code)
    }
}

/// Run the event source process as a Kubernetes pod.
#[cfg(feature = "kubernetes")]
async fn run_pod_process(
    config: &EventSourceConfig,
    client: &ServerClient,
    cancel: &CancellationToken,
    source_id: &str,
    namespace: &str,
) -> ProcessOutcome {
    use futures_util::io::AsyncBufReadExt as _;
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, DeleteParams, LogParams, PostParams};
    use kube::Client;
    use std::collections::HashMap;
    use stroem_common::constants::DEFAULT_RUNNER_IMAGE;
    use stroem_common::language::ScriptLanguage;
    use stroem_runner::script_exec;

    let kube_client = match Client::try_default()
        .await
        .context("Failed to create Kubernetes client")
    {
        Ok(c) => c,
        Err(e) => return ProcessOutcome::SpawnError(format!("{:#}", e)),
    };

    let pods: Api<Pod> = Api::namespaced(kube_client, namespace);

    let image = config
        .image
        .as_deref()
        .unwrap_or(DEFAULT_RUNNER_IMAGE)
        .to_string();

    let lang = ScriptLanguage::from_str_opt(config.language.as_deref());
    let env_array: Vec<serde_json::Value> = config
        .env
        .iter()
        .map(|(k, v)| serde_json::json!({ "name": k, "value": v }))
        .collect();

    // Build command (NoWorkspace style — no init container, no workspace volume)
    let container_cmd: Option<Vec<String>> = config.script.as_ref().map(|inline| {
        script_exec::build_container_script_cmd(
            inline,
            lang,
            &config.dependencies,
            config.interpreter.as_deref(),
            &[],
        )
    });

    // Unique pod name derived from the source_id
    let sanitized: String = source_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let pod_name = format!(
        "stroem-es-{}",
        sanitized
            .trim_matches('-')
            .chars()
            .take(50)
            .collect::<String>()
    );

    let mut labels: HashMap<String, String> = HashMap::new();
    labels.insert("app".to_string(), "stroem-event-source".to_string());
    labels.insert("stroem.io/source-id".to_string(), source_id.to_string());

    let mut container_json = serde_json::json!({
        "name": "event-source",
        "image": image,
        "env": env_array,
    });

    if let Some(cmd) = container_cmd {
        container_json["command"] = serde_json::json!(cmd);
    }

    let mut pod_json = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name,
            "labels": labels,
        },
        "spec": {
            "restartPolicy": "Never",
            "containers": [container_json],
        },
    });

    // Apply user-provided manifest overrides
    if let Some(ref overrides) = config.manifest {
        fn merge_json(base: &mut serde_json::Value, overrides: &serde_json::Value) {
            match (base, overrides) {
                (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
                    for (k, v) in o {
                        let entry = b.entry(k.clone()).or_insert(serde_json::Value::Null);
                        merge_json(entry, v);
                    }
                }
                (base, overrides) => *base = overrides.clone(),
            }
        }
        merge_json(&mut pod_json, overrides);
    }

    let pod_spec: Pod = match serde_json::from_value(pod_json).context("Failed to build pod spec") {
        Ok(p) => p,
        Err(e) => return ProcessOutcome::SpawnError(format!("{:#}", e)),
    };

    tracing::info!(
        "event_source creating pod: {} in namespace: {}",
        pod_name,
        namespace
    );
    if let Err(e) = pods.create(&PostParams::default(), &pod_spec).await {
        return ProcessOutcome::SpawnError(format!("Failed to create pod: {:#}", e));
    }

    // Wait for pod to reach Running or terminal state
    let mut exit_code: i32 = -1;
    let mut pod_terminal = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
                return ProcessOutcome::Cancelled;
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }

        let pod = match pods.get(&pod_name).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("event_source failed to get pod status: {:#}", e);
                continue;
            }
        };

        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");

        match phase {
            "Succeeded" => {
                exit_code = 0;
                pod_terminal = true;
                break;
            }
            "Failed" => {
                exit_code = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|cs| cs.first())
                    .and_then(|cs| cs.state.as_ref())
                    .and_then(|st| st.terminated.as_ref())
                    .map(|t| t.exit_code)
                    .unwrap_or(1);
                pod_terminal = true;
                break;
            }
            "Running" => break,
            _ => {
                tracing::debug!("event_source pod {} phase: {}", pod_name, phase);
            }
        }
    }

    if pod_terminal {
        // Pod already finished — delete and return without streaming
        let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
        if exit_code == 0 {
            return ProcessOutcome::ExitSuccess;
        }
        return ProcessOutcome::ExitFailure(exit_code);
    }

    // Stream logs with follow=true. Process stdout lines and emit events.
    let log_params = LogParams {
        follow: true,
        container: Some("event-source".to_string()),
        ..Default::default()
    };

    let stream_result = tokio::select! {
        r = pods.log_stream(&pod_name, &log_params) => r,
        _ = cancel.cancelled() => {
            let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
            return ProcessOutcome::Cancelled;
        }
    };

    match stream_result {
        Ok(stream) => {
            let reader = futures_util::io::BufReader::new(stream);
            let mut lines = reader.lines();

            loop {
                let line_result = tokio::select! {
                    biased;

                    _ = cancel.cancelled() => {
                        let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
                        return ProcessOutcome::Cancelled;
                    }

                    result = futures_util::StreamExt::next(&mut lines) => result,
                };

                match line_result {
                    Some(Ok(line)) if !line.trim().is_empty() => {
                        match serde_json::from_str::<serde_json::Value>(&line) {
                            Ok(payload) => {
                                let merged = merge_input(&config.input_defaults, &payload);
                                if let Err(e) = client
                                    .emit_event_source(
                                        &config.workspace,
                                        &config.target_task,
                                        merged,
                                        source_id,
                                    )
                                    .await
                                {
                                    tracing::error!("event_source emit failed: {:#}", e);
                                }
                            }
                            Err(_) => {
                                tracing::warn!(
                                    "event_source pod malformed JSON (skipped): {}",
                                    line
                                );
                            }
                        }
                    }
                    Some(Ok(_)) => {} // empty line
                    Some(Err(e)) => {
                        tracing::warn!("event_source pod log stream error: {:#}", e);
                        break;
                    }
                    None => break, // EOF
                }
            }
        }
        Err(e) => {
            tracing::warn!("event_source failed to open log stream: {:#}", e);
        }
    }

    // Poll for final pod phase after log stream ends
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
                return ProcessOutcome::Cancelled;
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }

        let pod = match pods.get(&pod_name).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("event_source pod status error: {:#}", e);
                break;
            }
        };

        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");

        match phase {
            "Succeeded" => {
                exit_code = 0;
                break;
            }
            "Failed" => {
                exit_code = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|cs| cs.first())
                    .and_then(|cs| cs.state.as_ref())
                    .and_then(|st| st.terminated.as_ref())
                    .map(|t| t.exit_code)
                    .unwrap_or(1);
                break;
            }
            _ => {
                tracing::debug!("event_source pod {} still in phase: {}", pod_name, phase);
            }
        }
    }

    let _ = pods.delete(&pod_name, &DeleteParams::default()).await;

    if exit_code == 0 {
        ProcessOutcome::ExitSuccess
    } else {
        ProcessOutcome::ExitFailure(exit_code)
    }
}

/// Execute one lifecycle of the event source process (single run, no restart logic).
async fn run_process(
    config: &EventSourceConfig,
    client: &ServerClient,
    cancel: &CancellationToken,
    source_id: &str,
    #[cfg(feature = "kubernetes")] kube_namespace: Option<&str>,
) -> ProcessOutcome {
    match config.runner.as_str() {
        "docker" => {
            #[cfg(feature = "docker")]
            return run_docker_process(config, client, cancel, source_id).await;
            #[cfg(not(feature = "docker"))]
            return ProcessOutcome::SpawnError(
                "docker runner not available (feature not enabled)".to_string(),
            );
        }
        "pod" => {
            #[cfg(feature = "kubernetes")]
            {
                let ns = kube_namespace.unwrap_or("default");
                return run_pod_process(config, client, cancel, source_id, ns).await;
            }
            #[cfg(not(feature = "kubernetes"))]
            return ProcessOutcome::SpawnError(
                "pod runner not available (feature not enabled)".to_string(),
            );
        }
        _ => {
            // "local" or any unrecognised runner defaults to local
            run_local_process(config, client, cancel, source_id).await
        }
    }
}

/// Drive the full lifecycle of an event source step.
///
/// Supervises a long-running process, restarting it according to the configured
/// restart policy with exponential backoff. Returns only when the step is cancelled
/// (via `cancel_token`) or the restart policy says to stop.
///
/// Event source steps never report completion to the server — they remain in
/// `running` status until the worker dies or the job is cancelled.
#[tracing::instrument(skip(client, step, cancel_token), fields(
    job_id = %step.job_id,
    step_name = %step.step_name,
    workspace = %step.workspace,
))]
pub async fn execute_event_source(
    client: &ServerClient,
    step: ClaimedStep,
    cancel_token: CancellationToken,
    #[cfg(feature = "kubernetes")] kube_namespace: Option<String>,
) {
    let config = match EventSourceConfig::from_step(&step) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to parse event_source config: {:#}", e);
            return;
        }
    };

    // Use job_id + step_name as the stable source identity sent to the emit endpoint
    let source_id = format!("{}/{}", step.job_id, step.step_name);

    tracing::info!(
        "Starting event_source '{}' targeting task '{}' via runner '{}'",
        step.step_name,
        config.target_task,
        config.runner,
    );

    let mut consecutive_failures: u32 = 0;

    loop {
        if cancel_token.is_cancelled() {
            tracing::info!(
                "event_source '{}' cancelled before process start",
                step.step_name
            );
            return;
        }

        tracing::debug!("event_source '{}' starting process run", step.step_name);

        let outcome = run_process(
            &config,
            client,
            &cancel_token,
            &source_id,
            #[cfg(feature = "kubernetes")]
            kube_namespace.as_deref(),
        )
        .await;

        match outcome {
            ProcessOutcome::Cancelled => {
                tracing::info!("event_source '{}' cancelled", step.step_name);
                return;
            }
            ProcessOutcome::ExitSuccess => {
                tracing::info!(
                    "event_source '{}' process exited successfully",
                    step.step_name
                );
                consecutive_failures = 0;
                if !should_restart(&config.restart_policy, true) {
                    tracing::info!(
                        "event_source '{}' restart_policy='{}' — not restarting after success",
                        step.step_name,
                        config.restart_policy,
                    );
                    return;
                }
            }
            ProcessOutcome::ExitFailure(code) => {
                tracing::warn!(
                    "event_source '{}' process exited with code {}",
                    step.step_name,
                    code
                );
                consecutive_failures = consecutive_failures.saturating_add(1);
                if !should_restart(&config.restart_policy, false) {
                    tracing::info!(
                        "event_source '{}' restart_policy='{}' — not restarting after failure",
                        step.step_name,
                        config.restart_policy,
                    );
                    return;
                }
            }
            ProcessOutcome::SpawnError(ref msg) => {
                tracing::error!(
                    "event_source '{}' spawn/setup error: {}",
                    step.step_name,
                    msg
                );
                consecutive_failures = consecutive_failures.saturating_add(1);
                if !should_restart(&config.restart_policy, false) {
                    tracing::info!(
                        "event_source '{}' restart_policy='{}' — not restarting after spawn error",
                        step.step_name,
                        config.restart_policy,
                    );
                    return;
                }
            }
        }

        // Exponential backoff: base * 2^(failures-1), capped at MAX_BACKOFF_SECS
        let delay = if consecutive_failures == 0 {
            0
        } else {
            let exp = consecutive_failures.saturating_sub(1).min(10);
            config
                .backoff_secs
                .saturating_mul(2u64.saturating_pow(exp))
                .min(MAX_BACKOFF_SECS)
        };

        if delay > 0 {
            tracing::info!(
                "event_source '{}' backing off {}s before restart (failure #{})",
                step.step_name,
                delay,
                consecutive_failures,
            );
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(delay)) => {}
                () = cancel_token.cancelled() => {
                    tracing::info!("event_source '{}' cancelled during backoff", step.step_name);
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_should_restart_always() {
        assert!(should_restart("always", true));
        assert!(should_restart("always", false));
    }

    #[test]
    fn test_should_restart_on_failure() {
        assert!(!should_restart("on_failure", true));
        assert!(should_restart("on_failure", false));
    }

    #[test]
    fn test_should_restart_never() {
        assert!(!should_restart("never", true));
        assert!(!should_restart("never", false));
    }

    #[test]
    fn test_should_restart_unknown_defaults_to_always() {
        assert!(should_restart("unknown_policy", true));
        assert!(should_restart("unknown_policy", false));
    }

    #[test]
    fn test_merge_input_overrides_base() {
        let mut base = HashMap::new();
        base.insert("key1".to_string(), json!("base_value"));
        base.insert("key2".to_string(), json!(42));

        let overrides = json!({ "key1": "override_value", "key3": true });
        let merged = merge_input(&base, &overrides);

        assert_eq!(merged["key1"], json!("override_value"));
        assert_eq!(merged["key2"], json!(42));
        assert_eq!(merged["key3"], json!(true));
    }

    #[test]
    fn test_merge_input_non_object_override_ignored() {
        let mut base = HashMap::new();
        base.insert("key1".to_string(), json!("base_value"));

        // Non-object override — base is returned unchanged
        let overrides = json!("not_an_object");
        let merged = merge_input(&base, &overrides);

        assert_eq!(merged["key1"], json!("base_value"));
    }

    #[test]
    fn test_event_source_config_from_step_minimal() {
        let step = ClaimedStep {
            job_id: uuid::Uuid::nil(),
            workspace: "default".to_string(),
            task_name: "my-task".to_string(),
            step_name: "event-source".to_string(),
            action_name: "my-source".to_string(),
            action_type: "event_source".to_string(),
            action_image: None,
            action_spec: Some(json!({
                "target_task": "process-event",
                "runner": "local",
                "script": "#!/bin/sh\necho '{}'",
            })),
            input: None,
            runner: None,
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
        };

        let config = EventSourceConfig::from_step(&step).unwrap();

        assert_eq!(config.workspace, "default");
        assert_eq!(config.target_task, "process-event");
        assert_eq!(config.runner, "local");
        assert_eq!(config.restart_policy, "always");
        assert_eq!(config.backoff_secs, 5);
        assert!(config.max_in_flight.is_none());
        assert!(config.manifest.is_none());
    }

    #[test]
    fn test_event_source_config_from_step_full() {
        let step = ClaimedStep {
            job_id: uuid::Uuid::nil(),
            workspace: "myws".to_string(),
            task_name: "my-task".to_string(),
            step_name: "sensor".to_string(),
            action_name: "sensor-action".to_string(),
            action_type: "event_source".to_string(),
            action_image: None,
            action_spec: Some(json!({
                "target_task": "handle-event",
                "runner": "docker",
                "image": "myimage:latest",
                "language": "python",
                "dependencies": ["requests", "boto3"],
                "interpreter": "python3",
                "env": { "API_KEY": "secret123" },
                "input_defaults": { "region": "us-east-1" },
                "restart_policy": "on_failure",
                "backoff_secs": 10,
                "max_in_flight": 5,
                "manifest": { "spec": { "serviceAccountName": "my-sa" } },
            })),
            input: None,
            runner: None,
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
        };

        let config = EventSourceConfig::from_step(&step).unwrap();

        assert_eq!(config.workspace, "myws");
        assert_eq!(config.target_task, "handle-event");
        assert_eq!(config.runner, "docker");
        assert_eq!(config.image.as_deref(), Some("myimage:latest"));
        assert_eq!(config.language.as_deref(), Some("python"));
        assert_eq!(config.dependencies, vec!["requests", "boto3"]);
        assert_eq!(config.interpreter.as_deref(), Some("python3"));
        assert_eq!(
            config.env.get("API_KEY").map(|s| s.as_str()),
            Some("secret123")
        );
        assert_eq!(
            config.input_defaults.get("region"),
            Some(&json!("us-east-1"))
        );
        assert_eq!(config.restart_policy, "on_failure");
        assert_eq!(config.backoff_secs, 10);
        assert_eq!(config.max_in_flight, Some(5));
        assert!(config.manifest.is_some());
    }

    #[test]
    fn test_event_source_config_missing_target_task_fails() {
        let step = ClaimedStep {
            job_id: uuid::Uuid::nil(),
            workspace: "default".to_string(),
            task_name: "my-task".to_string(),
            step_name: "source".to_string(),
            action_name: "source-action".to_string(),
            action_type: "event_source".to_string(),
            action_image: None,
            action_spec: Some(json!({ "runner": "local" })),
            input: None,
            runner: None,
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
        };

        let result = EventSourceConfig::from_step(&step);
        assert!(result.is_err(), "should fail without target_task");
        assert!(result.unwrap_err().to_string().contains("target_task"));
    }

    #[test]
    fn test_event_source_config_missing_action_spec_fails() {
        let step = ClaimedStep {
            job_id: uuid::Uuid::nil(),
            workspace: "default".to_string(),
            task_name: "my-task".to_string(),
            step_name: "source".to_string(),
            action_name: "source-action".to_string(),
            action_type: "event_source".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            runner: None,
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
        };

        let result = EventSourceConfig::from_step(&step);
        assert!(result.is_err(), "should fail without action_spec");
    }

    #[test]
    fn test_event_source_config_input_defaults_from_step_input() {
        // When input_defaults is not in action_spec, fall back to step.input
        let step = ClaimedStep {
            job_id: uuid::Uuid::nil(),
            workspace: "default".to_string(),
            task_name: "my-task".to_string(),
            step_name: "source".to_string(),
            action_name: "source-action".to_string(),
            action_type: "event_source".to_string(),
            action_image: None,
            action_spec: Some(json!({ "target_task": "handle", "runner": "local" })),
            input: Some(json!({ "region": "eu-west-1", "env": "prod" })),
            runner: None,
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
        };

        let config = EventSourceConfig::from_step(&step).unwrap();
        assert_eq!(
            config.input_defaults.get("region"),
            Some(&json!("eu-west-1"))
        );
        assert_eq!(config.input_defaults.get("env"), Some(&json!("prod")));
    }
}
