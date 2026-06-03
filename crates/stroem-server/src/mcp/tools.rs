use super::auth::{check_task_acl, resolve_acl_scope, McpAuthContext};
use super::handler::StromMcpHandler;
use crate::acl::TaskPermission;
use crate::log_storage::JobLogMeta;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stroem_db::{JobRepo, JobStepRepo};

/// Max size of an artifact body returned via MCP `get_artifact`. Larger blobs
/// must be downloaded out-of-band via the HTTP API to avoid blowing the agent's
/// context window.
const MCP_ARTIFACT_MAX_BYTES: i64 = 1024 * 1024;

/// Content-type prefixes considered "textual" for `get_artifact`. Anything else
/// is rejected so binary blobs (images, archives, executables) cannot be smuggled
/// into the agent's prompt.
const MCP_TEXT_PREFIXES: &[&str] = &[
    "text/",
    "application/json",
    "application/yaml",
    "application/xml",
];

// ---------------------------------------------------------------------------
// Parameter structs
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema, Default)]
pub struct ListTasksParams {
    /// Filter by workspace name. If omitted, lists tasks from all workspaces.
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetTaskParams {
    /// Workspace name
    pub workspace: String,
    /// Task name
    pub task_name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExecuteTaskParams {
    /// Workspace name
    pub workspace: String,
    /// Task name to execute
    pub task_name: String,
    /// Input parameters for the task (optional)
    #[serde(default)]
    pub input: Option<serde_json::Value>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetJobStatusParams {
    /// Job ID (UUID)
    pub job_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetJobLogsParams {
    /// Job ID (UUID)
    pub job_id: String,
    /// Step name to filter logs. If omitted, returns all logs.
    #[serde(default)]
    pub step: Option<String>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct ListJobsParams {
    /// Filter by workspace name
    #[serde(default)]
    pub workspace: Option<String>,
    /// Filter by task name (requires workspace)
    #[serde(default)]
    pub task_name: Option<String>,
    /// Filter by status (pending, running, completed, failed, cancelled)
    #[serde(default)]
    pub status: Option<String>,
    /// Maximum number of jobs to return (default: 20)
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CancelJobParams {
    /// Job ID (UUID) to cancel
    pub job_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListArtifactsParams {
    /// Job ID (UUID)
    pub job_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetArtifactParams {
    /// Job ID (UUID)
    pub job_id: String,
    /// Artifact name as returned by `list_artifacts`.
    pub name: String,
}

// ---------------------------------------------------------------------------
// Response structs (for JSON serialization only)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WorkspaceInfo {
    name: String,
    task_count: usize,
}

#[derive(Serialize)]
struct TaskSummary {
    name: String,
    workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
    can_execute: bool,
}

#[derive(Serialize)]
struct TaskDetail {
    name: String,
    workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
    input: std::collections::HashMap<String, serde_json::Value>,
    flow: Vec<FlowStepSummary>,
}

#[derive(Serialize)]
struct FlowStepSummary {
    name: String,
    action: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    when: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn internal_err(msg: impl Into<String>) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(msg.into(), None)
}

fn not_found(entity: &str) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(format!("{entity} not found"), None)
}

fn acl_err(msg: impl Into<String>) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(msg.into(), None)
}

/// Resolve the ACL scope for the current user and return it as a lookup set.
/// Returns `None` when no filtering is needed (no auth, no ACL, or admin).
async fn resolve_scope(
    handler: &StromMcpHandler,
) -> Result<Option<Vec<(String, String, TaskPermission)>>, rmcp::ErrorData> {
    resolve_acl_scope(&handler.state, &handler.auth)
        .await
        .map_err(acl_err)
}

/// Check ACL for a specific task. Returns the permission level.
async fn check_task_permission(
    handler: &StromMcpHandler,
    workspace: &str,
    task_name: &str,
    folder: Option<&str>,
) -> Result<TaskPermission, rmcp::ErrorData> {
    check_task_acl(&handler.state, &handler.auth, workspace, task_name, folder)
        .await
        .map_err(acl_err)
}

/// Get the source_id for audit trail (email when authenticated).
fn source_id_for_audit(auth: &Option<McpAuthContext>) -> Option<String> {
    auth.as_ref().map(|a| a.claims.email.clone())
}

/// Check ACL for a job by looking up its workspace/task.
/// Deny → not_found("Job"), View/Run → Ok(perm).
async fn check_job_acl(
    handler: &StromMcpHandler,
    job: &stroem_db::JobRow,
) -> Result<TaskPermission, rmcp::ErrorData> {
    // Look up the task's folder from workspace config for proper ACL path
    let folder = handler
        .state
        .workspaces
        .get_config(&job.workspace)
        .await
        .and_then(|ws| ws.tasks.get(&job.task_name).and_then(|t| t.folder.clone()));

    let perm =
        check_task_permission(handler, &job.workspace, &job.task_name, folder.as_deref()).await?;
    if perm == TaskPermission::Deny {
        return Err(not_found("Job"));
    }
    Ok(perm)
}

fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![rmcp::model::Content::text(text)])
}

fn json_result(value: &impl Serialize) -> CallToolResult {
    let text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}).to_string());
    CallToolResult::success(vec![rmcp::model::Content::text(text)])
}

