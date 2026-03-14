use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::FlowStep;
use uuid::Uuid;

const STEP_COLUMNS: &str = "job_id, step_name, action_name, action_type, action_image, action_spec, input, output, status, worker_id, started_at, completed_at, error_message, required_tags, runner, timeout_secs, when_condition";

/// Job step row from database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobStepRow {
    pub job_id: Uuid,
    pub step_name: String,
    pub action_name: String,
    pub action_type: String,
    pub action_image: Option<String>,
    pub action_spec: Option<JsonValue>,
    pub input: Option<JsonValue>,
    pub output: Option<JsonValue>,
    pub status: String,
    pub worker_id: Option<Uuid>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
    pub required_tags: JsonValue,
    pub runner: String,
    pub timeout_secs: Option<i32>,
    pub when_condition: Option<String>,
}

/// New job step for creation
#[derive(Debug, Clone)]
pub struct NewJobStep {
    pub job_id: Uuid,
    pub step_name: String,
    pub action_name: String,
    pub action_type: String,
    pub action_image: Option<String>,
    pub action_spec: Option<JsonValue>,
    pub input: Option<JsonValue>,
    pub status: String, // 'pending' or 'ready'
    pub required_tags: Vec<String>,
    pub runner: String,
    pub timeout_secs: Option<i32>,
    pub when_condition: Option<String>,
}

/// Bind parameters for a single row in the batch INSERT inside [`JobStepRepo::create_steps_tx`].
///
/// Using a named struct avoids the large anonymous tuple that would otherwise
/// require `#[allow(clippy::type_complexity)]`.
struct StepInsertRow {
    job_id: Uuid,
    step_name: String,
    action_name: String,
    action_type: String,
    action_image: Option<String>,
    action_spec: Option<JsonValue>,
    input: Option<JsonValue>,
    status: String,
    required_tags: JsonValue,
    runner: String,
    timeout_secs: Option<i32>,
    ready_at: Option<DateTime<Utc>>,
    when_condition: Option<String>,
}

/// A stale running step with its job info for recovery.
#[derive(Debug, sqlx::FromRow)]
pub struct StaleStepInfo {
    pub job_id: Uuid,
    pub step_name: String,
    pub worker_id: Option<Uuid>,
}

/// Repository for job step operations
pub struct JobStepRepo;

impl JobStepRepo {
    /// Create steps for a job (batch insert)
    pub async fn create_steps(pool: &PgPool, steps: &[NewJobStep]) -> Result<()> {
        Self::create_steps_tx(pool, steps).await
    }

