use anyhow::Result;
use sqlx::PgPool;
use stroem_db::run_migrations;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&url)
        .await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

#[tokio::test]
async fn test_double_migration_is_idempotent() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Insert some data
    let job_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(job_id)
    .bind("default")
    .bind("test-task")
    .bind("distributed")
    .bind("pending")
    .bind("api")
    .execute(&pool)
    .await?;

    // Run migrations again — should succeed without error
    run_migrations(&pool).await?;

    // Verify data is still intact
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM job WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.0, 1, "Data should survive second migration run");

    Ok(())
}

#[tokio::test]
async fn test_schema_completeness() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    // Query information_schema for all tables
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await?;

    let table_names: Vec<&str> = tables.iter().map(|t| t.0.as_str()).collect();

    // Verify all expected tables exist
    let expected = [
        "job",
        "job_step",
        "worker",
        "user",
        "refresh_token",
        "user_auth_link",
        "api_key",
    ];

    for table in &expected {
        assert!(
            table_names.contains(table),
            "Expected table '{}' not found. Found: {:?}",
            table,
            table_names
        );
    }

    Ok(())
}

/// Migration 048 backfills `source_job_id` on hook jobs from the UUID prefix of
/// their `source_id` (`{job_id}` or the legacy `{job_id}/{hook}`), but only
/// where the firing job still exists (the column is an FK) and only for
/// `source_type = 'hook'` — a retry's `source_id` is also a bare job id and
/// must not be read as hook lineage.
///
/// Rows are inserted in the pre-048 shape after all migrations have run, then
/// the migration's SQL is executed again; it only touches NULL rows, so a
/// second application is exactly what the first did to legacy data.
#[tokio::test]
async fn test_048_backfills_hook_source_job_id() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    async fn insert(
        pool: &PgPool,
        source_type: &str,
        source_id: Option<&str>,
    ) -> Result<uuid::Uuid> {
        let job_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id) \
             VALUES ($1, 'default', 't', 'distributed', 'completed', $2, $3)",
        )
        .bind(job_id)
        .bind(source_type)
        .bind(source_id)
        .execute(pool)
        .await?;
        Ok(job_id)
    }

    let fired_by = insert(&pool, "api", None).await?;
    let fired = fired_by.to_string();
    let bare = insert(&pool, "hook", Some(&fired)).await?;
    let suffixed = insert(&pool, "hook", Some(&format!("{fired}/notify"))).await?;
    let upper = insert(&pool, "hook", Some(&fired.to_uppercase())).await?;
    let deleted_source = insert(&pool, "hook", Some(&uuid::Uuid::new_v4().to_string())).await?;
    let not_a_uuid = insert(&pool, "hook", Some("not-a-uuid/at-all")).await?;
    let no_source = insert(&pool, "hook", None).await?;
    let retry = insert(&pool, "retry", Some(&fired)).await?;
    // Already written by a 048-aware server: its pointer wins over source_id.
    let other = insert(&pool, "api", None).await?;
    let already_set = insert(&pool, "hook", Some(&fired)).await?;
    sqlx::query("UPDATE job SET source_job_id = $1 WHERE job_id = $2")
        .bind(other)
        .bind(already_set)
        .execute(&pool)
        .await?;

    sqlx::raw_sql(include_str!("../migrations/048_hook_source_job_id.sql"))
        .execute(&pool)
        .await?;

    async fn source_job_id(pool: &PgPool, job_id: uuid::Uuid) -> Result<Option<uuid::Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT source_job_id FROM job WHERE job_id = $1")
                .bind(job_id)
                .fetch_one(pool)
                .await?,
        )
    }

    assert_eq!(source_job_id(&pool, bare).await?, Some(fired_by));
    assert_eq!(source_job_id(&pool, suffixed).await?, Some(fired_by));
    assert_eq!(source_job_id(&pool, upper).await?, Some(fired_by));
    assert_eq!(source_job_id(&pool, deleted_source).await?, None);
    assert_eq!(source_job_id(&pool, not_a_uuid).await?, None);
    assert_eq!(source_job_id(&pool, no_source).await?, None);
    assert_eq!(source_job_id(&pool, retry).await?, None);
    assert_eq!(source_job_id(&pool, already_set).await?, Some(other));

    Ok(())
}

/// Migrations 049 + 050 (spec 2026-10-02 § 6): the new columns exist, the
/// state lookups use the new `_ref` indexes, the old ones are gone, and both
/// files are re-runnable (an operator may have pre-run them by hand).
#[tokio::test]
async fn test_049_050_git_ref_columns_and_indexes() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    for (table, column) in [
        ("job", "git_ref"),
        ("job", "task_folder"),
        ("job_step", "action_ref"),
        ("job_step", "task_workspace"),
        ("job_step", "task_ref"),
        ("job_step", "task_revision"),
        ("job_step", "pin_releases"),
        ("task_state", "git_ref"),
        ("workspace_state", "git_ref"),
    ] {
        let found: Option<(String,)> = sqlx::query_as(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_optional(&pool)
        .await?;
        assert!(found.is_some(), "{table}.{column} missing");
    }

    let indexes: Vec<(String,)> =
        sqlx::query_as("SELECT indexname::text FROM pg_indexes WHERE schemaname = 'public'")
            .fetch_all(&pool)
            .await?;
    let names: Vec<&str> = indexes.iter().map(|r| r.0.as_str()).collect();
    for want in [
        "idx_task_state_lookup_ref",
        "idx_workspace_state_lookup_ref",
        "idx_job_pinned_tasks",
        "idx_job_step_pinned",
    ] {
        assert!(names.contains(&want), "{want} missing: {names:?}");
    }
    // The redaction short-circuit's step probe is the partial index's own
    // predicate, so the planner can answer it from the (tiny) index.
    let (def,): (String,) = sqlx::query_as(
        "SELECT indexdef::text FROM pg_indexes WHERE indexname = 'idx_job_step_pinned'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        def.contains("(action_ref IS NOT NULL) OR (task_ref IS NOT NULL)"),
        "{def}"
    );
    for gone in ["idx_task_state_lookup", "idx_workspace_state_lookup"] {
        assert!(!names.contains(&gone), "{gone} should be dropped");
    }

    sqlx::raw_sql(include_str!("../migrations/049_git_refs.sql"))
        .execute(&pool)
        .await?;
    sqlx::raw_sql(include_str!("../migrations/050_git_refs_indexes.sql"))
        .execute(&pool)
        .await?;
    Ok(())
}
