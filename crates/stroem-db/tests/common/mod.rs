#![allow(dead_code)]

use sqlx::PgPool;
use stroem_db::{run_migrations, JobRepo};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

/// Shared test helper: spin up a Postgres container and run migrations.
///
/// Leaks the container so callers don't have to thread it through their tests.
/// Each call yields a fresh database, so tests stay isolated.
pub async fn setup_db() -> PgPool {
    let container = Postgres::default()
        .start()
        .await
        .expect("postgres container should start");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port should be available");
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&url)
        .await
        .expect("connect to test postgres");
    run_migrations(&pool)
        .await
        .expect("run migrations on test postgres");
    // Keep the container alive for the duration of the test process.
    Box::leak(Box::new(container));
    pool
}

/// Create a minimal job row so artifact tests have a valid FK target.
pub async fn create_job(pool: &PgPool, workspace: &str, task_name: &str) -> Uuid {
    JobRepo::create(
        pool,
        workspace,
        task_name,
        "distributed",
        None,
        "user",
        None,
        None,
        None,
    )
    .await
    .expect("create test job")
}
