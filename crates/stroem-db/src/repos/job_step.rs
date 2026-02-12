use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::FlowStep;
use uuid::Uuid;

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
}

/// Repository for job step operations
pub struct JobStepRepo;

impl JobStepRepo {
    /// Create steps for a job (batch insert)
    pub async fn create_steps(pool: &PgPool, steps: &[NewJobStep]) -> Result<()> {
        if steps.is_empty() {
            return Ok(());
        }

        // Build a batch insert query
        let mut query = String::from(
            r#"
            INSERT INTO job_step (job_id, step_name, action_name, action_type, action_image, action_spec, input, status)
            VALUES
            "#,
        );

        let mut bindings = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            if i > 0 {
                query.push_str(", ");
            }
            let base = i * 8;
            query.push_str(&format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8
            ));
            bindings.push((
                step.job_id,
                step.step_name.clone(),
                step.action_name.clone(),
                step.action_type.clone(),
                step.action_image.clone(),
                step.action_spec.clone(),
                step.input.clone(),
                step.status.clone(),
            ));
        }

        let mut q = sqlx::query(&query);
        for binding in bindings {
            q = q
                .bind(binding.0)
                .bind(binding.1)
                .bind(binding.2)
                .bind(binding.3)
                .bind(binding.4)
                .bind(binding.5)
                .bind(binding.6)
                .bind(binding.7);
        }

        q.execute(pool).await?;
        Ok(())
    }

    /// Claim a ready step for a worker (SELECT FOR UPDATE SKIP LOCKED)
    /// This is the key concurrency-safe operation.
    /// capabilities: worker's capabilities array (e.g. ["shell", "docker"])
    pub async fn claim_ready_step(
        pool: &PgPool,
        capabilities: &[String],
        worker_id: Uuid,
    ) -> Result<Option<JobStepRow>> {
        let step = sqlx::query_as::<_, JobStepRow>(
            r#"
            UPDATE job_step SET status = 'running', worker_id = $2, started_at = NOW()
            WHERE (job_id, step_name) = (
                SELECT job_id, step_name FROM job_step
                WHERE status = 'ready' AND action_type = ANY($1)
                ORDER BY job_id, step_name
                FOR UPDATE SKIP LOCKED
                LIMIT 1
            )
            RETURNING job_id, step_name, action_name, action_type, action_image, action_spec,
                      input, output, status, worker_id, started_at, completed_at, error_message
            "#,
        )
        .bind(capabilities)
        .bind(worker_id)
        .fetch_optional(pool)
        .await?;

        Ok(step)
    }

    /// Get all steps for a job
    pub async fn get_steps_for_job(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobStepRow>> {
        let steps = sqlx::query_as::<_, JobStepRow>(
            r#"
            SELECT job_id, step_name, action_name, action_type, action_image, action_spec,
                   input, output, status, worker_id, started_at, completed_at, error_message
            FROM job_step
            WHERE job_id = $1
            ORDER BY step_name
            "#,
        )
        .bind(job_id)
        .fetch_all(pool)
        .await?;

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
        .await?;

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
        .await?;

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
        .await?;

        Ok(())
    }

    /// Get ready steps for a job (for orchestrator to check)
    pub async fn get_ready_steps(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobStepRow>> {
        let steps = sqlx::query_as::<_, JobStepRow>(
            r#"
            SELECT job_id, step_name, action_name, action_type, action_image, action_spec,
                   input, output, status, worker_id, started_at, completed_at, error_message
            FROM job_step
            WHERE job_id = $1 AND status = 'ready'
            ORDER BY step_name
            "#,
        )
        .bind(job_id)
        .fetch_all(pool)
        .await?;

        Ok(steps)
    }

    /// Check if all steps for a job are terminal (completed/failed/skipped)
    pub async fn all_steps_terminal(pool: &PgPool, job_id: Uuid) -> Result<bool> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM job_step
            WHERE job_id = $1 AND status NOT IN ('completed', 'failed', 'skipped')
            "#,
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?;

        Ok(row.0 == 0)
    }

    /// Get the names of all failed steps for a job
    pub async fn get_failed_step_names(pool: &PgPool, job_id: Uuid) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT step_name FROM job_step WHERE job_id = $1 AND status = 'failed'",
        )
        .bind(job_id)
        .fetch_all(pool)
        .await?;
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
        .await?;

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
        .await?;

        Ok(())
    }

    /// Update steps from pending to ready based on completed dependencies
    /// This is called by the orchestrator after a step completes
    /// Returns the names of newly promoted steps
    pub async fn promote_ready_steps(
        pool: &PgPool,
        job_id: Uuid,
        flow: &HashMap<String, FlowStep>,
    ) -> Result<Vec<String>> {
        // Get all steps for the job
        let steps = Self::get_steps_for_job(pool, job_id).await?;

        // Build a map of step statuses
        let status_map: HashMap<String, String> = steps
            .iter()
            .map(|s| (s.step_name.clone(), s.status.clone()))
            .collect();

        // Find pending steps that can be promoted
        let mut promoted = Vec::new();

        for step in steps.iter().filter(|s| s.status == "pending") {
            // Check if all dependencies are completed
            if let Some(flow_step) = flow.get(&step.step_name) {
                let deps_met = flow_step.depends_on.iter().all(|dep| {
                    status_map
                        .get(dep)
                        .map(|status| {
                            if flow_step.continue_on_failure {
                                status == "completed" || status == "failed"
                            } else {
                                status == "completed"
                            }
                        })
                        .unwrap_or(false)
                });

                if deps_met {
                    // Promote this step to ready
                    sqlx::query(
                        r#"
                        UPDATE job_step
                        SET status = 'ready'
                        WHERE job_id = $1 AND step_name = $2
                        "#,
                    )
                    .bind(job_id)
                    .bind(&step.step_name)
                    .execute(pool)
                    .await?;

                    promoted.push(step.step_name.clone());
                }
            }
        }

        Ok(promoted)
    }

    /// Mark a step as skipped (unreachable due to failed dependency)
    pub async fn mark_skipped(pool: &PgPool, job_id: Uuid, step_name: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'skipped', completed_at = NOW()
            WHERE job_id = $1 AND step_name = $2
            "#,
        )
        .bind(job_id)
        .bind(step_name)
        .execute(pool)
        .await?;

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
        let steps = Self::get_steps_for_job(pool, job_id).await?;
        let status_map: HashMap<String, String> = steps
            .iter()
            .map(|s| (s.step_name.clone(), s.status.clone()))
            .collect();

        let mut skipped = Vec::new();
        for step in steps.iter().filter(|s| s.status == "pending") {
            if let Some(flow_step) = flow.get(&step.step_name) {
                if flow_step.continue_on_failure {
                    continue;
                }
                let has_failed_dep = flow_step.depends_on.iter().any(|dep| {
                    status_map
                        .get(dep)
                        .map(|s| s == "failed" || s == "skipped")
                        .unwrap_or(false)
                });

                if has_failed_dep {
                    Self::mark_skipped(pool, job_id, &step.step_name).await?;
                    skipped.push(step.step_name.clone());
                }
            }
        }
        Ok(skipped)
    }
}
