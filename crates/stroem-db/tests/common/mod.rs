#![allow(dead_code)]

use sqlx::PgPool;
use stroem_db::JobRepo;
use uuid::Uuid;

/// Shared test helper: an isolated, migrated Postgres pool for one test.
pub async fn setup_db() -> PgPool {
    stroem_test_support::test_pool().await
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
