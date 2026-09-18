use anyhow::Result;
use sqlx::PgPool;
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
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
    let (pool, _c) = setup_db().await?;
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
    let (pool, _c) = setup_db().await?;
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
    let (pool, _c) = setup_db().await?;
    let caller = job(&pool, "caller", "c1").await?;
    cross_ws_step(&pool, caller, "owner", "o7").await?;
    set_status(&pool, caller, "completed").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(!has(&rows, "owner", "o7"));
    Ok(())
}

#[tokio::test]
async fn keeps_revision_of_failed_job_awaiting_task_retry() -> Result<()> {
    let (pool, _c) = setup_db().await?;
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
    let (pool, _c) = setup_db().await?;
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
    let (pool, _c) = setup_db().await?;
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
