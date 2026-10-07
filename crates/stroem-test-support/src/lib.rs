use std::io::Write;

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
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
///
/// Every container is labelled `stroem.test=true` (what
/// `scripts/test-clean.sh` removes when a crashed or interrupted run leaves
/// one behind) and named after the test binary it serves — see
/// [`container_name`].
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
        .with_label("stroem.test", "true")
        .with_container_name(this_container_name())
}

/// [`container_name`] for a new container of the running test binary.
/// Cargo sets `CARGO_PKG_NAME` for the test processes it runs too, not only
/// at compile time; the executable's own name gives the target.
fn this_container_name() -> String {
    let package = std::env::var("CARGO_PKG_NAME").ok();
    let exe = std::env::current_exe().ok();
    let stem = exe
        .as_deref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str());
    let nonce = Uuid::new_v4().simple().to_string();
    container_name(package.as_deref(), stem, std::process::id(), &nonce[..6])
}

/// `{package}-{target}-{pid}-{nonce}`, e.g.
/// `stroem-server-integration-48213-9f3a1c`; `target` is the test binary
/// without cargo's hash, `unit` for a library's own unit tests. The pid and
/// the nonce keep it unique: Docker refuses a name any existing container
/// has, a crashed or interrupted run's container outlives its process (until
/// `test-clean.sh`), and one process can start several (the tests below).
fn container_name(package: Option<&str>, exe_stem: Option<&str>, pid: u32, nonce: &str) -> String {
    let target = exe_stem.map(|stem| match stem.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            name
        }
        _ => stem,
    });
    // A library's unit-test binary is named after the crate itself.
    let target = match (package, target) {
        (Some(p), Some(t)) if t == p.replace('-', "_") => Some("unit"),
        (_, t) => t,
    };
    let base = [package, target]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("-");
    let base = if base.is_empty() {
        "stroem-test"
    } else {
        &base
    };
    // Docker allows `[a-zA-Z0-9][a-zA-Z0-9_.-]*`; cargo names already fit.
    let name: String = format!("{base}-{pid}-{nonce}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    name.trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

pub struct TestDb {
    pub pool: PgPool,
    pub url: String,
}

struct SharedContainer {
    // Holds the container alive for this binary's process lifetime. A
    // `static` is never dropped, so `ContainerAsync`'s own removal never
    // runs; [`remove_shared_container`] removes it at exit instead.
    container: ContainerAsync<GenericImage>,
    base_url: String,
}

static SHARED: OnceCell<SharedContainer> = OnceCell::const_new();

/// Registers [`remove_shared_container`] to run when this process exits.
/// libc runs exit handlers both when libtest returns from `main` and when it
/// calls `process::exit(101)` after a failed test; a crash, Ctrl-C or
/// `kill -9` skips them, which is what `scripts/test-clean.sh` is for.
fn remove_shared_container_at_exit() {
    // SAFETY: `remove_shared_container` captures nothing and never unwinds.
    if unsafe { libc::atexit(remove_shared_container) } != 0 {
        let _ = writeln!(
            std::io::stderr(),
            "stroem-test-support: could not register the exit handler; \
             the test container will outlive this process"
        );
    }
}

/// Exit handler: removes the shared container and its anonymous data volume
/// (`--volumes`; left behind, those filled the disk). Waits for `docker rm`
/// so the container is gone once the process is — a hung daemon would
/// already have hung `start()`. Must never panic: that aborts the process.
extern "C" fn remove_shared_container() {
    let Some(shared) = SHARED.get() else {
        return;
    };
    let id = shared.container.id();
    let removal = std::process::Command::new("docker")
        .args(["rm", "--force", "--volumes", id])
        .output();
    warn_if_not_removed(id, &removal);
}

/// Prints [`removal_warning`], if any, to stderr. Runs inside an exit
/// handler: `writeln!` with its result discarded, because `eprintln!` panics
/// on a closed stderr.
fn warn_if_not_removed(id: &str, removal: &std::io::Result<std::process::Output>) {
    if let Some(warning) = removal_warning(id, removal) {
        let _ = writeln!(std::io::stderr(), "{warning}");
    }
}

/// What to tell the user when `docker rm` left container `id` behind.
fn removal_warning(id: &str, removal: &std::io::Result<std::process::Output>) -> Option<String> {
    let cause = match removal {
        Ok(out) if out.status.success() => return None,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Older docker CLIs report a missing container even with --force.
            if stderr.contains("No such container") {
                return None;
            }
            format!("`docker rm` failed: {}", stderr.trim())
        }
        Err(err) => format!("`docker` could not be run: {err}"),
    };
    Some(format!(
        "stroem-test-support: test container {id} was not removed at exit \
         ({cause}); remove it with scripts/test-clean.sh"
    ))
}

