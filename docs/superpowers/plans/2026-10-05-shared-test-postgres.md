# Shared Test Postgres Container Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut testcontainers Postgres container count from one-per-test (hundreds, across a full `cargo test --workspace` run) to one-per-test-*binary* (~27-30), by centralizing container/database bootstrap in a new `stroem-test-support` crate, and give humans a cleanup command for the containers that still accumulate.

**Architecture:** One new dev-only crate (`stroem-test-support`) owns a per-binary `OnceCell`-guarded Postgres container, a migrated `stroem_template` database (tracked via a marker table in the `postgres` admin database, guarded by a Postgres advisory lock), and per-test isolation via `CREATE DATABASE … TEMPLATE …`. Every existing test file's inline `Postgres::default().start()` bootstrap is replaced with a call into this crate. A new `scripts/test-clean.sh` gives humans an on-demand way to sweep up accumulated containers — no automatic reaper, no wrapper script, no process/signal handling of any kind.

**Tech Stack:** Rust, `testcontainers` 0.27 / `testcontainers-modules` 0.15 (postgres), `sqlx` 0.8, `tokio`, `anyhow`, `uuid`, `sha2` (already a workspace dependency).

**Spec:** `docs/superpowers/specs/2026-10-05-shared-test-postgres-design.md` (revision 13, Codex MERGE).

## Global Constraints