// ---------------------------------------------------------------------------
// Tool implementations via #[tool_router] / #[tool] macros
// ---------------------------------------------------------------------------

#[tool_router(vis = "pub(super)")]
impl StromMcpHandler {
    /// List all workspaces and their task counts.
    #[tool(description = "List all workspaces and their task counts")]
    async fn list_workspaces(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        tracing::info!("MCP: list_workspaces");
        let scope = resolve_scope(self).await?;

        let mut workspaces = Vec::new();
        for (ws_name, ws_config) in self.state.workspaces.get_all_configs().await {
            let task_count = if let Some(ref allowed) = scope {
                // Count only tasks the user has any access to (View or Run)
                ws_config
                    .tasks
                    .keys()
                    .filter(|t| allowed.iter().any(|(w, tn, _)| w == &ws_name && tn == *t))
                    .count()
            } else {
                ws_config.tasks.len()
            };
            // Skip workspaces with zero accessible tasks
            if task_count > 0 {
                workspaces.push(WorkspaceInfo {
                    name: ws_name,
                    task_count,
                });
            }
        }
        workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(json_result(&workspaces))
    }

    /// List tasks from workspaces. Optionally filter by workspace name.
    #[tool(description = "List tasks from workspaces. Optionally filter by workspace name.")]
    async fn list_tasks(
        &self,
        params: Parameters<ListTasksParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(workspace = ?params.workspace, "MCP: list_tasks");
        let all_configs = self.state.workspaces.get_all_configs().await;
        let scope = resolve_scope(self).await?;
        let mut tasks = Vec::new();

        for (ws_name, ws_config) in &all_configs {
            if let Some(ref filter_ws) = params.workspace {
                if ws_name != filter_ws {
                    continue;
                }
            }
            for (task_name, task_def) in &ws_config.tasks {
                let can_execute = if let Some(ref allowed) = scope {
                    match allowed
                        .iter()
                        .find(|(w, t, _)| w == ws_name && t == task_name)
                    {
                        None => continue, // Deny — skip this task
                        Some((_, _, TaskPermission::Run)) => true,
                        Some(_) => false, // View only
                    }
                } else {
                    true // No ACL filtering — full access
                };
                tasks.push(TaskSummary {
                    name: task_name.clone(),
                    workspace: ws_name.clone(),
                    display_name: task_def.name.clone(),
                    description: task_def.description.clone(),
                    folder: task_def.folder.clone(),
                    can_execute,
                });
            }
        }

        if let Some(ref filter_ws) = params.workspace {
            if !all_configs.iter().any(|(ws, _)| ws == filter_ws) {
                return Err(not_found("Workspace"));
            }
        }

        tasks.sort_by(|a, b| (&a.workspace, &a.name).cmp(&(&b.workspace, &b.name)));
        Ok(json_result(&tasks))
    }

