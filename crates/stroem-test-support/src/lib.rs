use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::core::{ImageExt, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// The test Postgres container: `postgres:11-alpine`, user, password and
/// database all `postgres`, started with `-c fsync=off`. A plain
/// `GenericImage` equivalent of the `testcontainers-modules` 0.15 `Postgres`
/// module's defaults (that crate does not support `testcontainers` 0.28).
///
/// Ready once "database system is ready to accept connections" has appeared
/// on stderr and on stdout — the module's condition, kept as is. The
/// entrypoint's temporary init server (Unix socket only) can satisfy both
/// just before the real server starts; sqlx's pool connect retries a refused
/// TCP connection, which covers that gap.
fn postgres_image() -> ContainerRequest<GenericImage> {
    const READY: &str = "database system is ready to accept connections";
    GenericImage::new("postgres", "11-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(READY))
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_env_var("POSTGRES_DB", "postgres")
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_cmd(["-c", "fsync=off"])
}

pub struct TestDb {
    pub pool: PgPool,
    pub url: String,
}

struct SharedContainer {
    // Kept only to hold the container alive for this binary's process
    // lifetime; never read directly. `static` values are never dropped at
    // process exit — this container's eventual removal is
    // `scripts/test-clean.sh`'s job, not this field's, and that's by
    // design (see the design spec § 3.1).
    _container: ContainerAsync<GenericImage>,
    base_url: String,
}

static SHARED: OnceCell<SharedContainer> = OnceCell::const_new();

async fn default_base_url() -> Result<String> {
    let shared = SHARED
        .get_or_try_init(|| async {
            let container = postgres_image()
                .with_label("stroem.test", "true")
                // `postgres_image()`'s own command is `-c fsync=off`;
                // `.with_cmd` REPLACES it rather than appending, so
                // fsync=off must be restated here or every per-test
                // CREATE DATABASE ... TEMPLATE pays for a real fsync'd file
                // copy.
                .with_cmd(["-c", "fsync=off", "-c", "max_connections=200"])
                .start()
                .await
                .context("start per-binary postgres container")?;
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .context("get postgres container port")?;
            let base_url = format!("postgres://postgres:postgres@localhost:{port}");
            Ok::<_, anyhow::Error>(SharedContainer {
                _container: container,
                base_url,
            })
        })
        .await?;
    Ok(shared.base_url.clone())
}

async fn resolve_base_url() -> Result<String> {
    if let Ok(url) = std::env::var("TEST_DATABASE_URL") {
        return Ok(url);
    }
    default_base_url().await
}

/// Returns a fresh, migrated, isolated Postgres database for one test —
/// both a ready-to-use pool and its raw connection URL (several existing
/// call sites need the URL to build a real server's `DbConfig`, not just
/// a pool).
pub async fn test_db() -> TestDb {
    let admin_url = resolve_base_url()
        .await
        .expect("resolve test postgres base url");
    ensure_template_migrated(&admin_url)
        .await
        .expect("ensure stroem_template is migrated");

    let db_name = format!("t_{}", Uuid::new_v4().simple());
    let admin_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!("{admin_url}/postgres"))
        .await
        .expect("connect to postgres admin database");
    sqlx::query(&format!(
        r#"CREATE DATABASE "{db_name}" TEMPLATE stroem_template"#
    ))
    .execute(&admin_pool)
    .await
    .expect("create isolated test database");

    let url = format!("{admin_url}/{db_name}");
    let pool = PgPoolOptions::new()
        // Matches the per-test pool size every pre-migration setup_db()
        // helper in this codebase already used — a test that deliberately
        // holds one connection open while exercising concurrent cascade
        // behavior through others can need more than a couple at once.
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect to isolated test database");

    TestDb { pool, url }
}

/// Convenience wrapper for call sites that only need the pool.
pub async fn test_pool() -> PgPool {
    test_db().await.pool
}

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

    // Clear the marker BEFORE dropping, inside the lock: if this rebuild is
    // interrupted (Ctrl-C, a killed container, a migration that fails on an
    // edited older file), the marker must never be left naming a fingerprint
    // the template no longer matches. A later caller presenting that SAME
    // fingerprint again (e.g. switching back to the branch that originally
    // wrote it) must see "no marker" and rebuild, never a false match
    // against a dropped or half-migrated template.
    sqlx::query("DELETE FROM stroem_test_marker WHERE template_name = 'stroem_template'")
        .execute(admin_pool)
        .await
        .context("clear stale stroem_test_marker before rebuilding template")?;

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

    #[tokio::test]
    async fn test_db_returns_isolated_databases() {
        // Concurrent, not sequential: two tests in one binary calling test_db()
        // at the same time is the normal case this design has to handle,
        // including both hitting ensure_template_migrated's advisory lock at
        // once.
        let (a, b) = tokio::join!(super::test_db(), super::test_db());
        assert_ne!(a.url, b.url, "each call gets its own database");

        sqlx::query("CREATE TABLE marker (n int)")
            .execute(&a.pool)
            .await
            .unwrap();
        // b's database must not see a's table — they're genuinely separate.
        let err = sqlx::query("SELECT * FROM marker")
            .execute(&b.pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("marker") || err.as_database_error().is_some());
    }

    #[tokio::test]
    async fn test_pool_disables_fsync_for_speed() {
        // `postgres_image()` defaults to `-c fsync=off` (its own
        // command); `.with_cmd` in `default_base_url`
        // REPLACES that default rather than appending to it, so without
        // explicitly re-adding `fsync=off` here, every per-test
        // `CREATE DATABASE ... TEMPLATE` pays for a real fsync'd file copy
        // plus a WAL flush per test commit — exactly the cost this shared
        // per-binary container was meant to avoid paying hundreds of times.
        let pool = super::test_pool().await;
        let fsync: (String,) = sqlx::query_as("SHOW fsync").fetch_one(&pool).await.unwrap();
        assert_eq!(
            fsync.0, "off",
            "the shared container must run with fsync off"
        );
    }

    #[tokio::test]
    async fn test_pool_is_already_migrated() {
        let pool = super::test_pool().await;
        // The `job` table only exists if migrations actually ran.
        sqlx::query("SELECT 1 FROM job LIMIT 0")
            .execute(&pool)
            .await
            .expect("job table should exist after migration");
    }

    #[tokio::test]
    async fn ensure_template_migrated_creates_marker_and_is_idempotent() {
        let container = super::postgres_image().start().await.unwrap();
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
        let container = super::postgres_image().start().await.unwrap();
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
    async fn ensure_template_migrated_clears_the_marker_before_attempting_the_drop() {
        let container = super::postgres_image().start().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let admin_url = format!("postgres://postgres:postgres@localhost:{port}");
        let admin_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{admin_url}/postgres"))
            .await
            .unwrap();

        super::ensure_template_migrated(&admin_url).await.unwrap();

        // Force a fingerprint mismatch.
        sqlx::query(
            "UPDATE stroem_test_marker SET migration_fingerprint = 'stale-fingerprint' WHERE template_name = 'stroem_template'",
        )
        .execute(&admin_pool)
        .await
        .unwrap();

        // Hold stroem_template open so the mismatch handler's DROP DATABASE
        // fails partway through — simulating a rebuild interrupted by a
        // Ctrl-C, a killed container, or a migration that fails on an
        // edited older file (the exact scenario this test guards against).
        let holder = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&format!("{admin_url}/stroem_template"))
            .await
            .unwrap();
        sqlx::query("SELECT 1").execute(&holder).await.unwrap();

        let result = super::ensure_template_migrated(&admin_url).await;
        assert!(
            result.is_err(),
            "the held connection should make the DROP DATABASE fail"
        );

        // Whatever this interrupted attempt leaves behind, it must not be a
        // marker row naming a fingerprint the template no longer matches —
        // a later caller presenting that SAME fingerprint again (e.g.
        // switching back to the branch that originally wrote it) must never
        // see a false match against a template this attempt has already
        // dropped or is about to drop.
        let existing: Option<(String,)> = sqlx::query_as(
            "SELECT migration_fingerprint FROM stroem_test_marker WHERE template_name = 'stroem_template'",
        )
        .fetch_optional(&admin_pool)
        .await
        .unwrap();
        assert!(
            existing.is_none(),
            "an interrupted rebuild must leave no marker row, not a stale one: {existing:?}"
        );

        holder.close().await;
    }

    #[tokio::test]
    async fn ensure_template_migrated_serializes_concurrent_callers() {
        let container = super::postgres_image().start().await.unwrap();
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
