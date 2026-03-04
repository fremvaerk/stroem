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
