use anyhow::Result;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;

pub async fn create_pool(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .min_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Run database migrations (includes migration 027_step_retry)
pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// A deterministic fingerprint over every embedded migration's checksum —
/// changes whenever a migration is added OR an existing one is edited
/// (unlike comparing the highest version number alone). Reads the exact
/// same compile-time `sqlx::migrate!` data as `run_migrations`, so the two
/// can never drift apart.
pub fn migration_fingerprint() -> String {
    let migrator = sqlx::migrate!("./migrations");
    let mut hasher = Sha256::new();
    for migration in migrator.migrations.iter() {
        hasher.update(migration.checksum.as_ref());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_fingerprint_is_deterministic_and_nonempty() {
        let a = migration_fingerprint();
        let b = migration_fingerprint();
        assert_eq!(a, b, "fingerprint must be stable across calls");
        assert!(!a.is_empty());
    }
}