- `postgres:11-alpine` is the image used everywhere (matches the spec and today's existing tests) — never swap in a different tag.
- Every container this plan creates carries `--label stroem.test=true` — `scripts/test-clean.sh` finds containers by this label alone, never the generic `org.testcontainers.managed-by=testcontainers` label.
- No `Box::leak`, no `static` container handle expected to `Drop` at process exit, anywhere. The per-binary container's non-removal is accepted and documented, not hidden behind a false cleanup claim.
- `ensure_template_migrated`'s advisory lock is acquired **before** `CREATE TABLE IF NOT EXISTS stroem_test_marker` and held through the entire check-migrate-write sequence (spec revision 12/13 — acquiring it later leaves a real race on table creation itself).
- `test_db()`'s own pool uses `max_connections(2)` — never reuse `stroem_db::create_pool`'s production-sized defaults (5–20) for test connections.
- The per-binary default container is started with `-c max_connections=200` (raised from Postgres 11's stock 100). This is automatic only for the default path; the optional manual `TEST_DATABASE_URL` path requires the human to pass the same flag themselves (spec § 3.1) — never claim the crate can configure a server it didn't start.
- `migration_fingerprint()` and `run_migrations()` must read the exact same compile-time `sqlx::migrate!("./migrations")` data — never construct a second, independent `Migrator` anywhere.
- No wrapper script, no `trap`, no process groups, no signal handling. If any task's implementation starts reaching for these, stop — that mechanism was deliberately cut from the spec after nine review rounds.
- `cargo fmt --check --all` and `cargo clippy --workspace -- -D warnings` must pass after every task that touches `.rs` files (per this repo's CLAUDE.md).

## Review Focus

- **A second test binary racing the first one's template migration on a hand-started shared container** (the optional `TEST_DATABASE_URL` path) — expect no error, no duplicate-migration failure, both binaries proceed once the lock is free. Task 2's test must actually spawn two concurrent callers, not just call the function twice sequentially from one task.
- **A stale `stroem_template` from a previous run of a *different* migration set** (e.g. switching branches between runs against a hand-started shared container) — expect `ensure_template_migrated` to detect the fingerprint mismatch and re-migrate, not silently serve a wrong-schema template forever.
- **Two concurrent `test_db()` calls within one binary** getting genuinely distinct, independently-usable databases — expect no collision on the `t_<uuid>` name and no cross-test visibility of each other's rows.
- **`scripts/test-clean.sh` run with zero matching containers** (clean host) — expect a clear "nothing to remove" message, not an error or a confusing empty table.
- **A test file that still has a leftover inline `Postgres::default()` call after the migration tasks** — expect the final verification task's `rg` sweep to catch it, not silent reliance on "I think I got them all."

---

## Task 1: `stroem_db::migration_fingerprint()`

**Files:**
- Modify: `crates/stroem-db/src/pool.rs`
- Modify: `crates/stroem-db/src/lib.rs`
- Modify: `crates/stroem-db/Cargo.toml` (add `sha2.workspace = true` to `[dependencies]` — it's already a workspace dependency, just not yet used by this crate)

**Interfaces:**
- Produces: `pub fn migration_fingerprint() -> String` in `stroem_db` — a hex-encoded SHA-256 over every embedded migration's checksum, concatenated in order. Later tasks (`stroem-test-support`) call this directly; never re-derive it independently.

- [ ] **Step 1: Write the failing test**

Add to `crates/stroem-db/src/pool.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p stroem-db migration_fingerprint_is_deterministic_and_nonempty`
Expected: FAIL with `cannot find function 'migration_fingerprint' in this scope`

- [ ] **Step 3: Implement `migration_fingerprint()`**

Add to `crates/stroem-db/src/pool.rs` (above the `#[cfg(test)]` block):

```rust
use sha2::{Digest, Sha256};

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
    format!("{:x}", hasher.finalize())
}
```

Add `sha2.workspace = true` to `crates/stroem-db/Cargo.toml`'s `[dependencies]` section.

- [ ] **Step 4: Export it from `lib.rs`**

In `crates/stroem-db/src/lib.rs`, change:

```rust
pub use pool::{create_pool, run_migrations};
```

to:

```rust
pub use pool::{create_pool, migration_fingerprint, run_migrations};
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p stroem-db migration_fingerprint_is_deterministic_and_nonempty`
Expected: PASS

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt -p stroem-db
cargo clippy -p stroem-db -- -D warnings
git add crates/stroem-db/src/pool.rs crates/stroem-db/src/lib.rs crates/stroem-db/Cargo.toml
git commit -m "feat(stroem-db): add migration_fingerprint for test infra"
```

---

## Task 2: `stroem-test-support` crate — `ensure_template_migrated`

**Files:**
- Create: `crates/stroem-test-support/Cargo.toml`
- Create: `crates/stroem-test-support/src/lib.rs`
- Modify: `Cargo.toml` (workspace root — add `crates/stroem-test-support` to `members`, add `stroem-test-support = { path = "crates/stroem-test-support" }` to `[workspace.dependencies]`)

**Interfaces:**
- Consumes: `stroem_db::{run_migrations, migration_fingerprint}` (Task 1).
- Produces: `pub async fn ensure_template_migrated(admin_url: &str) -> anyhow::Result<()>` — `admin_url` is a base URL with no database suffix (e.g. `postgres://postgres:postgres@localhost:5432`); the function appends `/postgres` and `/stroem_template` itself. Task 3 calls this directly.

- [ ] **Step 1: Create the crate skeleton**

`crates/stroem-test-support/Cargo.toml`:

```toml
[package]
name = "stroem-test-support"
version.workspace = true
edition.workspace = true
publish = false

[dependencies]
anyhow.workspace = true
sqlx.workspace = true
stroem-db.workspace = true
tokio.workspace = true
uuid.workspace = true
testcontainers = { version = "0.27" }
testcontainers-modules = { version = "0.15", features = ["postgres"] }
```

In the workspace root `Cargo.toml`:
- Add `"crates/stroem-test-support",` to `[workspace] members` (after `"crates/stroem-e2e",`).
- Add `stroem-test-support = { path = "crates/stroem-test-support" }` next to the other `stroem-*` path entries.

- [ ] **Step 2: Write the failing test**

`crates/stroem-test-support/src/lib.rs` (test module only for now):

```rust
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
        assert_eq!(count.0, 1, "exactly one marker row, no duplicate-create races");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p stroem-test-support`
Expected: FAIL to compile — `ensure_template_migrated` doesn't exist yet.

- [ ] **Step 4: Implement `ensure_template_migrated`**

Add above the `#[cfg(test)]` module in `crates/stroem-test-support/src/lib.rs`:

```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p stroem-test-support`
Expected: PASS (both tests)

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt -p stroem-test-support
cargo clippy -p stroem-test-support -- -D warnings
git add Cargo.toml crates/stroem-test-support
git commit -m "feat(stroem-test-support): add ensure_template_migrated"
```

---

## Task 3: `stroem-test-support` — `test_db()` / `test_pool()`

**Files:**
- Modify: `crates/stroem-test-support/src/lib.rs`

**Interfaces:**
- Consumes: `ensure_template_migrated` (Task 2).
- Produces: `pub struct TestDb { pub pool: sqlx::PgPool, pub url: String }`, `pub async fn test_db() -> TestDb`, `pub async fn test_pool() -> sqlx::PgPool`. Every later migration task (5–10) calls one of these two functions exclusively — no other way of getting a test database.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/stroem-test-support/src/lib.rs`:

```rust
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
async fn test_pool_is_already_migrated() {
    let pool = super::test_pool().await;
    // The `job` table only exists if migrations actually ran.
    sqlx::query("SELECT 1 FROM job LIMIT 0")
        .execute(&pool)
        .await
        .expect("job table should exist after migration");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p stroem-test-support test_db_returns_isolated_databases test_pool_is_already_migrated`
Expected: FAIL to compile — `test_db`/`test_pool` don't exist yet.

- [ ] **Step 3: Implement `TestDb`, `test_db()`, `test_pool()`**

Add above the `#[cfg(test)]` module:

```rust
use testcontainers::core::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;
use uuid::Uuid;

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
    _container: ContainerAsync<Postgres>,
    base_url: String,
}

static SHARED: OnceCell<SharedContainer> = OnceCell::const_new();

async fn default_base_url() -> Result<String> {
    let shared = SHARED
        .get_or_try_init(|| async {
            let container = Postgres::default()
                .with_label("stroem.test", "true")
                .with_cmd(["-c", "max_connections=200"])
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
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect to isolated test database");

    TestDb { pool, url }
}

/// Convenience wrapper for call sites that only need the pool.
pub async fn test_pool() -> PgPool {
    test_db().await.pool
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p stroem-test-support`
Expected: PASS (all four tests in the crate)

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt -p stroem-test-support
cargo clippy -p stroem-test-support -- -D warnings
git add crates/stroem-test-support/src/lib.rs
git commit -m "feat(stroem-test-support): add test_db/test_pool"
```

---

## Task 4: `scripts/test-clean.sh`

**Files:**
- Create: `scripts/test-clean.sh`

**Interfaces:**
- Consumes: nothing from earlier tasks — pure shell, filters on the `stroem.test=true` label every container from Tasks 2–3 carries.
- Produces: a human-run command. No other task calls this script.

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
set -euo pipefail

MAX_AGE_SECONDS=300
if [ "${1:-}" = "--max-age" ]; then
    case "$2" in
        *h) MAX_AGE_SECONDS=$(( ${2%h} * 3600 )) ;;
        *m) MAX_AGE_SECONDS=$(( ${2%m} * 60 )) ;;
        *s) MAX_AGE_SECONDS=$(( ${2%s} )) ;;
        *) echo "usage: $0 [--max-age <Ns|Nm|Nh>]" >&2; exit 1 ;;
    esac
fi

now=$(date -u +%s)
removed=0
skipped=0

for id in $(docker ps -aq --filter "label=stroem.test=true"); do
    started=$(docker inspect --format '{{.State.StartedAt}}' "$id")
    # Docker's timestamp includes nanoseconds; date(1) wants seconds precision.
    started_epoch=$(date -u -d "${started%.*}Z" +%s 2>/dev/null \
        || date -u -j -f "%Y-%m-%dT%H:%M:%S" "${started%.*}" +%s)
    age=$(( now - started_epoch ))
    name=$(docker inspect --format '{{.Name}}' "$id" | sed 's#^/##')

    if [ "$age" -ge "$MAX_AGE_SECONDS" ]; then
        docker rm -f "$id" >/dev/null
        echo "removed $name (age ${age}s)"
        removed=$((removed + 1))
    else
        echo "skipped $name (age ${age}s, below --max-age ${MAX_AGE_SECONDS}s)"
        skipped=$((skipped + 1))
    fi
done

if [ "$removed" -eq 0 ] && [ "$skipped" -eq 0 ]; then
    echo "nothing to remove — no containers labelled stroem.test=true"
fi
```

- [ ] **Step 2: Make it executable**

```bash
chmod +x scripts/test-clean.sh
```

- [ ] **Step 3: Manually verify against a real container**

```bash
docker run -d --name test-clean-smoke --label stroem.test=true postgres:11-alpine
./scripts/test-clean.sh --max-age 0s
docker ps -a --filter name=test-clean-smoke   # expect: no output, container removed
```

Expected: the script prints `removed test-clean-smoke (age ...)`, and the follow-up `docker ps` shows nothing.

- [ ] **Step 4: Verify the no-op case**

```bash
./scripts/test-clean.sh
```

Expected: `nothing to remove — no containers labelled stroem.test=true` (assuming no other `stroem.test=true` containers are currently running — check with `docker ps` first if unsure).

- [ ] **Step 5: Commit**

```bash
git add scripts/test-clean.sh
git commit -m "feat(scripts): add test-clean.sh for stroem.test containers"
```

---

## Task 5: Migrate `stroem-db` test call sites

**Files:**
- Modify: `crates/stroem-db/tests/common/mod.rs`
- Modify: `crates/stroem-db/tests/integration_test.rs`
- Modify: `crates/stroem-db/tests/migration_test.rs`
- Modify: `crates/stroem-db/tests/tarball_keep_revisions_test.rs`
- Modify: `crates/stroem-db/tests/job_step_status_tests.rs`
- Modify: `crates/stroem-db/Cargo.toml` (dev-dependencies: remove `testcontainers`/`testcontainers-modules` and the "watchdog" comment above them, which described *this crate's own* direct usage that no longer exists; add `stroem-test-support.workspace = true`)

**Interfaces:**
- Consumes: `stroem_test_support::{test_pool, test_db}` (Tasks 2–3).

All five files currently open with a near-identical pattern (a local `setup_db()`-shaped function that starts its own `Postgres::default()` container, builds a pool, runs migrations). None of these files need `.url` — they only ever use the pool.

- [ ] **Step 1: Confirm nothing in this crate's tests needs a URL**

Run: `grep -n "DbConfig\|\.url" crates/stroem-db/tests/*.rs crates/stroem-db/tests/common/mod.rs`
Expected: no matches (stroem-db's own tests never build a server config) — confirming `test_pool()` alone suffices for all five files.

- [ ] **Step 2: Rewrite `crates/stroem-db/tests/common/mod.rs`**

Replace the whole file's `setup_db` with:

```rust
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
```

(This deletes the `testcontainers`/`testcontainers_modules`/`run_migrations` imports and the `Box::leak` call entirely — `setup_db()`'s signature and every caller stay unchanged.)

- [ ] **Step 3: Migrate the four remaining files**

In each of `integration_test.rs`, `migration_test.rs`, `tarball_keep_revisions_test.rs`, `job_step_status_tests.rs`: find the file's local setup function (shape: `async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> { let container = Postgres::default().start().await?; ... run_migrations(&pool).await?; Ok((pool, container)) }` or similar), and replace its body with:

```rust
async fn setup_db() -> Result<PgPool> {
    Ok(stroem_test_support::test_pool().await)
}
```

Update every call site in that file from `let (pool, _container) = setup_db().await?;` (or whatever the local destructuring pattern is) to `let pool = setup_db().await?;`, and remove the now-unused `testcontainers`/`testcontainers_modules::postgres::Postgres` imports at the top of the file.

- [ ] **Step 4: Update `crates/stroem-db/Cargo.toml`**

Remove:

```toml
# NOTE: the "watchdog" feature is intentionally NOT enabled. Its
# `conquer_once::Lazy` signal-cleanup thread races on concurrent init and panics
# ("entered unreachable code", conquer-once cell.rs) under parallel testcontainer
# startup, flaking CI (testcontainers' own docs note the watchdog "may panic").
# RAII Drop cleans up on normal exit; CI runners are ephemeral.
testcontainers = { version = "0.27" }
testcontainers-modules = { version = "0.15", features = ["postgres"] }
```

Add in its place:

```toml
stroem-test-support.workspace = true
```

- [ ] **Step 5: Run the crate's tests**

Run: `cargo test -p stroem-db`
Expected: PASS, and `docker ps` while the run is in progress shows at most a handful of `stroem.test=true` containers for this crate (not one per test).

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt -p stroem-db
cargo clippy -p stroem-db --all-targets -- -D warnings
git add crates/stroem-db/tests crates/stroem-db/Cargo.toml
git commit -m "refactor(stroem-db): migrate tests to stroem-test-support"
```

---

## Task 6: Migrate `crates/stroem-server/tests/integration_test.rs`

**Files:**
- Modify: `crates/stroem-server/tests/integration_test.rs` (411 tests, ~55 inline `Postgres::default()` call sites — the largest file by far, handled on its own)

**Interfaces:**
- Consumes: `stroem_test_support::test_db` (Task 3) — this file needs `.url` (confirmed in the earlier repo sweep: it builds `DbConfig { url, .. }`).

- [ ] **Step 1: Count the call sites before starting**

Run: `grep -c "Postgres::default()" crates/stroem-server/tests/integration_test.rs`
Expected: `55` (matches the design spec's count — if this has changed, note the new count and adjust Step 4's verification accordingly).

- [ ] **Step 2: Identify the setup function shape(s)**

Run: `grep -n "async fn setup\|Postgres::default()\|DbConfig" crates/stroem-server/tests/integration_test.rs | less`

This file has multiple local setup functions (not exactly one shared `setup()` — some tests have their own variant). Each one follows the same inner shape as `cascade_apply_test.rs`'s:

```rust
async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}
```

or the URL-needing variant (used wherever `DbConfig { url, .. }` is built afterward):

```rust
async fn setup() -> Result<(Router, PgPool, TempDir, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    // ... build config with DbConfig { url, .. }, build Router ...
}
```

- [ ] **Step 3: Replace every occurrence of the bootstrap block**

For every function matching the pool-only shape, replace:

```rust
let container = Postgres::default().start().await?;
let port = container.get_host_port_ipv4(5432).await?;
let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
let pool = create_pool(&url).await?;
run_migrations(&pool).await?;
```

with:

```rust
let test_db = stroem_test_support::test_db().await;
let pool = test_db.pool;
```

— and drop that function's `container` from its return tuple/signature and every call site's destructuring (`let (pool, _container) = ...` → `let pool = ...`).

For every function matching the URL-needing shape, replace the same bootstrap block with:

```rust
let test_db = stroem_test_support::test_db().await;
let pool = test_db.pool.clone();
let url = test_db.url;
```

(`test_db.pool` is consumed by `pool` here; keep `url` for the subsequent `DbConfig { url, .. }` construction, and drop `container` from the return tuple/signature the same way.)

Remove the file's `use testcontainers::runners::AsyncRunner;` and `use testcontainers_modules::postgres::Postgres;` imports once no reference to either remains.

- [ ] **Step 4: Verify every call site is gone**

Run: `grep -c "Postgres::default()" crates/stroem-server/tests/integration_test.rs`
Expected: `0`

- [ ] **Step 5: Run the file's tests**

Run: `cargo test -p stroem-server --test integration_test`
Expected: PASS (all 411 tests)

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt -p stroem-server
cargo clippy -p stroem-server --test integration_test -- -D warnings
git add crates/stroem-server/tests/integration_test.rs
git commit -m "refactor(stroem-server): migrate integration_test.rs to stroem-test-support"
```

---

## Task 7: Migrate remaining `stroem-server` test files needing `test_db()`

**Files:**
- Modify: `crates/stroem-server/tests/artifact_retention_test.rs`
- Modify: `crates/stroem-server/tests/artifact_api_test.rs`
- Modify: `crates/stroem-server/tests/artifact_upload_test.rs`
- Modify: `crates/stroem-server/tests/duration_stats_test.rs`
- Modify: `crates/stroem-server/tests/mcp_artifacts_test.rs`
- Modify: `crates/stroem-server/tests/ha_test.rs`
- Modify: `crates/stroem-server/tests/propagate_to_parent_test.rs`
- Modify: `crates/stroem-server/tests/mcp_test.rs`
- Modify: `crates/stroem-server/tests/oauth_flow_test.rs`
- Modify: `crates/stroem-server/tests/metrics_test.rs`
- Modify: `crates/stroem-server/tests/pin_store_test.rs`
- Modify: `crates/stroem-server/tests/rerun_integration_test.rs`
- Modify: `crates/stroem-server/tests/state_upload_test.rs`
- Modify: `crates/stroem-server/tests/restart_integration_test.rs`

**Interfaces:**
- Consumes: `stroem_test_support::test_db` (Task 3).

Each of these 14 files has exactly one local setup function (one `Postgres::default()` call site each — confirmed by the earlier repo sweep) and needs `.url` to build a `DbConfig`. The local function's *name* and surrounding return type vary per file (`setup()`, `boot() -> Result<Harness>`, etc.) — only the bootstrap lines inside each need to change.

- [ ] **Step 1: Confirm the call-site count per file**

Run: `for f in crates/stroem-server/tests/{artifact_retention,artifact_api,artifact_upload,duration_stats,mcp_artifacts,ha,propagate_to_parent,mcp,oauth_flow,metrics,pin_store,rerun_integration,state_upload,restart_integration}_test.rs; do echo "$(grep -c 'Postgres::default()' "$f") $f"; done`
Expected: `1` for every file listed (if any file shows a different count, re-check that file individually before proceeding — the repo may have changed since the spec's sweep).

- [ ] **Step 2: Migrate each file**

In each file, find the block:

```rust
let container = Postgres::default().start().await?;
let port = container.get_host_port_ipv4(5432).await?;
let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
let pool = create_pool(&url).await?;
run_migrations(&pool).await?;
```

(or equivalent — some files use `create_pool`+`run_migrations` separately, some may differ slightly in variable naming) and replace it with:

```rust
let test_db = stroem_test_support::test_db().await;
let pool = test_db.pool.clone();
let url = test_db.url;
```

Remove that function's `container` from its signature/return type and update its one caller to stop destructuring a container. Remove the file's now-unused `testcontainers`/`testcontainers_modules::postgres::Postgres` imports.

- [ ] **Step 3: Verify every call site is gone**

Run: `for f in crates/stroem-server/tests/{artifact_retention,artifact_api,artifact_upload,duration_stats,mcp_artifacts,ha,propagate_to_parent,mcp,oauth_flow,metrics,pin_store,rerun_integration,state_upload,restart_integration}_test.rs; do grep -c "Postgres::default()" "$f"; done`
Expected: `0` for every file (14 zeroes).

- [ ] **Step 4: Run the affected tests**

Run: `cargo test -p stroem-server --test artifact_retention_test --test artifact_api_test --test artifact_upload_test --test duration_stats_test --test mcp_artifacts_test --test ha_test --test propagate_to_parent_test --test mcp_test --test oauth_flow_test --test metrics_test --test pin_store_test --test rerun_integration_test --test state_upload_test --test restart_integration_test`
Expected: PASS

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt -p stroem-server
cargo clippy -p stroem-server --tests -- -D warnings
git add crates/stroem-server/tests/*.rs
git commit -m "refactor(stroem-server): migrate test_db()-needing test files to stroem-test-support"
```

---

## Task 8: Migrate remaining `stroem-server` pool-only test files, and the shared Cargo.toml dev-dependency swap

**Files:**
- Modify: `crates/stroem-server/tests/orchestrator_test.rs`
- Modify: `crates/stroem-server/tests/cascade_apply_test.rs`
- Modify: `crates/stroem-server/tests/common/pinned.rs` (verify — see Step 1)
- Modify: `crates/stroem-server/Cargo.toml` (dev-dependencies: remove `testcontainers`/`testcontainers-modules` postgres usage and the watchdog comment, add `stroem-test-support.workspace = true` — `testcontainers-modules`'s `minio` feature, used by `s3_integration_test.rs`, is unrelated to Postgres and must stay)

**Interfaces:**
- Consumes: `stroem_test_support::test_pool` (Task 3).

- [ ] **Step 1: Check `common/pinned.rs`'s actual testcontainers usage**

Run: `grep -n "Postgres::default\|testcontainers" crates/stroem-server/tests/common/pinned.rs`

If it only imports `testcontainers` types without calling `Postgres::default()` itself (i.e. it receives an already-built `PgPool` from its caller), no change is needed here — its callers (already migrated in Tasks 6–7) supply the pool. If it does call `Postgres::default()` directly, apply the same transformation as Step 2 below and add it to this task's file list.

- [ ] **Step 2: Migrate `orchestrator_test.rs` and `cascade_apply_test.rs`**

Both have the pool-only shape:

```rust
async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}
```

Replace with:

```rust
async fn setup_db() -> Result<PgPool> {
    Ok(stroem_test_support::test_pool().await)
}
```

Update each file's call sites from `let (pool, _container) = setup_db().await?;` to `let pool = setup_db().await?;`, and remove the now-unused `testcontainers`/`testcontainers_modules::postgres::Postgres` imports.

- [ ] **Step 3: Verify every Postgres-related call site is gone**

Run: `grep -rn "Postgres::default()" crates/stroem-server/tests/orchestrator_test.rs crates/stroem-server/tests/cascade_apply_test.rs crates/stroem-server/tests/common/pinned.rs`
Expected: no matches.

- [ ] **Step 4: Update `crates/stroem-server/Cargo.toml`**

Change:

```toml
# NOTE: the "watchdog" feature is intentionally NOT enabled. Its
# `conquer_once::Lazy` signal-cleanup thread races on concurrent init and panics
# ("entered unreachable code", conquer-once cell.rs) under parallel testcontainer
# startup, flaking CI (testcontainers' own docs note the watchdog "may panic").
# RAII Drop cleans up on normal exit; CI runners are ephemeral.
testcontainers = { version = "0.27" }
testcontainers-modules = { version = "0.15", features = ["postgres", "minio"] }
```

to:

```toml
testcontainers-modules = { version = "0.15", features = ["minio"] }
stroem-test-support.workspace = true
```

(`testcontainers-modules` stays, but only for its `minio` feature, used directly by `s3_integration_test.rs`; the bare `testcontainers` crate dependency is dropped entirely since nothing in `stroem-server`'s own tests calls it anymore.)

- [ ] **Step 5: Run the affected tests**

Run: `cargo test -p stroem-server --test orchestrator_test --test cascade_apply_test`
Expected: PASS

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt -p stroem-server
cargo clippy -p stroem-server --tests -- -D warnings
git add crates/stroem-server/tests/orchestrator_test.rs crates/stroem-server/tests/cascade_apply_test.rs crates/stroem-server/tests/common/pinned.rs crates/stroem-server/Cargo.toml
git commit -m "refactor(stroem-server): migrate remaining pool-only tests, drop direct testcontainers dep"
```

---

## Task 9: Migrate `crates/stroem-e2e/tests/harness.rs`

**Files:**
- Modify: `crates/stroem-e2e/tests/harness.rs`
- Modify: `crates/stroem-e2e/Cargo.toml` (dev-dependencies: remove `testcontainers`/`testcontainers-modules`, add `stroem-test-support.workspace = true`)

**Interfaces:**
- Consumes: `stroem_test_support::test_db` (Task 3) — `TestEnv::setup()` builds a `DbConfig { url, .. }` for a real server, so it needs `.url`.

- [ ] **Step 1: Replace the bootstrap block in `TestEnv::setup()`**

Change:

```rust
// 1. Start Postgres container
let container = Postgres::default().start().await?;
let port = container.get_host_port_ipv4(5432).await?;
let db_url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
let pool = create_pool(&db_url).await?;
run_migrations(&pool).await?;
```

to:

```rust
// 1. Get an isolated, migrated Postgres database
let test_db = stroem_test_support::test_db().await;
let pool = test_db.pool.clone();
let db_url = test_db.url;
```

Remove `container` from `TestEnv`'s struct fields (if it stores the container to keep it alive) and from `setup()`'s return value; remove the file's `testcontainers`/`testcontainers_modules::postgres::Postgres` imports.

- [ ] **Step 2: Update `crates/stroem-e2e/Cargo.toml`**

Change:

```toml
# ... (watchdog comment, if present in this file too)
testcontainers = { version = "0.27" }
testcontainers-modules = { version = "0.15", features = ["postgres"] }
```

to:

```toml
stroem-test-support.workspace = true
```

- [ ] **Step 3: Run the e2e tests**

Run: `cargo test -p stroem-e2e`
Expected: PASS

- [ ] **Step 4: Format, lint, commit**

```bash
cargo fmt -p stroem-e2e
cargo clippy -p stroem-e2e --tests -- -D warnings
git add crates/stroem-e2e/tests/harness.rs crates/stroem-e2e/Cargo.toml
git commit -m "refactor(stroem-e2e): migrate harness.rs to stroem-test-support"
```

---

## Task 10: Migrate `crates/stroem-server/src/settlement/hooks.rs` in-module test

**Files:**
- Modify: `crates/stroem-server/src/settlement/hooks.rs`

**Interfaces:**
- Consumes: `stroem_test_support::test_pool` (Task 3) — this test only needs a pool, confirmed by the existing code (`stroem_db::create_pool(&url)` then direct `JobRepo` calls, no server/`DbConfig` construction).

Note: `stroem-test-support` becomes a dev-dependency of `stroem-server`, which is also where this `src/`-level test lives — Cargo allows a crate's dev-dependencies to be used by its own in-module `#[cfg(test)]` code, so no additional wiring beyond Task 8's Cargo.toml change is needed here.

- [ ] **Step 1: Replace the bootstrap block**

In `hook_chain_depth_counts_hook_links_across_intermediate_task_levels` (and confirm via `grep -n "Postgres::default" crates/stroem-server/src/settlement/hooks.rs` whether any sibling test in the same `mod tests` block has the same pattern — migrate every match found), change:

```rust
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const INTERMEDIATE_LEVELS: usize = 9;
const HOOK_LINKS: usize = 3;

let container = Postgres::default().start().await.unwrap();
let port = container.get_host_port_ipv4(5432).await.unwrap();
let url = format!("postgres://postgres:postgres@localhost:{port}/postgres");
let pool = stroem_db::create_pool(&url).await.unwrap();
stroem_db::run_migrations(&pool).await.unwrap();
```

to:

```rust
const INTERMEDIATE_LEVELS: usize = 9;
const HOOK_LINKS: usize = 3;

let pool = stroem_test_support::test_pool().await;
```

(dropping the two now-unused `use` lines entirely).

- [ ] **Step 2: Verify the call site is gone**

Run: `grep -c "Postgres::default()" crates/stroem-server/src/settlement/hooks.rs`
Expected: `0`

- [ ] **Step 3: Run the test**

Run: `cargo test -p stroem-server --lib hook_chain_depth_counts_hook_links_across_intermediate_task_levels`
Expected: PASS

- [ ] **Step 4: Format, lint, commit**

```bash
cargo fmt -p stroem-server
cargo clippy -p stroem-server --lib -- -D warnings
git add crates/stroem-server/src/settlement/hooks.rs
git commit -m "refactor(stroem-server): migrate hooks.rs in-module test to stroem-test-support"
```

---

## Task 11: Remove dead CI config

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:** none — this task only removes configuration nothing reads.

- [ ] **Step 1: Confirm nothing reads `DATABASE_URL`**

Run: `grep -rn "DATABASE_URL" crates/`
Expected: no matches (confirmed during the design phase; re-confirming here since this is the last chance before deleting the block).

- [ ] **Step 2: Remove the unused service container and env var**

In `.github/workflows/ci.yml`'s `test` job, delete:

```yaml
    services:
      postgres:
        image: postgres:17
        env:
          POSTGRES_USER: stroem
          POSTGRES_PASSWORD: stroem
          POSTGRES_DB: stroem
        ports:
          - 5432:5432
        options: >-
          --health-cmd "pg_isready -U stroem"
          --health-interval 10s
          --health-timeout 5s
          --health-retries 5
    env:
      DATABASE_URL: postgres://stroem:stroem@localhost:5432/stroem
```

leaving the `test` job otherwise unchanged (still `runs-on: ubuntu-latest`, still `cargo test --workspace`).

- [ ] **Step 3: Verify the YAML is still valid**

Run: `cat .github/workflows/ci.yml | python3 -c "import sys, yaml; yaml.safe_load(sys.stdin)"` (or any available YAML validator)
Expected: no error.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: remove unused postgres service container and DATABASE_URL"
```

---

## Task 12: Documentation updates

**Files:**
- Modify: `docs/internal/TODO.md`
- Modify: `CLAUDE.md`

**Interfaces:** none.

- [ ] **Step 1: Close the TODO.md entry**

Find the entry at `docs/internal/TODO.md:333` ("Integration tests start one Postgres container per test"). Change its leading `- [ ]` to `- [x]`, and append: `Implemented via the stroem-test-support crate (one container per test binary, CREATE DATABASE ... TEMPLATE isolation) — see docs/superpowers/specs/2026-10-05-shared-test-postgres-design.md.`

- [ ] **Step 2: Add a CLAUDE.md convention note**

In `CLAUDE.md`'s "Tests" bullet (under Conventions), after the existing sentence about `testcontainers` for Postgres, add: `All DB-backed tests get their pool/URL via stroem-test-support::{test_pool, test_db}` — one Postgres container per test *binary* (via a per-process OnceCell), never per test. Never call testcontainers directly in a new test file.

- [ ] **Step 3: Commit**

```bash
git add docs/internal/TODO.md CLAUDE.md
git commit -m "docs: close testcontainers-per-test TODO, document stroem-test-support convention"
```

---

## Task 13: Final workspace-wide verification

**Files:** none — this task only runs checks.

**Interfaces:** none.

- [ ] **Step 1: Confirm zero remaining direct testcontainers call sites outside `stroem-test-support`**

Run: `rg -l 'Postgres::default\(\)|testcontainers::runners' --type rust | grep -v '^crates/stroem-test-support/'`
Expected: no output (every match from the original repo-wide sweep in Tasks 5–10 has been migrated; `crates/stroem-test-support/src/lib.rs` itself is the one legitimate remaining user).

- [ ] **Step 2: Full workspace build, lint, and test**

```bash
cargo fmt --check --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all PASS.

- [ ] **Step 3: Observe the container-count improvement**

In one terminal:

```bash
watch -n1 'docker ps --filter label=stroem.test=true | wc -l'
```

In another:

```bash
cargo test --workspace
```

Expected: the count stays in the low tens (roughly one per test binary) throughout the run, never climbing into the hundreds the original problem exhibited.

- [ ] **Step 4: Clean up and confirm the cleaner works end to end**

```bash
./scripts/test-clean.sh --max-age 0s
docker ps -a --filter label=stroem.test=true
```

Expected: the second command shows no containers.

- [ ] **Step 5: Final commit (if any formatting fixes were needed in Step 2)**

```bash
git add -A
git commit -m "chore: final fmt/clippy fixes after stroem-test-support migration" --allow-empty
```