    /// Get detailed information about a specific task including its input schema and flow steps.
    #[tool(
        description = "Get detailed information about a specific task including its input schema and flow steps."
    )]
    async fn get_task(
        &self,
        params: Parameters<GetTaskParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(workspace = %params.workspace, task = %params.task_name, "MCP: get_task");
        let ws_config = self
            .state
            .workspaces
            .get_config(&params.workspace)
            .await
            .ok_or_else(|| not_found("Workspace"))?;

        let task = ws_config
            .tasks
            .get(&params.task_name)
            .ok_or_else(|| not_found("Task"))?;

        // ACL: Deny → 404 (task not found)
        let perm = check_task_permission(
            self,
            &params.workspace,
            &params.task_name,
            task.folder.as_deref(),
        )
        .await?;
        if perm == TaskPermission::Deny {
            return Err(not_found("Task"));
        }

        let mut flow: Vec<FlowStepSummary> = task
            .flow
            .iter()
            .map(|(name, step)| FlowStepSummary {
                name: name.clone(),
                action: step.action.clone(),
                depends_on: step.depends_on.clone(),
                when: step.when.clone(),
            })
            .collect();
        flow.sort_by(|a, b| a.name.cmp(&b.name));

        let detail = TaskDetail {
            name: params.task_name,
            workspace: params.workspace,
            display_name: task.name.clone(),
            description: task.description.clone(),
            mode: task.mode.clone(),
            folder: task.folder.clone(),
            input: task
                .input
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or_default()))
                .collect(),
            flow,
        };

        Ok(json_result(&detail))
    }

    /// Execute a task in a workspace. Returns the created job ID.
    #[tool(description = "Execute a task in a workspace. Returns the created job ID.")]
    async fn execute_task(
        &self,
        params: Parameters<ExecuteTaskParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(workspace = %params.workspace, task = %params.task_name, "MCP: execute_task");
        let ws_config = self
            .state
            .workspaces
            .get_config(&params.workspace)
            .await
            .ok_or_else(|| not_found("Workspace"))?;

        let task = ws_config
            .tasks
            .get(&params.task_name)
            .ok_or_else(|| not_found("Task"))?;

        // ACL: Deny → 404, View → 403, Run → proceed
        let perm = check_task_permission(
            self,
            &params.workspace,
            &params.task_name,
            task.folder.as_deref(),
        )
        .await?;
        match perm {
            TaskPermission::Deny => return Err(not_found("Task")),
            TaskPermission::View => {
                return Err(acl_err(
                    "Insufficient permissions: execute requires Run access",
                ))
            }
            TaskPermission::Run => {}
        }

        let input = params
            .input
            .unwrap_or(serde_json::Value::Object(Default::default()));

        let source_id = source_id_for_audit(&self.auth);
        let revision = self.state.workspaces.get_revision(&params.workspace);
        let job_id = crate::job_creator::create_job_for_task(
            &self.state.pool,
            &ws_config,
            &params.workspace,
            &params.task_name,
            input,
            "mcp",
            source_id.as_deref(),
            revision.as_deref(),
            None,
            self.state.config.agents.as_ref(),
            crate::config::JobDefaults::from(self.state.config.as_ref()),
        )
        .await
        .map_err(|e| internal_err(format!("Failed to create job: {e}")))?;

        // Fire on_suspended hooks for any root-level approval steps that were
        // suspended during job creation (FIX 2).
        crate::job_creator::fire_initial_suspended_hooks(
            &self.state,
            &ws_config,
            &params.workspace,
            &params.task_name,
            job_id,
        )
        .await;

        Ok(json_result(
            &serde_json::json!({ "job_id": job_id.to_string() }),
        ))
    }

    /// Get the status of a job including its step details.
    #[tool(description = "Get the status of a job including its step details.")]
    async fn get_job_status(
        &self,
        params: Parameters<GetJobStatusParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(job_id = %params.job_id, "MCP: get_job_status");
        let job_id: uuid::Uuid = params
            .job_id
            .parse()
            .map_err(|_| rmcp::ErrorData::invalid_params("Invalid job_id UUID", None))?;

        let job = JobRepo::get(&self.state.pool, job_id)
            .await
            .map_err(|e| internal_err(format!("DB error: {e}")))?
            .ok_or_else(|| not_found("Job"))?;

        // ACL: Deny → 404
        check_job_acl(self, &job).await?;

        let steps = JobStepRepo::get_steps_for_job(&self.state.pool, job_id)
            .await
            .map_err(|e| internal_err(format!("DB error: {e}")))?;

        let steps_json: Vec<serde_json::Value> = steps
            .iter()
            .map(|step| {
                serde_json::json!({
                    "step_name": step.step_name,
                    "action_name": step.action_name,
                    "action_type": step.action_type,
                    "status": step.status,
                    "started_at": step.started_at,
                    "completed_at": step.completed_at,
                    "error_message": step.error_message,
                })
            })
            .collect();

        let result = serde_json::json!({
            "job_id": job.job_id,
            "workspace": job.workspace,
            "task_name": job.task_name,
            "status": job.status,
            "source_type": job.source_type,
            "revision": job.revision,
            "created_at": job.created_at.to_rfc3339(),
            "started_at": job.started_at.map(|dt| dt.to_rfc3339()),
            "completed_at": job.completed_at.map(|dt| dt.to_rfc3339()),
            "steps": steps_json,
        });

        Ok(json_result(&result))
    }

    /// Get log output for a job. Optionally filter by step name.
    #[tool(description = "Get log output for a job. Optionally filter by step name.")]
    async fn get_job_logs(
        &self,
        params: Parameters<GetJobLogsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(job_id = %params.job_id, step = ?params.step, "MCP: get_job_logs");
        let job_id: uuid::Uuid = params
            .job_id
            .parse()
            .map_err(|_| rmcp::ErrorData::invalid_params("Invalid job_id UUID", None))?;

        let job = JobRepo::get(&self.state.pool, job_id)
            .await
            .map_err(|e| internal_err(format!("DB error: {e}")))?
            .ok_or_else(|| not_found("Job"))?;

        // ACL: Deny → 404
        check_job_acl(self, &job).await?;

        let meta = JobLogMeta {
            workspace: job.workspace,
            task_name: job.task_name,
            created_at: job.created_at,
        };

        let logs = match params.step {
            Some(step) => self
                .state
                .log_storage
                .get_step_log(job_id, &step, &meta)
                .await
                .map_err(|e| internal_err(format!("Failed to get step logs: {e}")))?,
            None => self
                .state
                .log_storage
                .get_log(job_id, &meta)
                .await
                .map_err(|e| internal_err(format!("Failed to get logs: {e}")))?,
        };

        let formatted = format_logs(&logs);
        Ok(text_result(formatted))
    }

    /// List recent jobs. Filter by workspace, task, or status.
    #[tool(description = "List recent jobs. Filter by workspace, task, or status.")]
    async fn list_jobs(
        &self,
        params: Parameters<ListJobsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(workspace = ?params.workspace, task = ?params.task_name, status = ?params.status, "MCP: list_jobs");

        if params.task_name.is_some() && params.workspace.is_none() {
            return Err(rmcp::ErrorData::invalid_params(
                "task_name filter requires workspace",
                None,
            ));
        }

        let valid_statuses = [
            "pending",
            "running",
            "completed",
            "failed",
            "cancelled",
            "skipped",
        ];
        if let Some(ref s) = params.status {
            if !valid_statuses.contains(&s.as_str()) {
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "Invalid status '{}'. Must be one of: {}",
                        s,
                        valid_statuses.join(", ")
                    ),
                    None,
                ));
            }
        }

        let limit = params.limit.unwrap_or(20).clamp(1, 100);
        let status = params.status.as_deref();
        let scope = resolve_scope(self).await?;

        let jobs = match (params.workspace.as_deref(), params.task_name.as_deref()) {
            (Some(ws), Some(task)) => {
                JobRepo::list_by_task(&self.state.pool, ws, task, status, None, limit, 0)
                    .await
                    .map_err(|e| internal_err(format!("DB error: {e}")))?
            }
            _ => {
                let ws = params.workspace.as_deref();
                JobRepo::list(&self.state.pool, ws, status, None, None, limit, 0)
                    .await
                    .map_err(|e| internal_err(format!("DB error: {e}")))?
            }
        };

        // Filter jobs by ACL scope
        let jobs_json: Vec<serde_json::Value> = jobs
            .iter()
            .filter(|job| {
                if let Some(ref allowed) = scope {
                    allowed
                        .iter()
                        .any(|(w, t, _)| w == &job.workspace && t == &job.task_name)
                } else {
                    true
                }
            })
            .map(|job| {
                serde_json::json!({
                    "job_id": job.job_id,
                    "workspace": job.workspace,
                    "task_name": job.task_name,
                    "status": job.status,
                    "source_type": job.source_type,
                    "revision": job.revision,
                    "created_at": job.created_at.to_rfc3339(),
                    "completed_at": job.completed_at.map(|dt| dt.to_rfc3339()),
                })
            })
            .collect();

        Ok(json_result(&serde_json::json!({
            "count": jobs_json.len(),
            "jobs": jobs_json,
        })))
    }

    /// Cancel a running or pending job.
    #[tool(description = "Cancel a running or pending job.")]
    async fn cancel_job(
        &self,
        params: Parameters<CancelJobParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(job_id = %params.job_id, "MCP: cancel_job");
        let job_id: uuid::Uuid = params
            .job_id
            .parse()
            .map_err(|_| rmcp::ErrorData::invalid_params("Invalid job_id UUID", None))?;

        // Look up job to check ACL
        let job = JobRepo::get(&self.state.pool, job_id)
            .await
            .map_err(|e| internal_err(format!("DB error: {e}")))?
            .ok_or_else(|| not_found("Job"))?;

        // ACL: Deny → 404, View → 403, Run → proceed
        let perm = check_job_acl(self, &job).await?;
        if perm == TaskPermission::View {
            return Err(acl_err(
                "Insufficient permissions: cancel requires Run access",
            ));
        }

        match crate::cancellation::cancel_job(&self.state, job_id).await {
            Ok(crate::cancellation::CancelResult::Cancelled) => {
                Ok(json_result(&serde_json::json!({ "status": "cancelled" })))
            }
            Ok(crate::cancellation::CancelResult::NotFound) => Err(not_found("Job")),
            Ok(crate::cancellation::CancelResult::AlreadyTerminal) => {
                Ok(json_result(&serde_json::json!({
                    "status": "already_terminal",
                    "message": "Job is already in a terminal state"
                })))
            }
            Err(e) => Err(internal_err(format!("Failed to cancel job: {e}"))),
        }
    }

    /// List artifacts produced by a job.
    #[tool(
        description = "List artifacts produced by a job. Returns name, content_type, size_bytes, step_name, and a relative download URL (use list_artifacts then call get_artifact, or pass the URL to a human for binary content)."
    )]
    async fn list_artifacts(
        &self,
        params: Parameters<ListArtifactsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(job_id = %params.job_id, "MCP: list_artifacts");

        let job_id: uuid::Uuid = params
            .job_id
            .parse()
            .map_err(|_| rmcp::ErrorData::invalid_params("Invalid job_id UUID", None))?;

        let job = JobRepo::get(&self.state.pool, job_id)
            .await
            .map_err(|e| internal_err(format!("DB error: {e}")))?
            .ok_or_else(|| not_found("Job"))?;

        // ACL: Deny → 404
        check_job_acl(self, &job).await?;

        let rows = stroem_db::repos::job_artifact::JobArtifactRepo::new(self.state.pool.clone())
            .list_for_job(job_id)
            .await
            .map_err(|e| internal_err(format!("list artifacts: {e}")))?;

        let items: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|r| {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(r.name.as_bytes()).collect();
                serde_json::json!({
                    "name": r.name,
                    "content_type": r.content_type,
                    "size_bytes": r.size_bytes,
                    "step_name": r.step_name,
                    "created_at": r.created_at.to_rfc3339(),
                    "url": format!("/api/jobs/{job_id}/artifacts/{encoded}"),
                })
            })
            .collect();

        Ok(json_result(&items))
    }

    /// Fetch an artifact body (textual content only, up to 1 MiB).
    #[tool(
        description = "Read an artifact's bytes (text/JSON/YAML/XML up to 1MB only). Refuses binary or oversize files; the agent should call list_artifacts and pass the URL to a human for binary content."
    )]
    async fn get_artifact(
        &self,
        params: Parameters<GetArtifactParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Parameters(params) = params;
        tracing::info!(job_id = %params.job_id, name = %params.name, "MCP: get_artifact");

        let job_id: uuid::Uuid = params
            .job_id
            .parse()
            .map_err(|_| rmcp::ErrorData::invalid_params("Invalid job_id UUID", None))?;

        let job = JobRepo::get(&self.state.pool, job_id)
            .await
            .map_err(|e| internal_err(format!("DB error: {e}")))?
            .ok_or_else(|| not_found("Job"))?;

        // ACL: Deny → 404
        check_job_acl(self, &job).await?;

        let repo = stroem_db::repos::job_artifact::JobArtifactRepo::new(self.state.pool.clone());
        let rec = repo
            .get_by_name(job_id, &params.name)
            .await
            .map_err(|e| internal_err(format!("get artifact: {e}")))?
            .ok_or_else(|| not_found("Artifact"))?;

        // SECURITY: enforce size + content-type guards BEFORE reading the blob.
        if rec.size_bytes > MCP_ARTIFACT_MAX_BYTES {
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "artifact {} is {} bytes; MCP get_artifact caps at {}",
                    rec.name, rec.size_bytes, MCP_ARTIFACT_MAX_BYTES
                ),
                None,
            ));
        }
        let ct = rec
            .content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let is_text = MCP_TEXT_PREFIXES.iter().any(|p| ct.starts_with(p));
        if !is_text {
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "artifact {} has content_type {}; MCP get_artifact only returns textual content",
                    rec.name, rec.content_type
                ),
                None,
            ));
        }

        let blob = self
            .state
            .artifact_blob
            .clone()
            .ok_or_else(|| internal_err("artifact storage not configured"))?;
        let stored = blob
            .get(&rec.storage_key)
            .await
            .map_err(|e| internal_err(format!("blob fetch: {e:#}")))?
            .ok_or_else(|| internal_err(format!("blob missing for artifact {}", rec.name)))?;

        let text = String::from_utf8_lossy(&stored.bytes).into_owned();
        Ok(text_result(text))
    }
}