    /// Create steps for a job, accepting a generic executor.
    ///
    /// Use this variant inside transactions. The `pool`-based [`create_steps`]
    /// delegates here.
    pub async fn create_steps_tx<'e, E>(executor: E, steps: &[NewJobStep]) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        if steps.is_empty() {
            return Ok(());
        }

        // Build a batch insert query
        let mut query = String::from(
            r#"
            INSERT INTO job_step (job_id, step_name, action_name, action_type, action_image, action_spec, input, status, required_tags, runner, timeout_secs, ready_at, when_condition)
            VALUES
            "#,
        );

        let mut rows: Vec<StepInsertRow> = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            if i > 0 {
                query.push_str(", ");
            }
            let base = i * 13;
            query.push_str(&format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9,
                base + 10,
                base + 11,
                base + 12,
                base + 13
            ));
            let required_tags = serde_json::to_value(&step.required_tags).unwrap_or_default();
            let ready_at = if step.status == "ready" {
                Some(Utc::now())
            } else {
                None
            };
            rows.push(StepInsertRow {
                job_id: step.job_id,
                step_name: step.step_name.clone(),
                action_name: step.action_name.clone(),
                action_type: step.action_type.clone(),
                action_image: step.action_image.clone(),
                action_spec: step.action_spec.clone(),
                input: step.input.clone(),
                status: step.status.clone(),
                required_tags,
                runner: step.runner.clone(),
                timeout_secs: step.timeout_secs,
                ready_at,
                when_condition: step.when_condition.clone(),
            });
        }

        let mut q = sqlx::query(&query);
        for row in rows {
            q = q
                .bind(row.job_id)
                .bind(row.step_name)
                .bind(row.action_name)
                .bind(row.action_type)
                .bind(row.action_image)
                .bind(row.action_spec)
                .bind(row.input)
                .bind(row.status)
                .bind(row.required_tags)
                .bind(row.runner)
                .bind(row.timeout_secs)
                .bind(row.ready_at)
                .bind(row.when_condition);
        }

        q.execute(executor)
            .await
            .context("Failed to create job steps")?;
        Ok(())
    }

    /// Claim a ready step for a worker (SELECT FOR UPDATE SKIP LOCKED)
    /// This is the key concurrency-safe operation.
    /// worker_tags: worker's tags as JSONB array — step's required_tags must be a subset
    pub async fn claim_ready_step(
        pool: &PgPool,
        worker_tags: &[String],
        worker_id: Uuid,
    ) -> Result<Option<JobStepRow>> {
        let worker_tags_json =
            serde_json::to_value(worker_tags).context("Failed to serialize worker tags")?;
        let step = sqlx::query_as::<_, JobStepRow>(&format!(
            r#"
            UPDATE job_step SET status = 'running', worker_id = $2, started_at = NOW()
            WHERE (job_id, step_name) = (
                SELECT job_id, step_name FROM job_step
                WHERE status = 'ready' AND required_tags <@ $1::jsonb AND action_type != 'task'
                ORDER BY random()
                FOR UPDATE SKIP LOCKED
                LIMIT 1
            )
            RETURNING {}
            "#,
            STEP_COLUMNS
        ))
        .bind(worker_tags_json)
        .bind(worker_id)
        .fetch_optional(pool)
        .await
        .context("Failed to claim ready step")?;

        Ok(step)
    }

    /// Get all steps for a job
    pub async fn get_steps_for_job(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobStepRow>> {
        let steps = sqlx::query_as::<_, JobStepRow>(&format!(
            "SELECT {} FROM job_step WHERE job_id = $1 ORDER BY step_name",
            STEP_COLUMNS
        ))
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("Failed to get steps for job")?;

        Ok(steps)
    }

    /// Mark step as running
    pub async fn mark_running(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        worker_id: Uuid,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'running', worker_id = $1, started_at = NOW()
            WHERE job_id = $2 AND step_name = $3
            "#,
        )
        .bind(worker_id)
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await
        .context("Failed to mark step as running")?;

        Ok(())
    }

    /// Mark step as running (server-side, no worker — used for type: task steps)
    pub async fn mark_running_server(pool: &PgPool, job_id: Uuid, step_name: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'running', started_at = NOW()
            WHERE job_id = $1 AND step_name = $2
            "#,
        )
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await
        .context("Failed to mark step as running (server-side)")?;

        Ok(())
    }

    /// Mark step as completed with output
    pub async fn mark_completed(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        output: Option<JsonValue>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'completed', output = $1, completed_at = NOW()
            WHERE job_id = $2 AND step_name = $3
            "#,
        )
        .bind(output)
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await
        .context("Failed to mark step as completed")?;

        Ok(())
    }

    /// Mark step as failed with error message
    pub async fn mark_failed(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        error: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'failed', error_message = $1, completed_at = NOW()
            WHERE job_id = $2 AND step_name = $3
            "#,
        )
        .bind(error)
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await
        .context("Failed to mark step as failed")?;

        Ok(())
    }

    /// Get ready steps for a job (for orchestrator to check)
    pub async fn get_ready_steps(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobStepRow>> {
        let steps = sqlx::query_as::<_, JobStepRow>(&format!(
            "SELECT {} FROM job_step WHERE job_id = $1 AND status = 'ready' ORDER BY step_name",
            STEP_COLUMNS
        ))
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("Failed to get ready steps")?;

        Ok(steps)
    }

    /// Check if all steps for a job are terminal (completed/failed/skipped/cancelled)
    pub async fn all_steps_terminal(pool: &PgPool, job_id: Uuid) -> Result<bool> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM job_step
            WHERE job_id = $1 AND status NOT IN ('completed', 'failed', 'skipped', 'cancelled')
            "#,
        )
        .bind(job_id)
        .fetch_one(pool)
        .await
        .context("Failed to check if all steps are terminal")?;

        Ok(row.0 == 0)
    }

    /// Get the names of all failed steps for a job
    pub async fn get_failed_step_names(pool: &PgPool, job_id: Uuid) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT step_name FROM job_step WHERE job_id = $1 AND status = 'failed'",
        )
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("Failed to get failed step names")?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Check if any step failed
    pub async fn any_step_failed(pool: &PgPool, job_id: Uuid) -> Result<bool> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM job_step
            WHERE job_id = $1 AND status = 'failed'
            "#,
        )
        .bind(job_id)
        .fetch_one(pool)
        .await
        .context("Failed to check if any step failed")?;

        Ok(row.0 > 0)
    }

    /// Update the input field for a step (used to persist rendered template input)
    pub async fn update_input(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        input: Option<JsonValue>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET input = $1
            WHERE job_id = $2 AND step_name = $3
            "#,
        )
        .bind(input)
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await
        .context("Failed to update step input")?;

        Ok(())
    }

    /// Update steps from pending to ready based on completed dependencies and
    /// `when` conditions. Steps with `continue_on_failure: true` also accept
    /// `skipped` dependencies (convergence after conditional branches).
    ///
    /// `render_context` is a JSON object used to evaluate `when` Tera templates.
    /// When `None`, steps with `when` conditions are not promoted (they stay pending).
    ///
    /// Returns the names of newly promoted steps.
    pub async fn promote_ready_steps(
        pool: &PgPool,
        job_id: Uuid,
        flow: &HashMap<String, FlowStep>,
        render_context: Option<&serde_json::Value>,
    ) -> Result<Vec<String>> {
        // TODO(optimize): the orchestrator calls promote_ready_steps and then
        // skip_unreachable_steps in the same loop iteration, each fetching all
        // steps for the job via get_steps_for_job. These two fetches could be
        // collapsed into a single DB round-trip by passing the already-loaded
        // step slice into both functions.
        //
        // TODO(harden): the read-classify-write pattern here (fetch all steps,
        // decide which to promote/skip, then UPDATE) is subject to a TOCTOU
        // race if two orchestrator instances run concurrently for the same job.
        // Upgrading the surrounding transaction to REPEATABLE READ isolation
        // would prevent a concurrent UPDATE from being missed between the SELECT
        // and the subsequent UPDATE statements.

        // Get all steps for the job
        let steps = Self::get_steps_for_job(pool, job_id).await?;

        // Build a map of step statuses
        let status_map: HashMap<String, String> = steps
            .iter()
            .map(|s| (s.step_name.clone(), s.status.clone()))
            .collect();

        // Partition into steps to promote and steps to skip (condition false)
        let mut to_promote: Vec<String> = Vec::new();
        let mut to_skip: Vec<String> = Vec::new();
        let mut to_fail: Vec<(String, String)> = Vec::new();

        for step in &steps {
            if step.status != StepStatus::Pending.as_ref() {
                continue;
            }
            let flow_step = match flow.get(&step.step_name) {
                Some(fs) => fs,
                None => continue,
            };
            let deps_met = flow_step.depends_on.iter().all(|dep| {
                status_map
                    .get(dep)
                    .map(|status| {
                        if flow_step.continue_on_failure {
                            status == StepStatus::Completed.as_ref()
                                || status == StepStatus::Failed.as_ref()
                                || status == StepStatus::Skipped.as_ref()
                        } else {
                            status == StepStatus::Completed.as_ref()
                        }
                    })
                    .unwrap_or(false)
            });
            if !deps_met {
                continue;
            }

            // Dependencies met — evaluate `when` condition if present.
            // Use the stored `when_condition` from the DB row so we evaluate
            // the expression captured at job creation time, not the potentially
            // stale current workflow config.
            if let Some(ref when_expr) = step.when_condition {
                if let Some(ctx) = render_context {
                    match stroem_common::template::evaluate_condition(when_expr, ctx) {
                        Ok(true) => to_promote.push(step.step_name.clone()),
                        Ok(false) => to_skip.push(step.step_name.clone()),
                        Err(e) => to_fail.push((
                            step.step_name.clone(),
                            format!("when condition error: {:#}", e),
                        )),
                    }
                }
                // If render_context is None, leave step as pending (can't evaluate yet)
            } else {
                to_promote.push(step.step_name.clone());
            }
        }

        if !to_promote.is_empty() {
            sqlx::query(
                r#"
                UPDATE job_step
                SET status = 'ready', ready_at = NOW()
                WHERE job_id = $1 AND step_name = ANY($2) AND status = 'pending'
                "#,
            )
            .bind(job_id)
            .bind(&to_promote)
            .execute(pool)
            .await
            .context("Failed to promote steps to ready")?;
        }

        // Mark condition-false steps as skipped (batch, guarded against concurrent transitions)
        if !to_skip.is_empty() {
            sqlx::query(
                r#"
                UPDATE job_step
                SET status = 'skipped', completed_at = NOW()
                WHERE job_id = $1 AND step_name = ANY($2) AND status = 'pending'
                "#,
            )
            .bind(job_id)
            .bind(&to_skip)
            .execute(pool)
            .await
            .context("Failed to skip condition-false steps")?;
        }

        // Mark condition-error steps as failed (guarded against concurrent transitions)
        for (name, err) in &to_fail {
            sqlx::query(
                r#"
                UPDATE job_step
                SET status = 'failed', error_message = $1, completed_at = NOW()
                WHERE job_id = $2 AND step_name = $3 AND status = 'pending'
                "#,
            )
            .bind(err)
            .bind(job_id)
            .bind(name.as_str())
            .execute(pool)
            .await
            .context("Failed to mark condition-error step as failed")?;
        }

        // Return all steps that changed state (promoted + skipped + failed)
        // so the orchestrator knows to loop for cascading effects
        let mut changed = to_promote;
        changed.extend(to_skip);
        changed.extend(to_fail.into_iter().map(|(name, _)| name));
        Ok(changed)
    }

    /// Mark a step as skipped (unreachable due to failed dependency).
    ///
    /// The `AND status = 'pending'` guard prevents overwriting a step that has
    /// already transitioned to `running`, `completed`, or `failed` due to a
    /// concurrent process.
    pub async fn mark_skipped(pool: &PgPool, job_id: Uuid, step_name: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'skipped', completed_at = NOW()
            WHERE job_id = $1 AND step_name = $2 AND status = 'pending'
            "#,
        )
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await
        .context("Failed to mark step as skipped")?;

        Ok(())
    }

    /// Skip pending steps that are unreachable due to failed/skipped dependencies.
    /// A step is unreachable if any dependency is failed or skipped and the step
    /// does not have `continue_on_failure: true`.
    /// Returns the names of newly skipped steps (call in a loop until empty for cascading).
    pub async fn skip_unreachable_steps(
        pool: &PgPool,
        job_id: Uuid,
        flow: &HashMap<String, FlowStep>,
    ) -> Result<Vec<String>> {
        // TODO(optimize): see the note in promote_ready_steps — the orchestrator
        // calls both functions back-to-back in the same loop iteration, resulting
        // in two separate get_steps_for_job fetches per cascade round. A future
        // refactor could pass the already-fetched step slice in to avoid the
        // redundant DB round-trip.
        let steps = Self::get_steps_for_job(pool, job_id).await?;
        let status_map: HashMap<String, String> = steps
            .iter()
            .map(|s| (s.step_name.clone(), s.status.clone()))
            .collect();

        // Collect all step names that must be skipped in this pass
        let to_skip: Vec<String> = steps
            .iter()
            .filter(|s| s.status == StepStatus::Pending.as_ref())
            .filter_map(|step| {
                let flow_step = flow.get(&step.step_name)?;
                if flow_step.continue_on_failure {
                    return None;
                }
                let has_failed_dep = flow_step.depends_on.iter().any(|dep| {
                    status_map
                        .get(dep)
                        .map(|s| {
                            s == StepStatus::Failed.as_ref()
                                || s == StepStatus::Skipped.as_ref()
                                || s == StepStatus::Cancelled.as_ref()
                        })
                        .unwrap_or(false)
                });
                if has_failed_dep {
                    Some(step.step_name.clone())
                } else {
                    None
                }
            })
            .collect();

        if !to_skip.is_empty() {
            sqlx::query(
                r#"
                UPDATE job_step
                SET status = 'skipped', completed_at = NOW()
                WHERE job_id = $1 AND step_name = ANY($2) AND status = 'pending'
                "#,
            )
            .bind(job_id)
            .bind(&to_skip)
            .execute(pool)
            .await
            .context("Failed to skip unreachable steps")?;
        }

        Ok(to_skip)
    }

    /// Cancel all pending/ready steps for a job. Returns the number of steps cancelled.
    pub async fn cancel_pending_steps(pool: &PgPool, job_id: Uuid) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'cancelled', completed_at = NOW()
            WHERE job_id = $1 AND status IN ('pending', 'ready')
            "#,
        )
        .bind(job_id)
        .execute(pool)
        .await
        .context("Failed to cancel pending steps")?;

        Ok(result.rows_affected())
    }

    /// Get currently running steps for a job (for active cancellation/kill).
    pub async fn get_running_steps(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobStepRow>> {
        let steps = sqlx::query_as::<_, JobStepRow>(&format!(
            "SELECT {} FROM job_step WHERE job_id = $1 AND status = 'running'",
            STEP_COLUMNS
        ))
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("Failed to get running steps")?;

        Ok(steps)
    }

    /// Mark a specific running step as cancelled. Only transitions from 'running' status
    /// to prevent retroactively cancelling already-completed steps.
    pub async fn mark_cancelled(pool: &PgPool, job_id: Uuid, step_name: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'cancelled', error_message = 'Job cancelled', completed_at = NOW()
            WHERE job_id = $1 AND step_name = $2 AND status = 'running'
            "#,
        )
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await
        .context("Failed to mark step as cancelled")?;

        Ok(())
    }

    /// Find running steps assigned to any of the given (stale) workers.
    pub async fn get_running_steps_for_workers(
        pool: &PgPool,
        worker_ids: &[Uuid],
    ) -> Result<Vec<StaleStepInfo>> {
        if worker_ids.is_empty() {
            return Ok(vec![]);
        }
        let rows = sqlx::query_as::<_, StaleStepInfo>(
            r#"
            SELECT job_id, step_name, worker_id
            FROM job_step
            WHERE status = 'running'
              AND worker_id = ANY($1)
            "#,
        )
        .bind(worker_ids)
        .fetch_all(pool)
        .await
        .context("Failed to get running steps for workers")?;
        Ok(rows)
    }

    /// Return running steps whose `timeout_secs` deadline has elapsed.
    pub async fn get_timed_out_steps(pool: &PgPool) -> Result<Vec<StaleStepInfo>> {
        let rows = sqlx::query_as::<_, StaleStepInfo>(
            "SELECT job_id, step_name, worker_id FROM job_step \
             WHERE status = 'running' AND timeout_secs IS NOT NULL \
               AND started_at + make_interval(secs => timeout_secs::double precision) < NOW()",
        )
        .fetch_all(pool)
        .await
        .context("get_timed_out_steps")?;
        Ok(rows)
    }

    /// Find ready steps that have been waiting longer than `timeout_secs` and
    /// have no active worker whose tags satisfy the step's `required_tags`.
    pub async fn get_unmatched_ready_steps(
        pool: &PgPool,
        timeout_secs: f64,
    ) -> Result<Vec<StaleStepInfo>> {
        let rows = sqlx::query_as::<_, StaleStepInfo>(
            r#"
            SELECT js.job_id, js.step_name, NULL::uuid AS worker_id
            FROM job_step js
            WHERE js.status = 'ready'
              AND js.action_type != 'task'
              AND js.ready_at < NOW() - make_interval(secs => $1::double precision)
              AND NOT EXISTS (
                  SELECT 1 FROM worker w
                  WHERE w.status = 'active'
                    AND js.required_tags <@ w.tags
              )
            "#,
        )
        .bind(timeout_secs)
        .fetch_all(pool)
        .await
        .context("get_unmatched_ready_steps")?;
        Ok(rows)
    }
}