async fn default_base_url() -> Result<String> {
    let shared = SHARED
        .get_or_try_init(|| async {
            let container = postgres_image()
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
            remove_shared_container_at_exit();
            let base_url = format!("postgres://postgres:postgres@localhost:{port}");
            Ok::<_, anyhow::Error>(SharedContainer {
                container,
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
    // AssertSqlSafe: a database name cannot be a bind parameter; `db_name` is `t_` + a generated UUID.
    sqlx::query(AssertSqlSafe(format!(
        r#"CREATE DATABASE "{db_name}" TEMPLATE stroem_template"#
    )))
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

    use super::container_name;

    #[test]
    fn container_name_is_package_target_pid_nonce() {
        assert_eq!(
            container_name(
                Some("stroem-server"),
                Some("integration-0123456789abcdef"),
                48213,
                "9f3a1c"
            ),
            "stroem-server-integration-48213-9f3a1c"
        );
    }

    #[test]
    fn container_name_calls_a_libs_own_test_binary_unit() {
        assert_eq!(
            container_name(
                Some("stroem-test-support"),
                Some("stroem_test_support-0123456789abcdef"),
                7,
                "abc123"
            ),
            "stroem-test-support-unit-7-abc123"
        );
    }

    #[test]
    fn container_name_keeps_a_stem_without_cargos_hash() {
        // Not 16 hex digits after the last dash: part of the name.
        assert_eq!(
            container_name(Some("stroem-cli"), Some("stroem-api"), 1, "n"),
            "stroem-cli-stroem-api-1-n"
        );
    }

    #[test]
    fn container_name_falls_back_without_cargo_or_exe() {
        assert_eq!(
            container_name(None, None, 42, "beef00"),
            "stroem-test-42-beef00"
        );
        assert_eq!(
            container_name(None, Some("integration-0123456789abcdef"), 42, "n"),
            "integration-42-n"
        );
    }

    #[test]
    fn container_name_replaces_what_docker_rejects() {
        assert_eq!(
            container_name(None, Some("_my test+bin"), 3, "n"),
            "my-test-bin-3-n"
        );
    }

    /// Makes [`exit_child`] run; without it the test is a no-op.
    const EXIT_CHILD: &str = "STROEM_TEST_SUPPORT_EXIT_CHILD";
    const CONTAINER_ID_MARKER: &str = "SHARED_CONTAINER_ID=";

    /// The child process of the exit tests below: starts this binary's
    /// shared container, prints its id, then passes or fails as told.
    #[tokio::test]
    #[ignore = "run as a child process by the shared-container exit tests"]
    async fn exit_child() {
        let Ok(outcome) = std::env::var(EXIT_CHILD) else {
            return;
        };
        super::test_pool().await;
        let id = super::SHARED
            .get()
            .expect("shared container")
            .container
            .id();
        println!("{CONTAINER_ID_MARKER}{id}");
        assert_eq!(outcome, "pass", "this child was told to fail");
    }

    /// Runs [`exit_child`] in a new process of this test binary and returns
    /// whether it passed and the id of the container it started.
    fn run_exit_child(outcome: &str, env: &[(&str, &str)]) -> (bool, String) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "tests::exit_child", "--nocapture"])
            .env(EXIT_CHILD, outcome)
            .env_remove("TEST_DATABASE_URL")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let id = stdout
            .split(CONTAINER_ID_MARKER)
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| {
                panic!(
                    "the child printed no container id\nstdout:\n{stdout}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        (output.status.success(), id.to_string())
    }

    fn container_exists(id: &str) -> bool {
        std::process::Command::new("docker")
            .args(["container", "inspect", id])
            .output()
            .unwrap()
            .status
            .success()
    }

    /// A finished `docker rm` with exit code `code` and stderr `stderr`.
    fn docker_rm_output(code: i32, stderr: &str) -> std::io::Result<std::process::Output> {
        use std::os::unix::process::ExitStatusExt;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn removal_warning_is_silent_when_the_container_is_gone() {
        assert_eq!(
            super::removal_warning("abc", &docker_rm_output(0, "")),
            None
        );
        // Older docker CLIs report a missing container even with --force.
        let gone = docker_rm_output(1, "Error response from daemon: No such container: abc\n");
        assert_eq!(super::removal_warning("abc", &gone), None);
    }

    #[test]
    fn removal_warning_names_the_container_and_the_cause_when_docker_rm_fails() {
        let failed = docker_rm_output(1, "Cannot connect to the Docker daemon\n");
        let warning = super::removal_warning("abc", &failed).expect("a warning");
        assert!(warning.contains("abc"), "{warning}");
        assert!(
            warning.contains("Cannot connect to the Docker daemon"),
            "{warning}"
        );
        assert!(warning.contains("scripts/test-clean.sh"), "{warning}");
    }

    #[test]
    fn removal_warning_names_the_container_and_the_cause_when_docker_cannot_run() {
        let not_found = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        let warning = super::removal_warning("abc", &not_found).expect("a warning");
        assert!(warning.contains("abc"), "{warning}");
        assert!(warning.contains("not found"), "{warning}");
        assert!(warning.contains("scripts/test-clean.sh"), "{warning}");
    }

    #[test]
    fn shared_container_is_removed_when_its_process_exits() {
        for outcome in ["pass", "fail"] {
            let (passed, id) = run_exit_child(outcome, &[]);
            assert_eq!(passed, outcome == "pass", "child told to {outcome}");
            assert!(
                !container_exists(&id),
                "container {id} outlived its process (child told to {outcome})"
            );
        }
    }

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
