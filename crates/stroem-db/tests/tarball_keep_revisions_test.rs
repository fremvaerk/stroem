use anyhow::Result;
use sqlx::PgPool;
use stroem_db::{JobRepo, JobStepRepo, NewJobStep};
use uuid::Uuid;

async fn setup_db() -> Result<PgPool> {
    Ok(stroem_test_support::test_pool().await)
}

async fn job(pool: &PgPool, workspace: &str, revision: &str) -> Result<Uuid> {
    JobRepo::create(
        pool,
        workspace,
        "t",
        "distributed",
        None,
        "api",
        None,
        Some(revision),
        None,
    )
    .await
}

async fn set_status(pool: &PgPool, id: Uuid, status: &str) -> Result<()> {
    sqlx::query("UPDATE job SET status = $1, completed_at = NOW() WHERE job_id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn cross_ws_step(pool: &PgPool, job_id: Uuid, owner: &str, owner_rev: &str) -> Result<()> {
    let step = NewJobStep {
        action_ref: None,
        task_workspace: None,
        task_ref: None,
        task_revision: None,
        job_id,
        step_name: "s".to_string(),
        action_name: format!("{owner}.act"),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: "pending".to_string(),
        required_ability: "script".to_string(),
        required_tags: vec![],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
        for_each_expr: None,
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
        max_retries: None,
        retry_backoff_secs: None,
        retry_strategy: None,
        retry_jitter: false,
        action_workspace: Some(owner.to_string()),
        action_revision: Some(owner_rev.to_string()),
    };
    JobStepRepo::create_steps(pool, &[step]).await
}

/// Failed top-level job still owed a task-level retry.
async fn owed_retry(pool: &PgPool, id: Uuid, attempt: i32, max: i32, age: &str) -> Result<()> {
    sqlx::query(
        "UPDATE job SET status = 'failed', max_retries = $1, retry_attempt = $2, \
         completed_at = NOW() - $3::interval WHERE job_id = $4",
    )
    .bind(max)
    .bind(attempt)
    .bind(age)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

fn has(rows: &[(String, String)], ws: &str, rev: &str) -> bool {
    rows.iter().any(|(w, r)| w == ws && r == rev)
}

#[tokio::test]
async fn keeps_active_job_revision_and_drops_finished() -> Result<()> {
    let pool = setup_db().await?;
    let _active = job(&pool, "default", "r-active").await?;
    let done = job(&pool, "default", "r-done").await?;
    set_status(&pool, done, "completed").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(has(&rows, "default", "r-active"));
    assert!(!has(&rows, "default", "r-done"));
    Ok(())
}

#[tokio::test]
async fn keeps_cross_workspace_action_revision_of_active_job() -> Result<()> {
    let pool = setup_db().await?;
    let caller = job(&pool, "caller", "c1").await?;
    cross_ws_step(&pool, caller, "owner", "o7").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(
        has(&rows, "owner", "o7"),
        "cross-workspace owner revision must be kept: {rows:?}"
    );
    Ok(())
}

#[tokio::test]
async fn drops_cross_workspace_action_revision_of_finished_job() -> Result<()> {
    let pool = setup_db().await?;
    let caller = job(&pool, "caller", "c1").await?;
    cross_ws_step(&pool, caller, "owner", "o7").await?;
    set_status(&pool, caller, "completed").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(!has(&rows, "owner", "o7"));
    Ok(())
}

#[tokio::test]
async fn keeps_revision_of_failed_job_awaiting_task_retry() -> Result<()> {
    let pool = setup_db().await?;
    let id = job(&pool, "default", "r-retry").await?;
    owed_retry(&pool, id, 0, 2, "1 minute").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(
        has(&rows, "default", "r-retry"),
        "handoff window must keep the revision"
    );
    Ok(())
}

#[tokio::test]
async fn drops_failed_revision_once_retry_created_or_exhausted_or_stale() -> Result<()> {
    let pool = setup_db().await?;
    let created = job(&pool, "default", "r-created").await?;
    owed_retry(&pool, created, 0, 2, "1 minute").await?;
    let retry = job(&pool, "other", "x").await?;
    set_status(&pool, retry, "completed").await?;
    JobRepo::set_retry_job_id(&pool, created, retry).await?;

    let exhausted = job(&pool, "default", "r-exhausted").await?;
    owed_retry(&pool, exhausted, 2, 2, "1 minute").await?;

    let stale = job(&pool, "default", "r-stale").await?;
    owed_retry(&pool, stale, 0, 2, "2 hours").await?;

    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(!has(&rows, "default", "r-created"));
    assert!(!has(&rows, "default", "r-exhausted"));
    assert!(!has(&rows, "default", "r-stale"));
    Ok(())
}

#[tokio::test]
async fn child_jobs_never_hold_a_retry_window() -> Result<()> {
    let pool = setup_db().await?;
    let parent = job(&pool, "default", "r-parent").await?;
    set_status(&pool, parent, "completed").await?;
    let child = job(&pool, "default", "r-child").await?;
    owed_retry(&pool, child, 0, 2, "1 minute").await?;
    sqlx::query("UPDATE job SET parent_job_id = $1 WHERE job_id = $2")
        .bind(parent)
        .bind(child)
        .execute(&pool)
        .await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(!has(&rows, "default", "r-child"));
    Ok(())
}

async fn pin_job(pool: &PgPool, workspace: &str, commit: &str, git_ref: &str) -> Result<Uuid> {
    let id = job(pool, workspace, commit).await?;
    sqlx::query("UPDATE job SET git_ref = $1 WHERE job_id = $2")
        .bind(git_ref)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(id)
}

/// A step whose action (and/or `type: task` target) is pinned.
async fn pinned_step(
    pool: &PgPool,
    job_id: Uuid,
    name: &str,
    action: Option<(&str, &str, &str)>,
    task: Option<(&str, &str, &str)>,
) -> Result<()> {
    let step = NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "act".to_string(),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: "pending".to_string(),
        required_ability: "script".to_string(),
        required_tags: vec![],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
        for_each_expr: None,
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
        max_retries: None,
        retry_backoff_secs: None,
        retry_strategy: None,
        retry_jitter: false,
        action_workspace: action.map(|a| a.0.to_string()),
        action_revision: action.map(|a| a.2.to_string()),
        action_ref: action.map(|a| a.1.to_string()),
        task_workspace: task.map(|t| t.0.to_string()),
        task_ref: task.map(|t| t.1.to_string()),
        task_revision: task.map(|t| t.2.to_string()),
    };
    JobStepRepo::create_steps(pool, &[step]).await
}

#[tokio::test]
async fn pin_keep_set_keeps_only_active_pinned_jobs() -> Result<()> {
    let pool = setup_db().await?;
    pin_job(&pool, "w", "p-active", "release/1").await?;
    let done = pin_job(&pool, "w", "p-done", "release/1").await?;
    set_status(&pool, done, "completed").await?;
    job(&pool, "w", "unpinned-active").await?;
    let rows = JobRepo::pin_keep_set(&pool).await?;
    assert!(has(&rows, "w", "p-active"));
    assert!(!has(&rows, "w", "p-done"));
    assert!(
        !has(&rows, "w", "unpinned-active"),
        "an unpinned job holds no pin"
    );
    Ok(())
}

#[tokio::test]
async fn pin_keep_set_keeps_action_and_task_pins_of_active_jobs() -> Result<()> {
    let pool = setup_db().await?;
    let active = job(&pool, "caller", "c1").await?;
    pinned_step(&pool, active, "a", Some(("w", "release/2", "a-pin")), None).await?;
    pinned_step(&pool, active, "t", None, Some(("billing", "v4", "t-pin"))).await?;
    // A live cross-workspace step (no action_ref) holds no pin.
    pinned_step(&pool, active, "x", None, None).await?;
    sqlx::query(
        "UPDATE job_step SET action_workspace = 'o', action_revision = 'live-rev' WHERE step_name = 'x'",
    )
    .execute(&pool)
    .await?;
    let finished = job(&pool, "caller", "c2").await?;
    pinned_step(
        &pool,
        finished,
        "a",
        Some(("w", "release/2", "a-done")),
        None,
    )
    .await?;
    set_status(&pool, finished, "completed").await?;
    let rows = JobRepo::pin_keep_set(&pool).await?;
    assert!(has(&rows, "w", "a-pin"));
    assert!(has(&rows, "billing", "t-pin"));
    assert!(!has(&rows, "o", "live-rev"));
    assert!(!has(&rows, "w", "a-done"));
    Ok(())
}

#[tokio::test]
async fn pin_keep_set_keeps_a_failed_pinned_job_awaiting_task_retry() -> Result<()> {
    let pool = setup_db().await?;
    let owed = pin_job(&pool, "w", "p-retry", "release/1").await?;
    owed_retry(&pool, owed, 0, 2, "1 minute").await?;
    let stale = pin_job(&pool, "w", "p-stale", "release/1").await?;
    owed_retry(&pool, stale, 0, 2, "2 hours").await?;
    let rows = JobRepo::pin_keep_set(&pool).await?;
    assert!(has(&rows, "w", "p-retry"));
    assert!(!has(&rows, "w", "p-stale"));
    Ok(())
}
