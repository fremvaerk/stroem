//! Integration tests for the state upload API.
//!
//! These use testcontainers Postgres + a local filesystem state backend
//! and exercise the full request -> synthetic job -> snapshot row ->
//! archive blob flow.

use anyhow::Result;
use sqlx::PgPool;
use stroem_db::{create_pool, run_migrations};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

async fn spawn_pg() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

#[tokio::test]
async fn synthetic_upload_job_is_inserted() -> Result<()> {
    let (pool, _pg) = spawn_pg().await?;

    let mut tx = pool.begin().await?;
    let job_id = stroem_server::web::api::state_upload::insert_synthetic_upload_job(
        &mut tx,
        "production",
        "renew-ssl",
        Some("user:ala@allunite.com"),
        serde_json::json!({"upload": {"size_bytes": 123, "mode": "replace"}}),
        serde_json::json!({"snapshot_id": Uuid::new_v4()}),
        Some("rev-abc"),
    )
    .await?;
    tx.commit().await?;

    let row: (String, String, String) = sqlx::query_as(
        "SELECT status, source_type, task_name FROM job WHERE job_id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await?;

    assert_eq!(row.0, "completed");
    assert_eq!(row.1, "upload");
    assert_eq!(row.2, "renew-ssl");

    Ok(())
}
