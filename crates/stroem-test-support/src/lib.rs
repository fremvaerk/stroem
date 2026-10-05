use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Fixed key for the Postgres advisory lock guarding template
/// creation/migration. Arbitrary but stable — never reuse this constant
/// for an unrelated lock.
const TEMPLATE_LOCK_KEY: i64 = 0x5354524f_4d544553;

/// Ensures a `stroem_template` database exists at `admin_url`, migrated to
/// the current `stroem_db::migration_fingerprint()`. Safe to call
/// concurrently from multiple processes against the same `admin_url` — the
/// whole check-and-maybe-migrate sequence runs under a Postgres advisory
/// lock, acquired before anything else (including creating the marker
/// table itself, which Postgres does not guarantee is race-free under
/// concurrent `CREATE TABLE IF NOT EXISTS`).
pub async fn ensure_template_migrated(admin_url: &str) -> Result<()> {
    let admin_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!("{admin_url}/postgres"))
        .await
        .context("connect to postgres admin database")?;

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&admin_pool)
        .await
        .context("acquire template migration advisory lock")?;

    let result = ensure_template_migrated_locked(&admin_pool, admin_url).await;

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&admin_pool)
        .await
        .context("release template migration advisory lock")?;

    result
}

async fn ensure_template_migrated_locked(admin_pool: &PgPool, admin_url: &str) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS stroem_test_marker (
            template_name text PRIMARY KEY,
            migration_fingerprint text NOT NULL,
            created_at timestamptz NOT NULL DEFAULT now()
        )",
    )
    .execute(admin_pool)
    .await
    .context("create stroem_test_marker table")?;

    let current_fingerprint = stroem_db::migration_fingerprint();

    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT migration_fingerprint FROM stroem_test_marker WHERE template_name = 'stroem_template'",
    )
    .fetch_optional(admin_pool)
    .await
    .context("read stroem_test_marker")?;

    if existing.as_ref().map(|(fp,)| fp.as_str()) == Some(current_fingerprint.as_str()) {
        return Ok(());
    }

    sqlx::query("DROP DATABASE IF EXISTS stroem_template")
        .execute(admin_pool)
        .await
        .context("drop stale stroem_template")?;
    sqlx::query("CREATE DATABASE stroem_template")
        .execute(admin_pool)
        .await
        .context("create stroem_template")?;

    let template_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{admin_url}/stroem_template"))
        .await
        .context("connect to stroem_template for migration")?;
    stroem_db::run_migrations(&template_pool)
        .await
        .context("migrate stroem_template")?;
    // Load-bearing: Postgres refuses CREATE DATABASE ... TEMPLATE while any
    // session holds the source database open.
    template_pool.close().await;

    sqlx::query(
        "INSERT INTO stroem_test_marker (template_name, migration_fingerprint)
         VALUES ('stroem_template', $1)
         ON CONFLICT (template_name) DO UPDATE SET migration_fingerprint = $1, created_at = now()",
    )
    .bind(&current_fingerprint)
    .execute(admin_pool)
    .await
    .context("write stroem_test_marker")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    #[tokio::test]
    async fn ensure_template_migrated_creates_marker_and_is_idempotent() {
        let container = Postgres::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let admin_url = format!("postgres://postgres:postgres@localhost:{port}");

        super::ensure_template_migrated(&admin_url).await.unwrap();

        let admin_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{admin_url}/postgres"))
            .await
            .unwrap();
        let row: (String,) = sqlx::query_as(
            "SELECT migration_fingerprint FROM stroem_test_marker WHERE template_name = 'stroem_template'",
        )
        .fetch_one(&admin_pool)
        .await
        .unwrap();
        assert_eq!(row.0, stroem_db::migration_fingerprint());

        // Idempotent: calling again against an already-current template must
        // not error (and must not try to recreate it).
        super::ensure_template_migrated(&admin_url).await.unwrap();
    }

    #[tokio::test]
    async fn ensure_template_migrated_remigrates_on_fingerprint_mismatch() {
        let container = Postgres::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let admin_url = format!("postgres://postgres:postgres@localhost:{port}");
        let admin_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{admin_url}/postgres"))
            .await
            .unwrap();

        super::ensure_template_migrated(&admin_url).await.unwrap();

        // Simulate a stale template from a different migration set (e.g.
        // switching branches between runs against a hand-started shared
        // server) by poking a wrong fingerprint directly into the marker.
        sqlx::query(
            "UPDATE stroem_test_marker SET migration_fingerprint = 'stale-fingerprint' WHERE template_name = 'stroem_template'",
        )
        .execute(&admin_pool)
        .await
        .unwrap();

        super::ensure_template_migrated(&admin_url).await.unwrap();

        let row: (String,) = sqlx::query_as(
            "SELECT migration_fingerprint FROM stroem_test_marker WHERE template_name = 'stroem_template'",
        )
        .fetch_one(&admin_pool)
        .await
        .unwrap();
        assert_eq!(
            row.0,
            stroem_db::migration_fingerprint(),
            "a stale fingerprint must trigger re-migration, not be served forever"
        );
    }

    #[tokio::test]
    async fn ensure_template_migrated_serializes_concurrent_callers() {
        let container = Postgres::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let admin_url = format!("postgres://postgres:postgres@localhost:{port}");

        // Two "test binaries" racing to migrate the same fresh template at
        // once — this is exactly the scenario the advisory lock exists for.
        let (a, b) = tokio::join!(
            super::ensure_template_migrated(&admin_url),
            super::ensure_template_migrated(&admin_url),
        );
        a.unwrap();
        b.unwrap();

        let admin_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{admin_url}/postgres"))
            .await
            .unwrap();
        let count: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM stroem_test_marker WHERE template_name = 'stroem_template'",
        )
        .fetch_one(&admin_pool)
        .await
        .unwrap();
        assert_eq!(
            count.0, 1,
            "exactly one marker row, no duplicate-create races"
        );
    }
}