// ---------------------------------------------------------------------------
// Log formatting
// ---------------------------------------------------------------------------

/// Format JSONL log lines into human-readable text.
fn format_logs(raw: &str) -> String {
    let mut output = String::new();
    for line in raw.lines() {
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            let step = entry["step"].as_str().unwrap_or("?");
            let stream = entry["stream"].as_str().unwrap_or("stdout");
            let text = entry["line"].as_str().unwrap_or("");
            let ts = entry["ts"]
                .as_str()
                .and_then(|s| {
                    let t_pos = s.find('T')?;
                    s.get(t_pos + 1..t_pos + 9)
                })
                .unwrap_or("");
            if stream == "stderr" {
                output.push_str(&format!("[{ts}] [{step}] ERR: {text}\n"));
            } else {
                output.push_str(&format!("[{ts}] [{step}] {text}\n"));
            }
        } else if !line.trim().is_empty() {
            output.push_str(line);
            output.push('\n');
        }
    }
    if output.is_empty() {
        output.push_str("(no logs available)");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_logs_jsonl() {
        let input = r#"{"ts":"2025-01-01T12:30:45Z","step":"build","stream":"stdout","line":"compiling..."}
{"ts":"2025-01-01T12:30:46Z","step":"build","stream":"stderr","line":"warning: unused var"}
"#;
        let result = format_logs(input);
        assert!(result.contains("[12:30:45] [build] compiling..."));
        assert!(result.contains("[12:30:46] [build] ERR: warning: unused var"));
    }

    #[test]
    fn test_format_logs_empty() {
        assert_eq!(format_logs(""), "(no logs available)");
        assert_eq!(format_logs("   \n  "), "(no logs available)");
    }

    #[test]
    fn test_format_logs_plain_text_fallback() {
        let input = "not json\n";
        let result = format_logs(input);
        assert!(result.contains("not json"));
    }

    #[test]
    fn test_format_logs_missing_ts_field() {
        // When `ts` is absent the timestamp slot should be empty, not panic.
        let input = r#"{"step":"build","stream":"stdout","line":"hello"}
"#;
        let result = format_logs(input);
        assert!(result.contains("[build] hello"), "got: {result}");
        assert!(
            result.starts_with("[]"),
            "empty ts slot expected, got: {result}"
        );
    }

    #[test]
    fn test_format_logs_short_timestamp() {
        // Timestamp has fewer than 9 chars after 'T' — slice must not panic.
        let input = r#"{"ts":"T12:30","step":"s","stream":"stdout","line":"hi"}
"#;
        let result = format_logs(input);
        // The timestamp extraction will yield None (slice out of bounds), so ts=""
        assert!(result.contains("[s] hi"), "got: {result}");
    }

    #[test]
    fn test_format_logs_unicode() {
        let input = "{\"ts\":\"2025-06-01T09:00:00Z\",\"step\":\"emoji\",\"stream\":\"stdout\",\"line\":\"hello \u{1F916} world\"}\n";
        let result = format_logs(input);
        assert!(result.contains("hello \u{1F916} world"), "got: {result}");
        assert!(result.contains("[09:00:00] [emoji]"), "got: {result}");
    }

    #[test]
    fn test_format_logs_missing_step_and_stream_fields() {
        // Absent `step` defaults to "?", absent `stream` defaults to "stdout" (no ERR prefix).
        let input = r#"{"ts":"2025-01-01T08:00:00Z","line":"bare line"}
"#;
        let result = format_logs(input);
        assert!(result.contains("[?] bare line"), "got: {result}");
        assert!(
            !result.contains("ERR:"),
            "stderr prefix must not appear, got: {result}"
        );
    }

    #[test]
    fn test_format_logs_timestamp_with_timezone_offset() {
        // Only the 8 chars right after 'T' are extracted; the rest (offset) is ignored.
        let input = r#"{"ts":"2025-01-01T12:30:45+02:00","step":"deploy","stream":"stdout","line":"done"}
"#;
        let result = format_logs(input);
        assert!(result.contains("[12:30:45] [deploy] done"), "got: {result}");
    }

    #[test]
    fn test_format_logs_mixed_valid_and_invalid_lines() {
        let input = r#"{"ts":"2025-03-10T15:00:00Z","step":"init","stream":"stdout","line":"starting"}
this is plain text
{"ts":"2025-03-10T15:00:01Z","step":"init","stream":"stderr","line":"oops"}
another plain line
"#;
        let result = format_logs(input);
        assert!(
            result.contains("[15:00:00] [init] starting"),
            "got: {result}"
        );
        assert!(
            result.contains("[15:00:01] [init] ERR: oops"),
            "got: {result}"
        );
        assert!(result.contains("this is plain text"), "got: {result}");
        assert!(result.contains("another plain line"), "got: {result}");
    }
}
