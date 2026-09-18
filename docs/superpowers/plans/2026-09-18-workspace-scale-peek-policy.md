# Workspace Scale — Peek Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make workspace refresh survive 50+ git repos: upgrade git2, fix two retention bugs, and implement the approved § 4 peek policy (skip on peek failure, shared single-writer availability, bounded loads, watchdog) plus § 10 steps 1–2.

**Architecture:** Workspace state splits three ways — a long-held *execution mutex*, a microsecond `Availability` mutex driven by one pure `transition()` function, and a published `Arc` snapshot every reader uses. Every load runs as *worker* (`spawn_blocking`, may hang/panic) → *finalizer* (detached task owning guard + permit, the only writer) → *observer* (may give up waiting). Cooperative deadlines (`LoadBudget`) thread through git2 callbacks, the YAML scan, `sops` and `vals`.

**Tech Stack:** Rust, tokio 1.52, git2 0.21 (libgit2 1.9.4), sqlx/Postgres, Tera, metrics-exporter-prometheus, Helm.

**Spec:** `docs/superpowers/specs/2026-09-17-workspace-scale-design.md` (rev 6, § 4 approved by Codex, thread `01a0afcd`). Read §§ 4, 4.10 and 10 before starting any task.

## Global Constraints

- Implement **§ 4 only as scoped by § 4.10**. Do NOT implement: peer-dispatch-after-admission, notification origin tagging, startup non-blocking admission, signal-to-exit shutdown bound, worker cache eviction (§ 6), tarball single-flight (§ 7), listener-before-load (§ 3 option 2).
- K = 5, P = 30 s, L = 300 s, backoff cap = 900 s, `MAX_CONCURRENT_WORKSPACE_LOADS` = 8, libgit2 connect timeout = 10 000 ms, read timeout = 60 000 ms.
- Watcher load: guard + permit held by the finalizer until **real** completion (user decision).
- External callers (`reload`, `reload_for_api`) `try_lock` the execution mutex — never `.lock().await` — take **no** permit, have **no** watchdog, but DO run through the detached finalizer and `apply_load_result`.
- Startup keeps its blocking `JoinSet` + `acquire_owned().await`.
- `Busy`/`Saturated` admission is never a state transition.
- Runtime floor: `worker_threads = max(4, available_parallelism)`.
- Error handling: `anyhow::Result` + `.context(..)`. Logging: `tracing`.
- Every task ends green on: `cargo fmt --check --all` and `cargo clippy --workspace --all-targets -- -D warnings` plus the task's own tests.
- Commits: conventional prefixes (`feat:`, `fix:`, `docs:`, `chore:`), **no AI co-author trailer** (user's global rule).
- Fresh worktree needs `mkdir -p crates/stroem-server/static` (rust-embed); it is gitignored.
- Integration tests need Docker (testcontainers).

## Deviations from the spec (decided while planning)

1. `LoadBudget` carries only a deadline; P and L live in `ReloadSettings` and produce deadlines. Spec wording "a deadline plus P/L" is satisfied functionally.
2. `sops`/`vals` kill-at-deadline is tested through the shared `run_with_deadline` helper with `sleep`, not fake binaries on `PATH` (PATH is process-global; tests run in parallel).
3. The § 11 E2E check ("job runs while peek fails") is covered by a manager-level integration test in Task 12: the E2E harness does not start watchers and the folder poll interval (30 s) makes a poll-driven E2E impractically slow.
4. Only **External** callers stamp `reload_state.last_completed`, preserving today's API cooldown exactly (watcher loads never stamped it).
5. Knobs are a server-level `workspace_reload:` section (not per-workspace), avoiding the `WorkspaceSourceDef` tagged-enum env-override limitation.

## File Map

| File | Responsibility |
|---|---|
| `crates/stroem-common/src/budget.rs` (new) | `LoadBudget`, `DeadlineExceeded`, `is_deadline_exceeded`, `run_with_deadline` |
| `crates/stroem-common/src/{sops,template,workspace_loader}.rs`, `models/workflow.rs` | `*_with(.., &LoadBudget)` variants; old fns delegate unbounded |
| `crates/stroem-db/src/repos/job.rs` | `JobRepo::tarball_keep_revisions` |
| `crates/stroem-server/src/runtime.rs` (new) | `worker_threads()` floor |
| `crates/stroem-server/src/workspace/availability.rs` (new) | pure state machine + `ReloadSettings` |
| `crates/stroem-server/src/workspace/source.rs` (new) | `WorkspaceSource` trait, `Peek`, `LoadOutcome` |
| `crates/stroem-server/src/workspace/entry.rs` (new) | `WorkspaceEntry`, `Published`, `ReloadState`, `apply_load_result` |
| `crates/stroem-server/src/workspace/lifecycle.rs` (new) | `ReloadBusy`, `ReloadNotifier`, `spawn_load`, `spawn_peek`, `jitter_offset` |
| `crates/stroem-server/src/workspace/watcher.rs` (new) | watcher loop, peek observer, watcher admission |
| `crates/stroem-server/src/workspace/test_support.rs` (new, `cfg(test)`) | `TestSource` |
| `crates/stroem-server/src/workspace/{mod,git,folder}.rs` | manager + sources adapted |
| `crates/stroem-server/src/{config,main,recovery,events,scheduler,metrics}.rs`, `web/hooks.rs` | wiring |
| `helm/stroem/values.yaml`, docs, `ui/src/lib/types.ts` | envelope + docs |

---

### Task 1: Commit the spec; upgrade git2 to 0.21

**Files:**
- Modify: `Cargo.toml:94`, `Cargo.lock`
- Modify: `docs/superpowers/specs/2026-09-17-workspace-scale-design.md` (citations)

**Interfaces:** none (pure dependency bump — verified at planning time that 0.21.0 compiles with zero code changes and keeps `libgit2-sys 0.18.5+1.9.4`).

- [ ] **Step 1: Commit the approved spec and TODO tracking**

```bash
git add docs/superpowers/specs/2026-09-17-workspace-scale-design.md docs/internal/TODO.md docs/superpowers/plans/2026-09-18-workspace-scale-peek-policy.md
git commit -m "docs: workspace-scale design (§4 approved), plan and TODO tracking"
```

- [ ] **Step 2: Bump git2**

In `Cargo.toml` change `git2 = "0.20"` to `git2 = "0.21"`, then:

Run: `cargo update -p git2`
Expected: `Updating git2 v0.20.4 -> v0.21.0`; `grep -A1 'name = "libgit2-sys"' Cargo.lock` still shows `0.18.5+1.9.4`.

- [ ] **Step 3: Run the git source tests**

Run: `cargo test -p stroem-server --lib workspace::git`
Expected: all pass (the ignored network test stays ignored).

- [ ] **Step 4: Refresh spec citations to 0.21.0**

In the spec make exactly these replacements:
- `the pinned \`git2\` **0.20.4**` → `the pinned \`git2\` **0.21.0**`
- `(\`Cargo.lock:2095\`)` → `(\`Cargo.lock\`)`
- `(\`opts.rs:386\`)` → `(\`opts.rs:421\`)`
- `(\`opts.rs:425\`)` → `(\`opts.rs:460\`)`
- `(\`build.rs:474\`)` → `(\`build.rs:478\`)`
- `(\`repo.rs:771\`)` → `(\`repo.rs:782\`)`

Run: `grep -n "0.20.4\|opts.rs:386\|opts.rs:425\|build.rs:474\|repo.rs:771" docs/superpowers/specs/2026-09-17-workspace-scale-design.md`
Expected: no output.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock docs/superpowers/specs/2026-09-17-workspace-scale-design.md
git commit -m "chore(deps): bump git2 0.20 -> 0.21"
```

---

### Task 2: Runtime worker-thread floor (spec § 5 (2), § 10 step 2)

**Files:**
- Create: `crates/stroem-server/src/runtime.rs`
- Modify: `crates/stroem-server/src/lib.rs` (add `pub mod runtime;` after `pub mod restart;`)
- Modify: `crates/stroem-server/src/main.rs:26-27`

**Interfaces:**
- Produces: `stroem_server::runtime::{MIN_WORKER_THREADS: usize, worker_threads(available: Option<usize>) -> usize}`

- [ ] **Step 1: Write the failing test** — create `crates/stroem-server/src/runtime.rs`:

```rust
//! Tokio runtime sizing. `#[tokio::main]` sizes from `available_parallelism()`,
//! which honours the cgroup CPU quota — a `500m` limit yields ONE worker
//! thread (spec § 5). Floor it so the process is not at the mercy of the
//! deployed quota.

/// Minimum tokio worker threads, whatever the CPU quota says.
pub const MIN_WORKER_THREADS: usize = 4;

/// Worker threads for the server runtime: `max(MIN_WORKER_THREADS, available)`.
pub fn worker_threads(available: Option<usize>) -> usize {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_unknown_parallelism() {
        assert_eq!(worker_threads(None), MIN_WORKER_THREADS);
    }

    #[test]
    fn floors_a_sub_core_quota() {
        // 500m quota → available_parallelism() == 1
        assert_eq!(worker_threads(Some(1)), 4);
        assert_eq!(worker_threads(Some(2)), 4);
    }

    #[test]
    fn keeps_larger_parallelism() {
        assert_eq!(worker_threads(Some(16)), 16);
    }
}
```

Add `pub mod runtime;` to `crates/stroem-server/src/lib.rs` after `pub mod restart;`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-server --lib runtime::`
Expected: FAIL — panicked at `not yet implemented`.

- [ ] **Step 3: Implement**

```rust
pub fn worker_threads(available: Option<usize>) -> usize {
    available.unwrap_or(1).max(MIN_WORKER_THREADS)
}
```

- [ ] **Step 4: Replace `#[tokio::main]` in `main.rs`**

Replace lines `#[tokio::main]` / `async fn main() -> Result<()> {` with:

```rust
fn main() -> Result<()> {
    let worker_threads = stroem_server::runtime::worker_threads(
        std::thread::available_parallelism().map(|n| n.get()).ok(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;
    runtime.block_on(async_main(worker_threads))
}

async fn async_main(worker_threads: usize) -> Result<()> {
```

Right after the existing `tracing::info!("Starting Strøm server v{}", ...)` line add:

```rust
    tracing::info!("Tokio runtime: {} worker threads", worker_threads);
```

- [ ] **Step 5: Verify**

Run: `cargo test -p stroem-server --lib runtime:: && cargo build -p stroem-server`
Expected: 3 passed; build succeeds.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/runtime.rs crates/stroem-server/src/lib.rs crates/stroem-server/src/main.rs
git commit -m "feat(server): floor tokio worker threads at 4 regardless of CPU quota"
```

---

### Task 3: Helm envelope defaults + sizing guide (spec § 10 step 1)

**Files:**
- Modify: `helm/stroem/values.yaml:92-98` (startupProbe), `:156` (resources)
- Create: `docs/src/content/docs/operations/sizing.md`

**Interfaces:** none.

- [ ] **Step 1: Server resources** — replace `  resources: {}` (line 156, inside `server:`; NOT the worker's) with:

```yaml
  # Sized for ~50 git workspaces (docs/operations/sizing.md). A CPU limit
  # below 1 core used to leave the tokio runtime with ONE worker thread.
  resources:
    requests:
      cpu: "1"
      memory: 1Gi
    limits:
      cpu: "2"
      memory: 2Gi
```

- [ ] **Step 2: Enable the startup probe** — replace the `startupProbe: {}` block and its commented example (lines 92-98) with:

```yaml
  # Workspace loading blocks the listener at boot (docs/operations/sizing.md):
  # allow up to 5 minutes before liveness/readiness take over.
  startupProbe:
    enabled: true
    httpGet:
      path: /healthz
      port: http
    failureThreshold: 30
    periodSeconds: 10
```

- [ ] **Step 3: Render check**

Run: `helm template t helm/stroem --set server.config.worker_token=0123456789abcdef0123456789abcdef | grep -A12 "name: server" | grep -E "startupProbe|cpu:|memory:"`
Expected: `startupProbe:` and cpu `"1"`/`"2"`, memory `1Gi`/`2Gi` present. (If `helm` is unavailable, report that and inspect `templates/server-deployment.yaml:89-94` by eye.)

- [ ] **Step 4: Sizing guide** — create `docs/src/content/docs/operations/sizing.md`:

```markdown
---
title: Sizing
description: CPU, memory and probe settings for servers with many workspaces.
---

The server loads every configured workspace before it starts serving
requests, and keeps each git workspace's checkout and parsed configuration
in memory. Size it for the number of workspaces you run.

## Measured baseline

On a production server with **9 git workspaces**, `limits.cpu: 500m` and
`limits.memory: 512Mi`:

| Signal | Value |
|---|---|
| Workspace load at boot | 51 s |
| Memory in use | 95 % of the 512 Mi limit |
| Tokio worker threads | 1 (a sub-core CPU quota rounds down to one) |

Extrapolating linearly, 50 workspaces would need about 4.7 minutes to boot at
that quota — beyond a 5-minute startup probe. This is an estimate; measure your
own deployment.

## Recommended starting point (≈50 git workspaces)

| Setting | Value |
|---|---|
| `server.resources.requests` | `cpu: 1`, `memory: 1Gi` |
| `server.resources.limits` | `cpu: 2`, `memory: 2Gi` |
| `server.startupProbe` | `/healthz`, `failureThreshold: 30`, `periodSeconds: 10` |

These are the chart defaults. After changing them, check:

- the `Loaded N workspace(s) in …` log line at startup;
- `container_memory_working_set_bytes` for the server pod;
- `stroem_workspace_load_permits_available` and
  `stroem_workspace_load_overdue` (see [Metrics](/operations/metrics/)).

The server never runs fewer than 4 tokio worker threads, whatever the CPU
quota. Git clones live in the container's writable layer under `/tmp`; budget
node ephemeral storage for the sum of your repositories.
```

Check the docs sidebar: `grep -n "operations" docs/astro.config.mjs`. If the operations group lists pages explicitly, add `sizing` beside `metrics`; if it uses `autogenerate`, do nothing.

Run: `cd docs && bun run build 2>&1 | tail -3`
Expected: build completes without errors.

- [ ] **Step 5: Commit**

```bash
git add helm/stroem/values.yaml docs/src/content/docs/operations/sizing.md docs/astro.config.mjs
git commit -m "feat(helm): size the server for many workspaces; add sizing guide"
```

---

### Task 4: Tarball keep-set covers cross-workspace steps and the task-retry window (two bugs)

**Bugs fixed:**
1. `recovery.rs:443-446` keeps only `job.workspace/job.revision`; a cross-workspace step claims `job_step.action_workspace/action_revision` (`web/worker_api/jobs.rs:876`). A cold worker (restart, new pod) then 404s on download → step failure.
2. Task-retry handoff: settlement sees the failed job terminal before `create_retry_job` creates a retry inheriting `failed_job.revision` (`settlement/retry.rs:149`). A sweep in between finds no non-terminal owner.

**Files:**
- Modify: `crates/stroem-db/src/repos/job.rs` (add method to `impl JobRepo`)
- Modify: `crates/stroem-server/src/recovery.rs:441-466`
- Create: `crates/stroem-db/tests/tarball_keep_revisions_test.rs`

**Interfaces:**
- Produces: `JobRepo::tarball_keep_revisions(pool: &PgPool) -> anyhow::Result<Vec<(String, String)>>`

- [ ] **Step 1: Write the failing tests** — create `crates/stroem-db/tests/tarball_keep_revisions_test.rs`:

```rust
use anyhow::Result;
use sqlx::PgPool;
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

async fn job(pool: &PgPool, workspace: &str, revision: &str) -> Result<Uuid> {
    JobRepo::create(pool, workspace, "t", "distributed", None, "api", None, Some(revision), None).await
}

async fn set_status(pool: &PgPool, id: Uuid, status: &str) -> Result<()> {
    sqlx::query("UPDATE job SET status = $1, completed_at = NOW() WHERE job_id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn cross_ws_step(pool: &PgPool, job_id: Uuid, owner: &str, owner_rev: &str) -> Result<()> {
    let step = NewJobStep {
        job_id,
        step_name: "s".to_string(),
        action_name: format!("{owner}.act"),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: "pending".to_string(),
        required_ability: "script".to_string(),
        required_tags: vec![],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
        for_each_expr: None,
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
        max_retries: None,
        retry_backoff_secs: None,
        retry_strategy: None,
        retry_jitter: false,
        action_workspace: Some(owner.to_string()),
        action_revision: Some(owner_rev.to_string()),
    };
    JobStepRepo::create_steps(pool, &[step]).await
}

/// Failed top-level job still owed a task-level retry.
async fn owed_retry(pool: &PgPool, id: Uuid, attempt: i32, max: i32, age: &str) -> Result<()> {
    sqlx::query(
        "UPDATE job SET status = 'failed', max_retries = $1, retry_attempt = $2, \
         completed_at = NOW() - $3::interval WHERE job_id = $4",
    )
    .bind(max)
    .bind(attempt)
    .bind(age)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

fn has(rows: &[(String, String)], ws: &str, rev: &str) -> bool {
    rows.iter().any(|(w, r)| w == ws && r == rev)
}

#[tokio::test]
async fn keeps_active_job_revision_and_drops_finished() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let _active = job(&pool, "default", "r-active").await?;
    let done = job(&pool, "default", "r-done").await?;
    set_status(&pool, done, "completed").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(has(&rows, "default", "r-active"));
    assert!(!has(&rows, "default", "r-done"));
    Ok(())
}

#[tokio::test]
async fn keeps_cross_workspace_action_revision_of_active_job() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let caller = job(&pool, "caller", "c1").await?;
    cross_ws_step(&pool, caller, "owner", "o7").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(has(&rows, "owner", "o7"), "cross-workspace owner revision must be kept: {rows:?}");
    Ok(())
}

#[tokio::test]
async fn drops_cross_workspace_action_revision_of_finished_job() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let caller = job(&pool, "caller", "c1").await?;
    cross_ws_step(&pool, caller, "owner", "o7").await?;
    set_status(&pool, caller, "completed").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(!has(&rows, "owner", "o7"));
    Ok(())
}

#[tokio::test]
async fn keeps_revision_of_failed_job_awaiting_task_retry() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let id = job(&pool, "default", "r-retry").await?;
    owed_retry(&pool, id, 0, 2, "1 minute").await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(has(&rows, "default", "r-retry"), "handoff window must keep the revision");
    Ok(())
}

#[tokio::test]
async fn drops_failed_revision_once_retry_created_or_exhausted_or_stale() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let created = job(&pool, "default", "r-created").await?;
    owed_retry(&pool, created, 0, 2, "1 minute").await?;
    let retry = job(&pool, "other", "x").await?;
    set_status(&pool, retry, "completed").await?;
    JobRepo::set_retry_job_id(&pool, created, retry).await?;

    let exhausted = job(&pool, "default", "r-exhausted").await?;
    owed_retry(&pool, exhausted, 2, 2, "1 minute").await?;

    let stale = job(&pool, "default", "r-stale").await?;
    owed_retry(&pool, stale, 0, 2, "2 hours").await?;

    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(!has(&rows, "default", "r-created"));
    assert!(!has(&rows, "default", "r-exhausted"));
    assert!(!has(&rows, "default", "r-stale"));
    Ok(())
}

#[tokio::test]
async fn child_jobs_never_hold_a_retry_window() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let parent = job(&pool, "default", "r-parent").await?;
    set_status(&pool, parent, "completed").await?;
    let child = job(&pool, "default", "r-child").await?;
    owed_retry(&pool, child, 0, 2, "1 minute").await?;
    sqlx::query("UPDATE job SET parent_job_id = $1 WHERE job_id = $2")
        .bind(parent)
        .bind(child)
        .execute(&pool)
        .await?;
    let rows = JobRepo::tarball_keep_revisions(&pool).await?;
    assert!(!has(&rows, "default", "r-child"));
    Ok(())
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p stroem-db --test tarball_keep_revisions_test`
Expected: compile error — `no function or associated item named tarball_keep_revisions`.

- [ ] **Step 3: Implement** — add to `impl JobRepo` in `crates/stroem-db/src/repos/job.rs` (next to `set_retry_job_id`):

```rust
    /// `(workspace, revision)` pairs whose workspace tarball the server must
    /// keep cached:
    /// 1. every non-terminal job's own revision;
    /// 2. cross-workspace action revisions (`job_step.action_revision`) of
    ///    steps in non-terminal jobs — a step claims its OWNER's revision;
    /// 3. failed top-level jobs still owed a task-level retry — settlement
    ///    observes the job terminal before the retry job exists, and the
    ///    retry inherits this revision. Mirrors `terminal::plan`'s retry
    ///    gate; bounded to one hour so a retry that never got created does
    ///    not pin a revision forever.
    pub async fn tarball_keep_revisions(pool: &PgPool) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT workspace, revision FROM job \
              WHERE status IN ('pending', 'running') AND revision IS NOT NULL \
             UNION \
             SELECT s.action_workspace, s.action_revision FROM job_step s \
               JOIN job j ON j.job_id = s.job_id \
              WHERE j.status IN ('pending', 'running') \
                AND s.action_workspace IS NOT NULL AND s.action_revision IS NOT NULL \
             UNION \
             SELECT workspace, revision FROM job \
              WHERE status = 'failed' AND revision IS NOT NULL \
                AND parent_job_id IS NULL AND retry_job_id IS NULL \
                AND max_retries IS NOT NULL AND retry_attempt < max_retries \
                AND completed_at > NOW() - INTERVAL '1 hour'",
        )
        .fetch_all(pool)
        .await
        .context("query tarball keep revisions")?;
        Ok(rows)
    }
```

- [ ] **Step 4: Use it in recovery** — in `crates/stroem-server/src/recovery.rs`, replace the `let active_revisions ... = match sqlx::query_as::<_, (String, String)>( ... ).fetch_all(&state.pool).await { ... };` block (lines 442-459) with:

```rust
    // Active jobs, cross-workspace owner revisions of active steps, and
    // failed jobs still owed a task-level retry (JobRepo::tarball_keep_revisions).
    let active_revisions = match JobRepo::tarball_keep_revisions(&state.pool).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                "Tarball cache cleanup: failed to query revisions to keep: {:#}",
                e
            );
            return;
        }
    };
```

Update the doc comment above `tarball_cache_cleanup` to: `/// Keeps cached tarballs for every revision JobRepo::tarball_keep_revisions returns, plus each workspace's current revision. Everything else is evicted.`

- [ ] **Step 5: Verify**

Run: `cargo test -p stroem-db --test tarball_keep_revisions_test && cargo build -p stroem-server`
Expected: 6 passed; build OK.

- [ ] **Step 6: Mark TODO and commit** — in `docs/internal/TODO.md` change `- [ ] **Server tarball keep-set ignores \`job_step.action_revision\`**` and `- [ ] **Task-retry handoff window loses the tarball keep-set**` to `- [x]`.

```bash
git add crates/stroem-db crates/stroem-server/src/recovery.rs docs/internal/TODO.md
git commit -m "fix(server): keep tarballs of cross-workspace steps and owed task retries"
```

---

### Task 5: `LoadBudget` and deadline-bounded subprocesses (stroem-common)

**Files:**
- Create: `crates/stroem-common/src/budget.rs`
- Modify: `crates/stroem-common/src/lib.rs` (add `pub mod budget;` first in the list)

**Interfaces:**
- Produces:
  - `stroem_common::budget::LoadBudget` (`Copy`): `unbounded()`, `until(Instant)`, `from_now(Duration)`, `deadline() -> Option<Instant>`, `expired() -> bool`, `check() -> Result<(), DeadlineExceeded>`; `Default` = unbounded
  - `stroem_common::budget::DeadlineExceeded` (unit struct, `std::error::Error`)
  - `stroem_common::budget::is_deadline_exceeded(&anyhow::Error) -> bool`
  - `stroem_common::budget::run_with_deadline(cmd: std::process::Command, stdin: Option<&[u8]>, budget: &LoadBudget) -> anyhow::Result<std::process::Output>`

- [ ] **Step 1: Write the module with failing tests** — create `crates/stroem-common/src/budget.rs`:

```rust
//! Cooperative deadlines for workspace loading (spec § 4.5).
//!
//! A `LoadBudget` is checked between units of work (files, git callbacks) and
//! enforced on subprocesses (`sops`, `vals`) by killing them. It cannot
//! interrupt a single blocked syscall — that residual risk is stated in the spec.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A deadline threaded through a workspace load. `Copy`, so it can be moved
/// into git2 callbacks and Tera filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoadBudget {
    deadline: Option<Instant>,
}

impl LoadBudget {
    pub fn unbounded() -> Self {
        Self { deadline: None }
    }
    pub fn until(deadline: Instant) -> Self {
        Self { deadline: Some(deadline) }
    }
    pub fn from_now(duration: Duration) -> Self {
        Self::until(Instant::now() + duration)
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
    pub fn check(&self) -> std::result::Result<(), DeadlineExceeded> {
        if self.expired() {
            Err(DeadlineExceeded)
        } else {
            Ok(())
        }
    }
}

/// The load ran past its `LoadBudget`. A distinct type so callers can tell it
/// apart from ordinary per-file errors and abort the whole load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineExceeded;

impl std::fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "workspace load deadline exceeded")
    }
}

impl std::error::Error for DeadlineExceeded {}

/// True when `err` is, or wraps, [`DeadlineExceeded`].
pub fn is_deadline_exceeded(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DeadlineExceeded>().is_some()
        || err.chain().any(|cause| cause.is::<DeadlineExceeded>())
}

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run `cmd` to completion, killing and reaping it if `budget` expires.
///
/// stdout/stderr are drained on their own threads so a chatty child cannot
/// deadlock on a full pipe. After a kill the reader threads are DETACHED, not
/// joined: a grandchild that inherited the pipes may keep them open.
pub fn run_with_deadline(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    budget: &LoadBudget,
) -> Result<Output> {
    todo!()
}

fn spawn_reader<R: Read + Send + 'static>(pipe: Option<R>) -> Option<JoinHandle<Vec<u8>>> {
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    })
}

fn join_reader(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbounded_never_expires() {
        let b = LoadBudget::unbounded();
        assert!(!b.expired());
        assert!(b.check().is_ok());
        assert_eq!(b.deadline(), None);
        assert_eq!(LoadBudget::default(), b);
    }

    #[test]
    fn deadline_in_the_past_is_expired() {
        let b = LoadBudget::until(Instant::now());
        assert!(b.expired());
        assert_eq!(b.check(), Err(DeadlineExceeded));
    }

    #[test]
    fn deadline_detected_through_context_layers() {
        let err = anyhow::Error::new(DeadlineExceeded)
            .context("inner")
            .context("outer");
        assert!(is_deadline_exceeded(&err));
        assert!(!is_deadline_exceeded(&anyhow::anyhow!("something else")));
    }

    #[cfg(unix)]
    #[test]
    fn returns_output_of_a_normal_command() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2"]);
        let out = run_with_deadline(cmd, None, &LoadBudget::from_now(Duration::from_secs(10))).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[cfg(unix)]
    #[test]
    fn passes_stdin() {
        let out = run_with_deadline(Command::new("cat"), Some(b"payload"), &LoadBudget::unbounded()).unwrap();
        assert_eq!(out.stdout, b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn large_output_does_not_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 300000 /dev/zero"]);
        let out = run_with_deadline(cmd, None, &LoadBudget::from_now(Duration::from_secs(10))).unwrap();
        assert_eq!(out.stdout.len(), 300_000);
    }

    #[cfg(unix)]
    #[test]
    fn kills_the_child_at_the_deadline() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let err = run_with_deadline(cmd, None, &LoadBudget::from_now(Duration::from_millis(200)))
            .unwrap_err();
        assert!(is_deadline_exceeded(&err), "got {err:#}");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[test]
    fn expired_budget_does_not_spawn() {
        let cmd = Command::new("definitely-not-a-real-binary-7f3a");
        let err = run_with_deadline(cmd, None, &LoadBudget::until(Instant::now())).unwrap_err();
        assert!(is_deadline_exceeded(&err), "must fail on the deadline, not on spawn: {err:#}");
    }
}
```

Add `pub mod budget;` as the first line of `crates/stroem-common/src/lib.rs`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p stroem-common --lib budget::`
Expected: the three pure tests pass; `returns_output_of_a_normal_command`, `passes_stdin`, `large_output_does_not_deadlock`, `kills_the_child_at_the_deadline`, `expired_budget_does_not_spawn` FAIL with `not yet implemented`.

- [ ] **Step 3: Implement `run_with_deadline`**

```rust
pub fn run_with_deadline(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    budget: &LoadBudget,
) -> Result<Output> {
    budget.check()?;
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("failed to spawn subprocess")?;

    let writer = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => {
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            }))
        }
        _ => None,
    };
    let stdout = spawn_reader(child.stdout.take());
    let stderr = spawn_reader(child.stderr.take());

    let status = if budget.deadline().is_none() {
        child.wait().context("failed to wait for subprocess")?
    } else {
        loop {
            if let Some(status) = child.try_wait().context("failed to poll subprocess")? {
                break status;
            }
            if budget.expired() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DeadlineExceeded.into());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    };

    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(Output {
        status,
        stdout: join_reader(stdout),
        stderr: join_reader(stderr),
    })
}
```

- [ ] **Step 4: Verify**

Run: `cargo test -p stroem-common --lib budget::`
Expected: 8 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/budget.rs crates/stroem-common/src/lib.rs
git commit -m "feat(common): LoadBudget and deadline-bounded subprocess runner"
```

---

### Task 6: Thread `LoadBudget` through sops, vals and the workspace loader

Every existing function keeps its signature and delegates with `LoadBudget::unbounded()`, so the CLI, libraries and claim-time rendering are unchanged.

**Files:**
- Modify: `crates/stroem-common/src/sops.rs`
- Modify: `crates/stroem-common/src/template.rs:13-86`
- Modify: `crates/stroem-common/src/models/workflow.rs:1024-1096`
- Modify: `crates/stroem-common/src/workspace_loader.rs:33-178`

**Interfaces:**
- Consumes: Task 5 `LoadBudget`, `run_with_deadline`, `is_deadline_exceeded`, `DeadlineExceeded`
- Produces:
  - `sops::decrypt_sops_file_with(&Path, &LoadBudget) -> Result<String>`, `sops::read_yaml_file_with(&Path, &LoadBudget) -> Result<String>`
  - `template::render_template_with(&str, &serde_json::Value, &LoadBudget) -> Result<String>`
  - `WorkspaceConfig::render_secrets_with(&mut self, &LoadBudget)`, `WorkspaceConfig::render_connections_with(&mut self, &LoadBudget)`
  - `workspace_loader::scan_and_merge_yaml_files_with(&Path, bool, bool, &LoadBudget)`, `workspace_loader::load_workspace_with(&Path, &LoadBudget) -> Result<(WorkspaceConfig, Vec<String>)>`

- [ ] **Step 1: Write failing tests**

Append to `sops.rs` `mod tests`:

```rust
    #[test]
    fn read_yaml_file_with_expired_budget_is_deadline_exceeded() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("plain.yaml");
        std::fs::write(&path, "key: value\n").unwrap();
        let expired = crate::budget::LoadBudget::until(std::time::Instant::now());
        let err = read_yaml_file_with(&path, &expired).unwrap_err();
        assert!(crate::budget::is_deadline_exceeded(&err), "{err:#}");
    }
```

Append to `template.rs` `mod tests`:

```rust
    #[test]
    fn render_template_with_expired_budget_fails_on_ref_secret() {
        let expired = crate::budget::LoadBudget::until(std::time::Instant::now());
        let err = render_template_with(
            "{{ 'ref+echo://x' | vals }}",
            &serde_json::json!({}),
            &expired,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("deadline"), "{err:#}");
    }

    #[test]
    fn render_template_with_expired_budget_still_renders_plain_values() {
        let expired = crate::budget::LoadBudget::until(std::time::Instant::now());
        let out = render_template_with("{{ 'plain' | vals }}", &serde_json::json!({}), &expired)
            .unwrap();
        assert_eq!(out, "plain");
    }
```

Append to `workspace_loader.rs` `mod tests`:

```rust
    #[test]
    fn scan_with_expired_budget_aborts_instead_of_warning() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.yaml"), "actions: {}\n").unwrap();
        fs::write(dir.path().join("b.yaml"), "actions: {}\n").unwrap();
        let expired = crate::budget::LoadBudget::until(std::time::Instant::now());
        let err = scan_and_merge_yaml_files_with(dir.path(), false, true, &expired).unwrap_err();
        assert!(crate::budget::is_deadline_exceeded(&err), "{err:#}");
    }

    #[test]
    fn load_workspace_with_unbounded_matches_load_workspace() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("w.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo hi\n",
        )
        .unwrap();
        let (a, _) = load_workspace(dir.path()).unwrap();
        let (b, _) =
            load_workspace_with(dir.path(), &crate::budget::LoadBudget::unbounded()).unwrap();
        assert_eq!(a.actions.len(), b.actions.len());
    }
```

Run: `cargo test -p stroem-common --lib`
Expected: compile errors (`read_yaml_file_with`, `render_template_with`, `scan_and_merge_yaml_files_with`, `load_workspace_with` not found).

- [ ] **Step 2: sops.rs** — replace `decrypt_sops_file` and `read_yaml_file` with:

```rust
/// Decrypt a SOPS-encrypted file by shelling out to the `sops` CLI.
/// Returns the decrypted YAML content as a string.
pub fn decrypt_sops_file(path: &Path) -> Result<String> {
    decrypt_sops_file_with(path, &LoadBudget::unbounded())
}

/// [`decrypt_sops_file`], killing `sops` if `budget` expires.
pub fn decrypt_sops_file_with(path: &Path, budget: &LoadBudget) -> Result<String> {
    let mut cmd = std::process::Command::new("sops");
    cmd.arg("-d").arg(path);
    let output = run_with_deadline(cmd, None, budget).map_err(|e| {
        if is_deadline_exceeded(&e) {
            e.context(format!(
                "sops decryption of {} exceeded the load deadline",
                path.display()
            ))
        } else {
            e.context("Failed to run sops — is it installed and on PATH?")
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "sops decryption failed for {}: {}",
            path.display(),
            stderr.trim()
        );
    }

    String::from_utf8(output.stdout)
        .with_context(|| format!("sops output for {} is not valid UTF-8", path.display()))
}

/// Read a YAML file, decrypting it first if it's a SOPS file.
pub fn read_yaml_file(path: &Path) -> Result<String> {
    read_yaml_file_with(path, &LoadBudget::unbounded())
}

/// [`read_yaml_file`] under a [`LoadBudget`].
pub fn read_yaml_file_with(path: &Path, budget: &LoadBudget) -> Result<String> {
    budget.check()?;
    if is_sops_file(path) {
        decrypt_sops_file_with(path, budget)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read file: {}", path.display()))
    }
}
```

Add at the top: `use crate::budget::{is_deadline_exceeded, run_with_deadline, LoadBudget};`

- [ ] **Step 3: template.rs** — replace `fn vals_filter(...)` (lines 13-69) and `render_template` (lines 71-86) with:

```rust
/// Test-only wrapper keeping the historic two-argument filter signature.
#[cfg(test)]
fn vals_filter(
    value: &tera::Value,
    args: &HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    vals_filter_with(value, args, LoadBudget::unbounded())
}

/// Tera filter that resolves `ref+` secret references via the vals CLI.
///
/// Usage in templates: `{{ secret.KEY | vals }}`
/// - Non-string values pass through unchanged
/// - Strings not starting with `ref+` pass through unchanged
/// - Strings starting with `ref+` are resolved via `vals eval`, killed if
///   `budget` expires
fn vals_filter_with(
    value: &tera::Value,
    _args: &HashMap<String, tera::Value>,
    budget: LoadBudget,
) -> tera::Result<tera::Value> {
    let s = match value.as_str() {
        Some(s) => s,
        None => return Ok(value.clone()),
    };

    if !s.starts_with("ref+") {
        return Ok(value.clone());
    }

    let input = serde_json::json!({"_v": s});
    let input_str = serde_json::to_string(&input)
        .map_err(|e| tera::Error::msg(format!("vals: serialize failed: {e}")))?;

    let mut cmd = std::process::Command::new("vals");
    cmd.args(["eval", "-f", "-", "-o", "json"]);
    let output = run_with_deadline(cmd, Some(input_str.as_bytes()), &budget).map_err(|e| {
        if is_deadline_exceeded(&e) {
            tera::Error::msg("vals: deadline exceeded while resolving a ref+ secret")
        } else {
            tera::Error::msg(format!(
                "vals CLI not found. Install vals to use ref+ secrets: {e:#}"
            ))
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tera::Error::msg(format!(
            "vals eval failed (exit {}): {}",
            output.status,
            stderr.trim()
        )));
    }

    let resolved: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| tera::Error::msg(format!("vals: invalid output JSON: {e}")))?;

    match resolved.get("_v").and_then(|v| v.as_str()) {
        Some(resolved_str) => Ok(tera::Value::String(resolved_str.to_string())),
        None => Err(tera::Error::msg("vals: resolved output missing '_v' key")),
    }
}

/// Renders a single Tera template string against a JSON context
pub fn render_template(template: &str, context: &serde_json::Value) -> Result<String> {
    render_template_with(template, context, &LoadBudget::unbounded())
}

/// [`render_template`] whose `vals` filter honours `budget`.
pub fn render_template_with(
    template: &str,
    context: &serde_json::Value,
    budget: &LoadBudget,
) -> Result<String> {
    let mut tera = Tera::default();
    let template_name = "__template__";

    tera.add_raw_template(template_name, template)
        .context("Failed to parse template")?;

    let budget = *budget;
    tera.register_filter(
        "vals",
        move |value: &tera::Value, args: &HashMap<String, tera::Value>| {
            vals_filter_with(value, args, budget)
        },
    );

    let tera_context =
        tera::Context::from_serialize(context).context("Failed to convert JSON to Tera context")?;

    tera.render(template_name, &tera_context)
        .context("Failed to render template")
}
```

Add at the top: `use crate::budget::{is_deadline_exceeded, run_with_deadline, LoadBudget};`

- [ ] **Step 4: models/workflow.rs** — replace `render_secrets`, `render_connections` heads and `render_secret_value`:

```rust
    pub fn render_secrets(&mut self) -> anyhow::Result<()> {
        self.render_secrets_with(&crate::budget::LoadBudget::unbounded())
    }

    /// [`Self::render_secrets`] whose `vals` calls honour `budget`.
    pub fn render_secrets_with(&mut self, budget: &crate::budget::LoadBudget) -> anyhow::Result<()> {
        let empty_context = serde_json::json!({});
        for (key, value) in &mut self.secrets {
            render_secret_value(value, &empty_context, budget)
                .with_context(|| format!("Failed to render secret '{key}'"))?;
        }
        Ok(())
    }
```

For `render_connections`: rename the existing body to `pub fn render_connections_with(&mut self, budget: &crate::budget::LoadBudget) -> anyhow::Result<()>`, change its call `render_secret_value(value, &context)` to `render_secret_value(value, &context, budget)`, and add:

```rust
    pub fn render_connections(&mut self) -> anyhow::Result<()> {
        self.render_connections_with(&crate::budget::LoadBudget::unbounded())
    }
```

Change `render_secret_value` to take and pass the budget:

```rust
fn render_secret_value(
    value: &mut serde_json::Value,
    context: &serde_json::Value,
    budget: &crate::budget::LoadBudget,
) -> anyhow::Result<()> {
    use crate::template::render_template_with;

    match value {
        serde_json::Value::String(s) if s.contains("{{") => {
            let rendered = render_template_with(s, context, budget)?;
            *s = rendered;
        }
        serde_json::Value::String(_) => {}
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                render_secret_value(v, context, budget)?;
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                render_secret_value(v, context, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}
```

- [ ] **Step 5: workspace_loader.rs** — make `scan_and_merge_yaml_files` delegate, add the `_with` variant, and do the same for `load_workspace`:

```rust
pub fn scan_and_merge_yaml_files(
    scan_dir: &Path,
    skip_sops: bool,
    infer_folders: bool,
) -> Result<(WorkspaceConfig, Vec<String>)> {
    scan_and_merge_yaml_files_with(scan_dir, skip_sops, infer_folders, &LoadBudget::unbounded())
}
```

Rename the existing body to `pub fn scan_and_merge_yaml_files_with(scan_dir: &Path, skip_sops: bool, infer_folders: bool, budget: &LoadBudget) -> Result<(WorkspaceConfig, Vec<String>)>` (keep its doc comment on the `_with` fn, adding: "`budget` expiry aborts the WHOLE scan with [`DeadlineExceeded`] — it is never downgraded to a per-file warning, which would publish a partial config."). Inside the `for file_path in entries {` loop, make the first statement `budget.check()?;`, and replace the read with:

```rust
        let content = match crate::sops::read_yaml_file_with(&file_path, budget) {
            Ok(c) => c,
            Err(e) if is_deadline_exceeded(&e) => return Err(e),
            Err(e) => {
                let msg = format!("Skipping '{}': failed to read file: {:#}", display_path, e);
                tracing::warn!("{}", msg);
                warnings.push(msg);
                continue;
            }
        };
```

Then:

```rust
pub fn load_workspace(path: &Path) -> Result<(WorkspaceConfig, Vec<String>)> {
    load_workspace_with(path, &LoadBudget::unbounded())
}

/// [`load_workspace`] under a [`LoadBudget`] (spec § 4.5).
pub fn load_workspace_with(path: &Path, budget: &LoadBudget) -> Result<(WorkspaceConfig, Vec<String>)> {
    // (existing load_workspace body, with these three calls changed:)
    //   scan_and_merge_yaml_files(&scan_dir, false, true)  → scan_and_merge_yaml_files_with(&scan_dir, false, true, budget)
    //   workspace.render_secrets()                          → workspace.render_secrets_with(budget)
    //   workspace.render_connections()                      → workspace.render_connections_with(budget)
}
```

(Move the existing body verbatim; the comment lists the only three edits.) Add at the top: `use crate::budget::{is_deadline_exceeded, LoadBudget};`

- [ ] **Step 6: Verify**

Run: `cargo test -p stroem-common --lib && cargo test -p stroem-cli --lib && cargo clippy -p stroem-common --all-targets -- -D warnings`
Expected: all pass (the 5 new tests included); no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-common
git commit -m "feat(common): thread LoadBudget through sops, vals and the workspace loader"
```

---

### Task 7: Availability state machine (pure, spec § 4.4)

**Files:**
- Create: `crates/stroem-server/src/workspace/availability.rs`
- Modify: `crates/stroem-server/src/workspace/mod.rs:1-3` (add `pub mod availability;`)

**Interfaces:**
- Produces (all `pub`, all `Copy`): `Caller {Watcher, External, Startup}`, `InFlight {op_id: u64, started_at: Instant, deadline: Instant}`, `Freshness {Fresh{consecutive_peek_failures: u32}, Errored{backoff: Duration, next_attempt: Instant}}`, `Availability {freshness, load_in_flight: Option<InFlight>, peek_in_flight: Option<InFlight>}` with `fresh()`, `startup_failed(now, &Policy)`, `is_errored()`, `load_overdue(now) -> bool`; `Policy {peek_failure_threshold: u32, poll_interval: Duration, max_backoff: Duration}`; `ReloadSettings {peek_failure_threshold: u32, peek_timeout: Duration, load_timeout: Duration, max_backoff: Duration}` with `Default` (5, 30 s, 300 s, 900 s) and `policy(poll_interval) -> Policy`; `PeekObservation {Matches, Differs, NeedsLoad, Failed}`; `Event` (below); `Effect {None, Skip, Peek, AttemptLoad, PeekFailureSkipped{first: bool}}`; `transition(&mut Availability, Event, &Policy) -> Effect`.

- [ ] **Step 1: Write the module skeleton and the table tests** — create `availability.rs`:

```rust
//! Watcher availability state machine (spec § 4.4).
//!
//! Pure: no I/O, no locks, no clock reads — every `Instant` arrives inside an
//! `Event`. `transition` is the ONLY writer of an `Availability`; the entry
//! wraps it in a microsecond `std::sync::Mutex`.

use std::time::{Duration, Instant};

/// Who initiated a load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The per-workspace watcher — the only caller with a retry policy.
    Watcher,
    /// API refresh, peer reload notification, scheduler/webhook `force_refresh`.
    External,
    /// `WorkspaceManager::new`.
    Startup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    pub op_id: u64,
    pub started_at: Instant,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Serving a successfully loaded config. The counter saturates at K.
    Fresh { consecutive_peek_failures: u32 },
    /// Last load failed; the watcher retries at `next_attempt`.
    Errored { backoff: Duration, next_attempt: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Availability {
    pub freshness: Freshness,
    pub load_in_flight: Option<InFlight>,
    pub peek_in_flight: Option<InFlight>,
}

/// Per-workspace policy: K, the poll interval, and the backoff cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub peek_failure_threshold: u32,
    pub poll_interval: Duration,
    pub max_backoff: Duration,
}

/// Server-wide reload tuning (config `workspace_reload:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadSettings {
    /// K — consecutive failed peeks before a forced load.
    pub peek_failure_threshold: u32,
    /// P — budget for one peek.
    pub peek_timeout: Duration,
    /// L — budget for one load.
    pub load_timeout: Duration,
    /// Cap of the watcher's retry backoff while `Errored`.
    pub max_backoff: Duration,
}

impl Default for ReloadSettings {
    fn default() -> Self {
        Self {
            peek_failure_threshold: 5,
            peek_timeout: Duration::from_secs(30),
            load_timeout: Duration::from_secs(300),
            max_backoff: Duration::from_secs(900),
        }
    }
}

impl ReloadSettings {
    pub fn policy(&self, poll_interval: Duration) -> Policy {
        Policy {
            peek_failure_threshold: self.peek_failure_threshold.max(1),
            poll_interval,
            max_backoff: self.max_backoff,
        }
    }
}

/// A peek outcome reduced to what the transition needs (spec § 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekObservation {
    /// `Peek::Revision(r)` with `r` == the published revision.
    Matches,
    /// `Peek::Revision(r)` with `r` != the published revision.
    Differs,
    /// `Peek::Unsupported` or `Peek::LocalInvalid` — only a load can decide.
    NeedsLoad,
    /// `Peek::Failed` — "I don't know".
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Tick { now: Instant },
    PeekStarted { op_id: u64, started_at: Instant, deadline: Instant },
    /// Observer: the peek answered within P.
    PeekCompleted { observation: PeekObservation },
    /// Observer: P expired.
    PeekTimedOut,
    /// Finalizer: the peek worker really returned (or panicked).
    PeekFinished { op_id: u64 },
    LoadStarted { op_id: u64, started_at: Instant, deadline: Instant },
    /// Finalizer: the load really returned. `op_id` is `None` for loads that
    /// were never recorded as in flight (external, startup).
    LoadCompleted { op_id: Option<u64>, caller: Caller, ok: bool, completed_at: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Skip,
    Peek,
    AttemptLoad,
    /// A failed peek below K: skip, keep serving. `first` ⇒ log once.
    PeekFailureSkipped { first: bool },
}

impl Availability {
    pub fn fresh() -> Self {
        Self {
            freshness: Freshness::Fresh { consecutive_peek_failures: 0 },
            load_in_flight: None,
            peek_in_flight: None,
        }
    }

    /// Startup load failed: retry on the first watcher tick (spec § 4.4).
    pub fn startup_failed(now: Instant, policy: &Policy) -> Self {
        Self {
            freshness: Freshness::Errored { backoff: policy.poll_interval, next_attempt: now },
            load_in_flight: None,
            peek_in_flight: None,
        }
    }

    pub fn is_errored(&self) -> bool {
        matches!(self.freshness, Freshness::Errored { .. })
    }

    /// Derived, never stored (spec § 4.5 (4)).
    pub fn load_overdue(&self, now: Instant) -> bool {
        self.load_in_flight.is_some_and(|f| now > f.deadline)
    }
}

/// The single writer of an [`Availability`].
pub fn transition(a: &mut Availability, event: Event, policy: &Policy) -> Effect {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_secs(60);

    fn policy() -> Policy {
        Policy { peek_failure_threshold: 3, poll_interval: POLL, max_backoff: Duration::from_secs(900) }
    }

    fn fresh(n: u32) -> Availability {
        Availability { freshness: Freshness::Fresh { consecutive_peek_failures: n }, ..Availability::fresh() }
    }

    fn errored(backoff: Duration, next_attempt: Instant) -> Availability {
        Availability { freshness: Freshness::Errored { backoff, next_attempt }, ..Availability::fresh() }
    }

    fn inflight(op_id: u64, t: Instant) -> InFlight {
        InFlight { op_id, started_at: t, deadline: t + Duration::from_secs(5) }
    }

    fn done(caller: Caller, ok: bool, at: Instant) -> Event {
        Event::LoadCompleted { op_id: None, caller, ok, completed_at: at }
    }

    #[test]
    fn tick_while_errored_before_next_attempt_skips() {
        let now = Instant::now();
        let mut a = errored(POLL, now + POLL);
        assert_eq!(transition(&mut a, Event::Tick { now }, &policy()), Effect::Skip);
    }

    #[test]
    fn tick_while_errored_at_next_attempt_loads_without_peeking() {
        let now = Instant::now();
        let mut a = errored(POLL, now);
        assert_eq!(transition(&mut a, Event::Tick { now }, &policy()), Effect::AttemptLoad);
    }

    #[test]
    fn tick_while_fresh_peeks() {
        let mut a = fresh(0);
        assert_eq!(transition(&mut a, Event::Tick { now: Instant::now() }, &policy()), Effect::Peek);
    }

    #[test]
    fn tick_with_a_hung_peek_counts_as_timeout_and_starts_no_peek() {
        let now = Instant::now();
        let mut a = fresh(0);
        a.peek_in_flight = Some(inflight(1, now));
        assert_eq!(
            transition(&mut a, Event::Tick { now }, &policy()),
            Effect::PeekFailureSkipped { first: true }
        );
        assert_eq!(a.freshness, Freshness::Fresh { consecutive_peek_failures: 1 });
    }

    #[test]
    fn matching_peek_resets_the_counter() {
        let mut a = fresh(2);
        let e = transition(&mut a, Event::PeekCompleted { observation: PeekObservation::Matches }, &policy());
        assert_eq!(e, Effect::None);
        assert_eq!(a, fresh(0));
    }

    #[test]
    fn differing_or_undecidable_peek_attempts_a_load_without_state_change() {
        for obs in [PeekObservation::Differs, PeekObservation::NeedsLoad] {
            let mut a = fresh(1);
            let e = transition(&mut a, Event::PeekCompleted { observation: obs }, &policy());
            assert_eq!(e, Effect::AttemptLoad);
            assert_eq!(a, fresh(1));
        }
    }

    #[test]
    fn failed_peek_below_k_skips_and_counts() {
        let mut a = fresh(0);
        let p = policy();
        let failed = Event::PeekCompleted { observation: PeekObservation::Failed };
        assert_eq!(transition(&mut a, failed, &p), Effect::PeekFailureSkipped { first: true });
        assert_eq!(transition(&mut a, failed, &p), Effect::PeekFailureSkipped { first: false });
        assert_eq!(a, fresh(2));
    }

    #[test]
    fn kth_failure_saturates_and_attempts_a_forced_load() {
        let mut a = fresh(2);
        let e = transition(&mut a, Event::PeekTimedOut, &policy());
        assert_eq!(e, Effect::AttemptLoad);
        assert_eq!(a, fresh(3));
    }

    /// Round-4 finding 4.1: a rejected admission (no LoadStarted) must keep
    /// the forced probe pending instead of losing it.
    #[test]
    fn k_boundary_rejected_admission_keeps_the_probe_pending() {
        let p = policy();
        let failed = Event::PeekCompleted { observation: PeekObservation::Failed };
        let mut a = fresh(2);
        assert_eq!(transition(&mut a, failed, &p), Effect::AttemptLoad);
        // admission Busy/Saturated: nothing is applied
        assert_eq!(transition(&mut a, failed, &p), Effect::AttemptLoad, "probe still pending");
        assert_eq!(a, fresh(3));
        // a matching peek clears it
        let e = transition(&mut a, Event::PeekCompleted { observation: PeekObservation::Matches }, &p);
        assert_eq!(e, Effect::None);
        assert_eq!(a, fresh(0));
    }

    #[test]
    fn load_started_records_in_flight_and_resets_the_counter() {
        let now = Instant::now();
        let mut a = fresh(3);
        transition(&mut a, Event::LoadStarted { op_id: 7, started_at: now, deadline: now + POLL }, &policy());
        assert_eq!(a.freshness, Freshness::Fresh { consecutive_peek_failures: 0 });
        assert_eq!(a.load_in_flight.map(|f| f.op_id), Some(7));
    }

    #[test]
    fn load_started_while_errored_keeps_the_backoff() {
        let now = Instant::now();
        let mut a = errored(Duration::from_secs(120), now);
        transition(&mut a, Event::LoadStarted { op_id: 1, started_at: now, deadline: now + POLL }, &policy());
        assert_eq!(a.freshness, Freshness::Errored { backoff: Duration::from_secs(120), next_attempt: now });
    }

    #[test]
    fn peek_result_is_ignored_while_errored() {
        let now = Instant::now();
        let mut a = errored(POLL, now);
        for ev in [Event::PeekCompleted { observation: PeekObservation::Failed }, Event::PeekTimedOut] {
            assert_eq!(transition(&mut a, ev, &policy()), Effect::None);
        }
        assert!(a.is_errored());
    }

    #[test]
    fn finishes_clear_only_their_own_operation() {
        let now = Instant::now();
        let mut a = fresh(0);
        a.peek_in_flight = Some(inflight(9, now));
        a.load_in_flight = Some(inflight(10, now));
        transition(&mut a, Event::PeekFinished { op_id: 8 }, &policy());
        transition(&mut a, Event::LoadCompleted { op_id: Some(4), caller: Caller::Watcher, ok: true, completed_at: now }, &policy());
        assert!(a.peek_in_flight.is_some(), "stale op_id must not clear a newer peek");
        assert!(a.load_in_flight.is_some(), "stale op_id must not clear a newer load");
        transition(&mut a, Event::PeekFinished { op_id: 9 }, &policy());
        transition(&mut a, Event::LoadCompleted { op_id: Some(10), caller: Caller::Watcher, ok: true, completed_at: now }, &policy());
        assert!(a.peek_in_flight.is_none());
        assert!(a.load_in_flight.is_none());
    }

    #[test]
    fn successful_load_from_any_state_is_fresh() {
        let now = Instant::now();
        for mut a in [fresh(3), errored(Duration::from_secs(600), now)] {
            transition(&mut a, done(Caller::External, true, now), &policy());
            assert_eq!(a.freshness, Freshness::Fresh { consecutive_peek_failures: 0 });
        }
    }

    /// Round-2 regression: an EXTERNAL failure while fresh must land in
    /// Errored, where the watcher's backoff retries it.
    #[test]
    fn failure_while_fresh_enters_errored_for_watcher_and_external() {
        let now = Instant::now();
        for caller in [Caller::Watcher, Caller::External] {
            let mut a = fresh(0);
            transition(&mut a, done(caller, false, now), &policy());
            assert_eq!(a.freshness, Freshness::Errored { backoff: POLL, next_attempt: now + POLL });
            assert_eq!(transition(&mut a, Event::Tick { now: now + POLL }, &policy()), Effect::AttemptLoad);
        }
    }

    #[test]
    fn startup_failure_retries_on_the_first_tick() {
        let now = Instant::now();
        let mut a = Availability::startup_failed(now, &policy());
        assert_eq!(transition(&mut a, Event::Tick { now }, &policy()), Effect::AttemptLoad);
        let mut b = fresh(0);
        transition(&mut b, done(Caller::Startup, false, now), &policy());
        assert_eq!(b.freshness, Freshness::Errored { backoff: POLL, next_attempt: now });
    }

    #[test]
    fn watcher_failure_while_errored_doubles_backoff_up_to_the_cap() {
        let now = Instant::now();
        let mut a = errored(Duration::from_secs(600), now);
        transition(&mut a, done(Caller::Watcher, false, now), &policy());
        assert_eq!(a.freshness, Freshness::Errored {
            backoff: Duration::from_secs(900),
            next_attempt: now + Duration::from_secs(900),
        });
        let mut b = errored(POLL, now);
        transition(&mut b, done(Caller::Watcher, false, now), &policy());
        assert_eq!(b.freshness, Freshness::Errored { backoff: 2 * POLL, next_attempt: now + 2 * POLL });
    }

    #[test]
    fn external_failure_while_errored_leaves_the_ladder_unchanged() {
        let now = Instant::now();
        let before = errored(Duration::from_secs(240), now + Duration::from_secs(10));
        let mut a = before;
        transition(&mut a, done(Caller::External, false, now), &policy());
        assert_eq!(a, before);
    }

    #[test]
    fn overdue_is_derived_from_the_deadline() {
        let now = Instant::now();
        let mut a = fresh(0);
        a.load_in_flight = Some(InFlight { op_id: 1, started_at: now, deadline: now + Duration::from_secs(1) });
        assert!(!a.load_overdue(now));
        assert!(a.load_overdue(now + Duration::from_secs(2)));
    }

    #[test]
    fn settings_default_to_the_spec_values() {
        let s = ReloadSettings::default();
        assert_eq!(s.peek_failure_threshold, 5);
        assert_eq!(s.peek_timeout, Duration::from_secs(30));
        assert_eq!(s.load_timeout, Duration::from_secs(300));
        assert_eq!(s.max_backoff, Duration::from_secs(900));
        assert_eq!(ReloadSettings { peek_failure_threshold: 0, ..s }.policy(POLL).peek_failure_threshold, 1);
    }
}
```

Add `pub mod availability;` to `workspace/mod.rs` (line 1, before `pub mod folder;`).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p stroem-server --lib workspace::availability`
Expected: tests panic with `not yet implemented` (except `overdue_is_derived_from_the_deadline` and `settings_default_to_the_spec_values`).

- [ ] **Step 3: Implement `transition`**

```rust
pub fn transition(a: &mut Availability, event: Event, policy: &Policy) -> Effect {
    match event {
        Event::Tick { now } => match a.freshness {
            Freshness::Errored { next_attempt, .. } => {
                if now < next_attempt {
                    Effect::Skip
                } else {
                    Effect::AttemptLoad
                }
            }
            Freshness::Fresh { .. } => {
                if a.peek_in_flight.is_some() {
                    peek_failed(a, policy)
                } else {
                    Effect::Peek
                }
            }
        },
        Event::PeekStarted { op_id, started_at, deadline } => {
            a.peek_in_flight = Some(InFlight { op_id, started_at, deadline });
            Effect::None
        }
        Event::PeekCompleted { observation } => {
            if a.is_errored() {
                return Effect::None;
            }
            match observation {
                PeekObservation::Matches => {
                    a.freshness = Freshness::Fresh { consecutive_peek_failures: 0 };
                    Effect::None
                }
                PeekObservation::Differs | PeekObservation::NeedsLoad => Effect::AttemptLoad,
                PeekObservation::Failed => peek_failed(a, policy),
            }
        }
        Event::PeekTimedOut => {
            if a.is_errored() {
                Effect::None
            } else {
                peek_failed(a, policy)
            }
        }
        Event::PeekFinished { op_id } => {
            if a.peek_in_flight.is_some_and(|f| f.op_id == op_id) {
                a.peek_in_flight = None;
            }
            Effect::None
        }
        Event::LoadStarted { op_id, started_at, deadline } => {
            a.load_in_flight = Some(InFlight { op_id, started_at, deadline });
            if !a.is_errored() {
                a.freshness = Freshness::Fresh { consecutive_peek_failures: 0 };
            }
            Effect::None
        }
        Event::LoadCompleted { op_id, caller, ok, completed_at } => {
            if let Some(id) = op_id {
                if a.load_in_flight.is_some_and(|f| f.op_id == id) {
                    a.load_in_flight = None;
                }
            }
            a.freshness = match (ok, a.freshness, caller) {
                (true, _, _) => Freshness::Fresh { consecutive_peek_failures: 0 },
                (false, _, Caller::Startup) => Freshness::Errored {
                    backoff: policy.poll_interval,
                    next_attempt: completed_at,
                },
                (false, Freshness::Fresh { .. }, _) => Freshness::Errored {
                    backoff: policy.poll_interval,
                    next_attempt: completed_at + policy.poll_interval,
                },
                (false, Freshness::Errored { backoff, .. }, Caller::Watcher) => {
                    let next = backoff.saturating_mul(2).min(policy.max_backoff);
                    Freshness::Errored { backoff: next, next_attempt: completed_at + next }
                }
                (false, errored @ Freshness::Errored { .. }, Caller::External) => errored,
            };
            Effect::None
        }
    }
}

/// A failed or timed-out peek: count it; at K, force one load.
fn peek_failed(a: &mut Availability, policy: &Policy) -> Effect {
    let Freshness::Fresh { consecutive_peek_failures: n } = a.freshness else {
        return Effect::None;
    };
    let k = policy.peek_failure_threshold.max(1);
    let next = n.saturating_add(1);
    if next < k {
        a.freshness = Freshness::Fresh { consecutive_peek_failures: next };
        Effect::PeekFailureSkipped { first: n == 0 }
    } else {
        a.freshness = Freshness::Fresh { consecutive_peek_failures: k };
        Effect::AttemptLoad
    }
}
```

- [ ] **Step 4: Verify**

Run: `cargo test -p stroem-server --lib workspace::availability && cargo clippy -p stroem-server --all-targets -- -D warnings`
Expected: 20 passed; no warnings. (`dead_code` is not expected: the types are `pub`.)

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/workspace/availability.rs crates/stroem-server/src/workspace/mod.rs
git commit -m "feat(workspace): pure availability state machine for the watcher"
```

---

### Task 8: Three-way entry state and the single writer (spec §§ 4.4, 4.6)

The trait is still `async fn load` here (Task 9 changes it); this task only restructures state. The manager reads the revision **after a successful load only**, which already closes the publication-boundary bug at the manager level.

**Files:**
- Create: `crates/stroem-server/src/workspace/source.rs` (trait moved verbatim + `LoadOutcome`)
- Create: `crates/stroem-server/src/workspace/entry.rs`
- Modify: `crates/stroem-server/src/workspace/mod.rs` (manager; tests at ~`:2642`)
- Modify: `crates/stroem-server/tests/integration_test.rs` (4 `WorkspaceEntry {..}` literals at ~11291, ~11619, ~21548, ~21798)

**Interfaces:**
- Consumes: Task 7 `Availability`, `Caller`, `Event`, `Policy`, `ReloadSettings`, `transition`
- Produces:
  - `workspace::source::LoadOutcome { pub config: WorkspaceConfig, pub warnings: Vec<String>, pub revision: Option<String> }`
  - `workspace::entry::{ReloadState, Published, LoadSuccess, WorkspaceEntry}` re-exported from `workspace`
  - `WorkspaceEntry::new(name: impl Into<String>, source: Arc<dyn WorkspaceSource>, config: WorkspaceConfig, revision: Option<String>) -> Self`
  - `WorkspaceEntry::{published() -> Arc<Published>, is_healthy() -> bool, availability() -> Availability, exec() -> Arc<tokio::sync::Mutex<ReloadState>>, poll_interval() -> Duration}` (pub)
  - `pub(crate)`: `loaded(..)`, `startup_failed(..)`, `transition(Event, &Policy) -> Effect`, `next_op_id() -> u64`, `apply_load_result(Caller, Option<u64>, Result<LoadOutcome>, &HashMap<String, ResolvedLibrary>, &Policy, Instant) -> Result<LoadSuccess>`, `replace_config(WorkspaceConfig)`
  - `WorkspaceManager` fields become `entries: HashMap<String, Arc<WorkspaceEntry>>`, `resolved_libraries: Arc<HashMap<String, ResolvedLibrary>>`, plus `settings: ReloadSettings`; new `pub(crate) fn entry(&self, name: &str) -> Option<Arc<WorkspaceEntry>>`

- [ ] **Step 1: Move the trait** — create `source.rs` with the trait block from `mod.rs:27-49` moved **verbatim** (still `#[async_trait]`, still `revision()`), plus:

```rust
//! The workspace source contract (spec § 4.3).

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use stroem_common::models::workflow::WorkspaceConfig;

/// A successful load. Loading mutates no PUBLISHED state — only
/// `WorkspaceEntry::apply_load_result` publishes (spec § 4.6).
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    pub config: WorkspaceConfig,
    pub warnings: Vec<String>,
    pub revision: Option<String>,
}

// ... moved `pub trait WorkspaceSource` here, unchanged ...
```

In `mod.rs`: delete the trait, add `pub mod entry; pub mod source;` and `pub use entry::{LoadSuccess, Published, ReloadState, WorkspaceEntry}; pub use source::{LoadOutcome, WorkspaceSource};`.

- [ ] **Step 2: Write `entry.rs`**

```rust
//! One workspace's state, split by how long each part is held (spec § 4.4):
//! the execution mutex (whole load), `Availability` (microseconds) and the
//! published snapshot (clone/swap an `Arc`). Readers touch ONLY the snapshot.

use super::availability::{transition, Availability, Caller, Effect, Event, Policy};
use super::library::{merge_library_into_workspace, ResolvedLibrary};
use super::source::{LoadOutcome, WorkspaceSource};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use stroem_common::models::workflow::WorkspaceConfig;

/// Guarded by the execution mutex. `last_completed` drives the API refresh
/// cooldown and is stamped by external loads only.
#[derive(Default)]
pub struct ReloadState {
    pub last_completed: Option<Instant>,
}

/// What readers serve, swapped as one `Arc` (spec § 4.6).
#[derive(Debug, Clone)]
pub struct Published {
    pub config: Arc<WorkspaceConfig>,
    pub revision: Option<String>,
    pub warnings: Vec<String>,
    /// `Some` ⇔ the workspace is unavailable (last load failed).
    pub error: Option<String>,
    pub loaded_at: Option<Instant>,
    pub loaded_at_utc: Option<DateTime<Utc>>,
}

/// Outcome of a successful load, as seen by its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSuccess {
    pub revision_changed: bool,
}

pub struct WorkspaceEntry {
    pub name: String,
    pub source: Arc<dyn WorkspaceSource>,
    pub source_path: PathBuf,
    published: RwLock<Arc<Published>>,
    availability: Mutex<Availability>,
    /// Serializes loads on one checkout. Held for a whole load — by the load's
    /// finalizer, until the worker really returns. Nobody waits on it.
    exec: Arc<tokio::sync::Mutex<ReloadState>>,
    op_seq: AtomicU64,
}

impl WorkspaceEntry {
    /// A healthy entry serving `config` at `revision` (tests and in-memory sources).
    pub fn new(
        name: impl Into<String>,
        source: Arc<dyn WorkspaceSource>,
        config: WorkspaceConfig,
        revision: Option<String>,
    ) -> Self {
        Self::loaded(name.into(), source, config, Vec::new(), revision)
    }

    pub(crate) fn loaded(
        name: String,
        source: Arc<dyn WorkspaceSource>,
        config: WorkspaceConfig,
        warnings: Vec<String>,
        revision: Option<String>,
    ) -> Self {
        let published = Published {
            config: Arc::new(config),
            revision,
            warnings,
            error: None,
            loaded_at: Some(Instant::now()),
            loaded_at_utc: Some(Utc::now()),
        };
        Self::build(name, source, published, Availability::fresh())
    }

    pub(crate) fn startup_failed(
        name: String,
        source: Arc<dyn WorkspaceSource>,
        error: String,
        policy: &Policy,
    ) -> Self {
        let published = Published {
            config: Arc::new(WorkspaceConfig::new()),
            revision: None,
            warnings: Vec::new(),
            error: Some(error),
            loaded_at: None,
            loaded_at_utc: None,
        };
        Self::build(name, source, published, Availability::startup_failed(Instant::now(), policy))
    }

    fn build(
        name: String,
        source: Arc<dyn WorkspaceSource>,
        published: Published,
        availability: Availability,
    ) -> Self {
        let source_path = source.path().to_path_buf();
        Self {
            name,
            source,
            source_path,
            published: RwLock::new(Arc::new(published)),
            availability: Mutex::new(availability),
            exec: Arc::new(tokio::sync::Mutex::new(ReloadState::default())),
            op_seq: AtomicU64::new(0),
        }
    }

    pub fn published(&self) -> Arc<Published> {
        Arc::clone(&self.published.read().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn is_healthy(&self) -> bool {
        self.published().error.is_none()
    }

    pub fn availability(&self) -> Availability {
        *self.availability.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn exec(&self) -> Arc<tokio::sync::Mutex<ReloadState>> {
        Arc::clone(&self.exec)
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.source.poll_interval_secs().max(1))
    }

    pub(crate) fn transition(&self, event: Event, policy: &Policy) -> Effect {
        let mut a = self.availability.lock().unwrap_or_else(|e| e.into_inner());
        transition(&mut a, event, policy)
    }

    pub(crate) fn next_op_id(&self) -> u64 {
        self.op_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The ONLY writer of the published snapshot and of load-completion
    /// availability (spec § 4.4). Every load path calls it — watcher,
    /// external callers, startup. A failed load publishes nothing new: the
    /// previous config and revision stay, hidden behind `error`.
    pub(crate) fn apply_load_result(
        &self,
        caller: Caller,
        op_id: Option<u64>,
        result: Result<LoadOutcome>,
        libs: &HashMap<String, ResolvedLibrary>,
        policy: &Policy,
        completed_at: Instant,
    ) -> Result<LoadSuccess> {
        // Lock order: availability, then published. Readers take only the
        // published read lock, so this cannot deadlock.
        let mut availability = self.availability.lock().unwrap_or_else(|e| e.into_inner());
        let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
        let event = Event::LoadCompleted { op_id, caller, ok: result.is_ok(), completed_at };
        transition(&mut availability, event, policy);
        match result {
            Ok(LoadOutcome { mut config, warnings, revision }) => {
                for lib in libs.values() {
                    merge_library_into_workspace(&mut config, lib);
                }
                if !warnings.is_empty() {
                    tracing::warn!(
                        "Workspace '{}': {} file(s) skipped due to errors",
                        self.name,
                        warnings.len()
                    );
                }
                let revision_changed = published.revision != revision;
                *published = Arc::new(Published {
                    config: Arc::new(config),
                    revision,
                    warnings,
                    error: None,
                    loaded_at: Some(completed_at),
                    loaded_at_utc: Some(Utc::now()),
                });
                Ok(LoadSuccess { revision_changed })
            }
            Err(e) => {
                let previous = Arc::clone(&published);
                *published = Arc::new(Published {
                    config: Arc::clone(&previous.config),
                    revision: previous.revision.clone(),
                    warnings: Vec::new(),
                    error: Some(format!("{e:#}")),
                    loaded_at: previous.loaded_at,
                    loaded_at_utc: previous.loaded_at_utc,
                });
                Err(e)
            }
        }
    }

    /// Test support: swap the config, keeping revision and health.
    pub(crate) fn replace_config(&self, config: WorkspaceConfig) {
        let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
        let mut next = (**published).clone();
        next.config = Arc::new(config);
        *published = Arc::new(next);
    }
}

impl std::fmt::Debug for WorkspaceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("WorkspaceEntry");
        s.field("name", &self.name).field("source_path", &self.source_path);
        if let Some(err) = &self.published().error {
            s.field("load_error", err);
        }
        s.finish()
    }
}
```

Delete `ReloadState`, `WorkspaceEntry`, its `impl` and its `Debug` impl from `mod.rs` (lines 51-76, 107-135).

- [ ] **Step 3: Rewrite the manager's state access in `mod.rs`**

Struct:

```rust
#[derive(Debug)]
pub struct WorkspaceManager {
    entries: HashMap<String, Arc<WorkspaceEntry>>,
    load_errors: HashMap<String, String>,
    /// Resolved libraries — shared across all workspaces and load tasks.
    resolved_libraries: Arc<HashMap<String, ResolvedLibrary>>,
    triggers_disabled: HashSet<String>,
    settings: ReloadSettings,
}
```

Every constructor (`new`, `from_entries`, `from_config`, `from_configs`) sets `settings: ReloadSettings::default()` and wraps libraries in `Arc::new(..)`. Add `use availability::{Caller, ReloadSettings};` and `use std::time::Instant;` as needed.

`new()` — after the `JoinSet` loop, replace the `for (name, source, result) in loaded { .. }` body (`mod.rs:309-357`) with:

```rust
        for (name, source, result) in loaded {
            let entry = match result {
                Ok((mut config, warnings)) => {
                    for lib in resolved_libraries.values() {
                        merge_library_into_workspace(&mut config, lib);
                    }
                    if !warnings.is_empty() {
                        tracing::warn!(
                            "Workspace '{}': {} file(s) skipped due to errors",
                            name,
                            warnings.len()
                        );
                    }
                    let revision = source.revision();
                    WorkspaceEntry::loaded(name.clone(), source, config, warnings, revision)
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    tracing::error!("Failed to load workspace '{}': {}", name, err_msg);
                    let poll = Duration::from_secs(source.poll_interval_secs().max(1));
                    let policy = settings.policy(poll);
                    WorkspaceEntry::startup_failed(name.clone(), source, err_msg, &policy)
                }
            };
            entries.insert(name, Arc::new(entry));
        }
```

with `let settings = ReloadSettings::default();` near the top of `new()`.

`from_entries`:

```rust
    pub fn from_entries(entries: HashMap<String, WorkspaceEntry>) -> Self {
        Self {
            entries: entries.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            load_errors: HashMap::new(),
            resolved_libraries: Arc::new(HashMap::new()),
            triggers_disabled: HashSet::new(),
            settings: ReloadSettings::default(),
        }
    }
```

`from_config` / `from_configs`: build each entry with `WorkspaceEntry::new(name, source, config, revision)` (`from_config` passes `None`).

Accessors:

```rust
    pub(crate) fn entry(&self, name: &str) -> Option<Arc<WorkspaceEntry>> {
        self.entries.get(name).cloned()
    }

    pub async fn replace_config_for_test(&self, name: &str, cfg: WorkspaceConfig) {
        if let Some(entry) = self.entries.get(name) {
            entry.replace_config(cfg);
        }
    }

    pub fn mark_unavailable_for_test(&self, name: &str) {
        let entry = self
            .entries
            .get(name)
            .expect("mark_unavailable_for_test: no entry registered for this name");
        self.fail_for_test(entry, "marked unavailable for test");
    }

    #[cfg(test)]
    pub fn mark_errored_for_test(&self, name: &str, error: &str) {
        let entry = self.entries.get(name).expect("workspace entry");
        self.fail_for_test(entry, error);
    }

    /// Route a synthetic failure through the single writer so availability
    /// and the published error never diverge.
    fn fail_for_test(&self, entry: &WorkspaceEntry, error: &str) {
        let policy = self.settings.policy(entry.poll_interval());
        let _ = entry.apply_load_result(
            Caller::External,
            None,
            Err(anyhow::anyhow!("{error}")),
            &HashMap::new(),
            &policy,
            Instant::now(),
        );
    }

    pub async fn get_config(&self, name: &str) -> Option<Arc<WorkspaceConfig>> {
        let published = self.entries.get(name)?.published();
        published.error.is_none().then(|| Arc::clone(&published.config))
    }

    pub fn get_path(&self, name: &str) -> Option<&Path> {
        let entry = self.entries.get(name)?;
        entry.is_healthy().then_some(entry.source_path.as_path())
    }

    pub fn get_revision(&self, name: &str) -> Option<String> {
        let published = self.entries.get(name)?.published();
        if published.error.is_some() {
            return None;
        }
        published.revision.clone()
    }

    pub async fn get_all_configs(&self) -> Vec<(String, Arc<WorkspaceConfig>)> {
        self.entries
            .iter()
            .filter_map(|(name, entry)| {
                let p = entry.published();
                p.error.is_none().then(|| (name.clone(), Arc::clone(&p.config)))
            })
            .collect()
    }
```

`list_workspace_info` — the per-entry block becomes:

```rust
        for (name, entry) in &self.entries {
            let p = entry.published();
            let warnings = if p.error.is_none() { p.warnings.clone() } else { Vec::new() };
            infos.push(WorkspaceInfo {
                name: name.clone(),
                tasks_count: p.config.tasks.len(),
                actions_count: p.config.actions.len(),
                triggers_count: p.config.triggers.len(),
                connections_count: p.config.connections.len(),
                revision: p.revision.clone(),
                error: p.error.clone(),
                warnings,
                triggers_enabled: self.triggers_enabled(name),
            });
        }
```

`reload` / `reload_for_api` / `do_reload` (the `.lock().await` stays until Task 11):

```rust
    pub async fn reload(&self, name: &str) -> Result<()> {
        let entry = self
            .entries
            .get(name)
            .with_context(|| format!("Workspace '{}' not found", name))?;
        let exec = entry.exec();
        let mut reload_state = exec.lock().await;
        let result = self.do_reload(name, entry).await;
        reload_state.last_completed = Some(Instant::now());
        result
    }
```

In `reload_for_api` replace `entry.reload_state.try_lock()` with `let exec = entry.exec(); let mut reload_state = match exec.try_lock() { .. }` and the `Self::do_reload(name, entry, &self.resolved_libraries)` call with `self.do_reload(name, entry)`.

```rust
    /// Caller must hold `entry.exec()`.
    async fn do_reload(&self, name: &str, entry: &WorkspaceEntry) -> Result<()> {
        let result = entry.source.load().await.map(|(config, warnings)| LoadOutcome {
            config,
            warnings,
            revision: entry.source.revision(),
        });
        let policy = self.settings.policy(entry.poll_interval());
        entry
            .apply_load_result(
                Caller::External,
                None,
                result,
                &self.resolved_libraries,
                &policy,
                Instant::now(),
            )
            .map(|_| ())
            .with_context(|| format!("Failed to reload workspace '{}'", name))
    }
```

`start_watchers` — keep the loop, adapted (Task 12 replaces it): iterate `for entry in self.entries.values()`, clone `entry` (Arc), `libs = self.resolved_libraries.clone()`, `settings = self.settings`; remove `config_lock`, `load_error`, `load_warnings`, `needs_initial_load` captures (`let needs_initial_load = !entry.is_healthy();`). Inside the task:
- `let mut last_revision = entry.published().revision.clone();`
- `let is_errored = !entry.is_healthy();`
- replace the whole `match source.load().await { Ok(..) => {..} Err(e) => {..} }` with:

```rust
                    let source = entry.source.clone();
                    let result = source.load().await.map(|(config, warnings)| LoadOutcome {
                        config,
                        warnings,
                        revision: source.revision(),
                    });
                    let policy = settings.policy(entry.poll_interval());
                    match entry.apply_load_result(Caller::Watcher, None, result, &libs, &policy, Instant::now()) {
                        Ok(_) => {
                            let new_revision = entry.published().revision.clone();
                            tracing::info!(
                                "Workspace '{}' reloaded (revision: {:?} -> {:?})",
                                entry.name,
                                last_revision.as_deref().map(|s| &s[..8.min(s.len())]),
                                new_revision.as_deref().map(|s| &s[..8.min(s.len())]),
                            );
                            if let Some(bus) = &bus {
                                bus.publish_workspace_reloaded(&entry.name).await;
                            }
                            last_revision = new_revision;
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reload workspace '{}': {:#}", entry.name, e);
                        }
                    }
```

The peek call stays `source_clone.peek_revision()` via `spawn_blocking` (unchanged).

Existing test at ~`mod.rs:2642`: replace the two lines with

```rust
        let entry = mgr.entry("ws").unwrap();
        let _guard = entry.exec().lock_owned().await;
```

- [ ] **Step 4: Migrate integration-test literals** — at each of the 4 `WorkspaceEntry { config: .., source: .., name: .., source_path: .., load_error: .., load_warnings: .., reload_state: .. }` literals in `tests/integration_test.rs`, replace with `WorkspaceEntry::new("<name>", <source var>, <config var>, Some("<rev>".to_string()))` using the revision that source's `revision()` returns (`"test-rev"` at ~11291/~11619/~21798, `EXPECTED_REVISION` at ~21548). Remove the now-unused `use std::path::PathBuf;` and `use tokio::sync::RwLock;` in those scopes.

- [ ] **Step 5: Add the Task 8 tests** — append to `mod.rs` `mod tests`:

```rust
    fn cfg_with_action(action: &str) -> WorkspaceConfig {
        serde_yaml::from_str(&format!(
            "actions:\n  {action}:\n    type: script\n    script: echo hi\n"
        ))
        .unwrap()
    }

    fn one_ws(rev: &str) -> WorkspaceManager {
        WorkspaceManager::from_configs(vec![(
            "ws".to_string(),
            cfg_with_action("a"),
            Some(rev.to_string()),
        )])
    }

    #[tokio::test]
    async fn failed_load_publishes_only_the_error() {
        let mgr = one_ws("rev-a");
        let entry = mgr.entry("ws").unwrap();
        let policy = mgr.settings.policy(entry.poll_interval());
        let before = entry.published();
        let err = entry
            .apply_load_result(
                availability::Caller::External,
                None,
                Err(anyhow::anyhow!("secret render failed")),
                &HashMap::new(),
                &policy,
                Instant::now(),
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("secret render failed"));
        let after = entry.published();
        assert!(Arc::ptr_eq(&before.config, &after.config), "config must not change");
        assert_eq!(after.revision.as_deref(), Some("rev-a"), "revision must not change");
        assert_eq!(after.error.as_deref(), Some("secret render failed"));
        assert!(mgr.get_config("ws").await.is_none());
        assert!(mgr.get_revision("ws").is_none());
        assert!(entry.availability().is_errored(), "external failure must land in Errored");
    }

    #[tokio::test]
    async fn successful_load_publishes_config_and_revision_together() {
        let mgr = one_ws("rev-a");
        let entry = mgr.entry("ws").unwrap();
        let policy = mgr.settings.policy(entry.poll_interval());
        let ok = entry
            .apply_load_result(
                availability::Caller::Watcher,
                None,
                Ok(LoadOutcome {
                    config: cfg_with_action("b"),
                    warnings: vec!["w".to_string()],
                    revision: Some("rev-b".to_string()),
                }),
                &HashMap::new(),
                &policy,
                Instant::now(),
            )
            .unwrap();
        assert!(ok.revision_changed);
        assert_eq!(mgr.get_revision("ws").as_deref(), Some("rev-b"));
        assert!(mgr.get_config("ws").await.unwrap().actions.contains_key("b"));
        assert_eq!(mgr.list_workspace_info().await[0].warnings, vec!["w".to_string()]);
    }

    #[tokio::test]
    async fn readers_do_not_wait_for_the_execution_mutex() {
        let mgr = one_ws("rev-a");
        let entry = mgr.entry("ws").unwrap();
        let _held = entry.exec().lock_owned().await;
        let cfg = tokio::time::timeout(Duration::from_millis(200), mgr.get_config("ws"))
            .await
            .expect("get_config must not wait for a load");
        assert!(cfg.is_some());
        assert!(mgr.get_path("ws").is_some());
        assert_eq!(mgr.get_revision("ws").as_deref(), Some("rev-a"));
        assert_eq!(mgr.list_workspace_info().await.len(), 1);
    }
```

(Ensure `use std::time::Instant;` and `use super::*;` cover these; `settings` is a private field readable from the child `tests` module.)

- [ ] **Step 6: Verify**

Run: `cargo test -p stroem-server --lib workspace && cargo check -p stroem-server --tests && cargo clippy -p stroem-server --all-targets -- -D warnings`
Expected: all workspace tests pass (3 new); integration tests compile; no warnings.

Run: `cargo test -p stroem-server --test integration_test -- --test-threads=4 2>&1 | tail -3` (Docker)
Expected: same pass/fail counts as `main` (record the 4 pre-existing failures noted in memory if they recur; any NEW failure is a regression to fix).

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server
git commit -m "refactor(workspace): three-way entry state with a single load-result writer"
```

---

### Task 9: Synchronous source contract, `Peek` classification, git deadlines (spec §§ 4.3, 4.5 (0), 4.6)

**Files:**
- Modify: `crates/stroem-server/src/workspace/source.rs`
- Modify: `crates/stroem-server/src/workspace/git.rs` (impl + tests)
- Modify: `crates/stroem-server/src/workspace/folder.rs` (impl + tests)
- Modify: `crates/stroem-server/src/workspace/mod.rs` (`InMemorySource`, `new`, `do_reload`, watcher)
- Modify: `crates/stroem-server/tests/integration_test.rs` (4 source impls)

**Interfaces:**
- Consumes: Task 5 `LoadBudget`, `DeadlineExceeded`; Task 6 `load_workspace_with`; Task 8 `LoadOutcome`
- Produces:
  - `pub enum Peek { Revision(String), Unsupported, Failed(anyhow::Error), LocalInvalid(anyhow::Error) }`
  - `pub trait WorkspaceSource: Send + Sync { fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome>; fn path(&self) -> &Path; fn peek_revision(&self, _budget: &LoadBudget) -> Peek { Peek::Unsupported } fn poll_interval_secs(&self) -> u64 { 30 } }` — **no** `revision()`, **no** `async`
  - `folder::load_folder_workspace_with(path: &Path, budget: &LoadBudget) -> Result<(WorkspaceConfig, Vec<String>)>`

- [ ] **Step 1: Replace the trait** in `source.rs` (keep `LoadOutcome`, drop `async_trait`):

```rust
use stroem_common::budget::LoadBudget;

/// Result of a cheap change check (spec § 4.3).
#[derive(Debug)]
pub enum Peek {
    /// Remote/tree answered authoritatively; compare to the PUBLISHED revision.
    Revision(String),
    /// This source cannot peek; the caller must do a full load.
    Unsupported,
    /// Could not determine the current state — skip, keep the loaded config.
    Failed(anyhow::Error),
    /// Local state is unusable — only a full load can fix it.
    LocalInvalid(anyhow::Error),
}

/// A workspace source. Both `load` and `peek_revision` BLOCK — always call
/// them through `tokio::task::spawn_blocking`, never on a runtime thread.
pub trait WorkspaceSource: Send + Sync {
    /// Load the workspace. Mutates no published state (spec § 4.6).
    fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome>;
    /// Filesystem path where the workspace files reside.
    fn path(&self) -> &Path;
    /// Cheap change check. Default: cannot peek.
    fn peek_revision(&self, _budget: &LoadBudget) -> Peek {
        Peek::Unsupported
    }
    /// Polling interval in seconds for the background watcher.
    fn poll_interval_secs(&self) -> u64 {
        30
    }
}
```

Re-export: `pub use source::{LoadOutcome, Peek, WorkspaceSource};` in `mod.rs`.

- [ ] **Step 2: Write the new git tests** (append to `git.rs` `mod tests`; `use stroem_common::budget::{is_deadline_exceeded, LoadBudget};` and `use crate::workspace::source::Peek;` at the top of the module):

```rust
    const YAML_V1: &str = "actions:\n  a:\n    type: script\n    script: echo v1\n";
    const YAML_V2: &str = "actions:\n  a:\n    type: script\n    script: echo v2\n";

    fn unbounded() -> LoadBudget {
        LoadBudget::unbounded()
    }

    #[test]
    fn load_runs_outside_any_tokio_runtime() {
        // block_in_place would panic here: loading must be plain blocking code.
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, dir.path().join("repo"));
        let out = source.load(&unbounded()).unwrap();
        assert_eq!(out.revision.as_deref().map(str::len), Some(40));
        assert_eq!(out.config.actions.len(), 1);
    }

    #[test]
    fn peek_classifies_a_missing_clone_as_local_invalid() {
        let dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir("file:///nowhere", "main", None, dir.path().join("repo"));
        assert!(matches!(source.peek_revision(&unbounded()), Peek::LocalInvalid(_)));
    }

    #[test]
    fn peek_classifies_a_corrupt_checkout_as_local_invalid() {
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join("not-a-repo"), "x").unwrap();
        let source = GitSource::with_clone_dir("file:///nowhere", "main", None, clone);
        assert!(matches!(source.peek_revision(&unbounded()), Peek::LocalInvalid(_)));
    }

    #[test]
    fn peek_classifies_a_missing_origin_as_local_invalid() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        let source = GitSource::with_clone_dir(&url, "main", None, clone.clone());
        source.load(&unbounded()).unwrap();
        git2::Repository::open(&clone).unwrap().remote_delete("origin").unwrap();
        assert!(matches!(source.peek_revision(&unbounded()), Peek::LocalInvalid(_)));
    }

    #[test]
    fn peek_classifies_an_unreachable_remote_as_failed() {
        let (bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, dir.path().join("repo"));
        source.load(&unbounded()).unwrap();
        drop(bare); // the remote disappears
        assert!(matches!(source.peek_revision(&unbounded()), Peek::Failed(_)));
    }

    #[test]
    fn peek_classifies_a_missing_branch_as_failed() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        GitSource::with_clone_dir(&url, "main", None, clone.clone()).load(&unbounded()).unwrap();
        let other = GitSource::with_clone_dir(&url, "no-such-branch", None, clone);
        assert!(matches!(other.peek_revision(&unbounded()), Peek::Failed(_)));
    }

    #[test]
    fn peek_revision_matches_the_loaded_revision() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, dir.path().join("repo"));
        let loaded = source.load(&unbounded()).unwrap().revision.unwrap();
        match source.peek_revision(&unbounded()) {
            Peek::Revision(r) => assert_eq!(r, loaded),
            other => panic!("expected Revision, got {other:?}"),
        }
    }

    #[test]
    fn expired_budget_fails_before_touching_the_checkout() {
        let (bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        let source = GitSource::with_clone_dir(&url, "main", None, clone.clone());
        source.load(&unbounded()).unwrap();
        add_commit(bare.path(), "main", &[("test.yaml", YAML_V2)], "v2");
        let expired = LoadBudget::until(std::time::Instant::now());
        let err = source.load(&expired).unwrap_err();
        assert!(is_deadline_exceeded(&err), "{err:#}");
        let on_disk = std::fs::read_to_string(clone.join("test.yaml")).unwrap();
        assert!(on_disk.contains("echo v1"), "checkout must be untouched");
    }
```

Run: `cargo test -p stroem-server --lib workspace::git`
Expected: compile errors (new trait signature not implemented yet).

- [ ] **Step 3: Rewrite `GitSource`'s implementation** (`git.rs`):
- remove the `revision: RwLock<Option<String>>` field from the struct and both constructors; remove `use std::sync::RwLock`.
- `fn clone_or_fetch(&self, budget: &LoadBudget) -> Result<String>` — first statement `budget.check()?;` (pre-mutation expiry check, spec § 4.5 (3)); pass `budget` to `build_remote_callbacks`; in the fetch branch replace the `.context("Failed to fetch from origin")?` with `.map_err(|e| git_error(e, budget, "Failed to fetch from origin"))?` and the reset with:

```rust
            let mut checkout = checkout_builder(budget);
            repo.reset(&object, git2::ResetType::Hard, Some(&mut checkout))
                .map_err(|e| git_error(e, budget, "Failed to reset to fetched ref"))?;
```

  in the clone branch add `builder.with_checkout(checkout_builder(budget));` before `builder.branch(..)` and map the clone error with `git_error(e, budget, "Failed to clone git repository")`.
- `build_remote_callbacks(auth: &Option<GitAuthConfig>, budget: &LoadBudget) -> git2::RemoteCallbacks<'_>`: after `let mut callbacks = git2::RemoteCallbacks::new();` add

```rust
        // Cooperative total deadline for object transfer (spec § 4.5). Fires
        // only between reads; a blocked read is bounded by the global libgit2
        // server timeout instead.
        let budget = *budget;
        callbacks.transfer_progress(move |_| !budget.expired());
```

- add free functions:

```rust
/// Checkout options whose `notify` callback cancels during checkout PLANNING
/// (`checkout_get_actions`) once `budget` expires. libgit2 cannot cancel the
/// write phase that follows — see spec § 4.5. `notify_on` is required:
/// notification types default to none.
fn checkout_builder(budget: &LoadBudget) -> git2::build::CheckoutBuilder<'static> {
    let budget = *budget;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.notify_on(
        git2::CheckoutNotificationType::UPDATED
            | git2::CheckoutNotificationType::CONFLICT
            | git2::CheckoutNotificationType::DIRTY,
    );
    checkout.notify(move |_, _, _, _, _| !budget.expired());
    checkout
}

/// A libgit2 error, reported as `DeadlineExceeded` when our own callbacks
/// aborted it because the budget ran out.
fn git_error(err: git2::Error, budget: &LoadBudget, msg: &'static str) -> anyhow::Error {
    if budget.expired() {
        anyhow::Error::new(DeadlineExceeded).context(format!("{msg}: {err}"))
    } else {
        anyhow::Error::new(err).context(msg)
    }
}
```

- the trait impl:

```rust
impl WorkspaceSource for GitSource {
    fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome> {
        let oid = self.clone_or_fetch(budget).context("Git clone/fetch failed")?;
        let (config, warnings) = super::folder::load_folder_workspace_with(&self.clone_dir, budget)?;
        Ok(LoadOutcome { config, warnings, revision: Some(oid) })
    }

    fn path(&self) -> &Path {
        &self.clone_dir
    }

    fn peek_revision(&self, budget: &LoadBudget) -> Peek {
        if !self.clone_dir.exists() {
            return Peek::LocalInvalid(anyhow::anyhow!(
                "clone directory {} does not exist",
                self.clone_dir.display()
            ));
        }
        let repo = match git2::Repository::open(&self.clone_dir) {
            Ok(r) => r,
            Err(e) => return Peek::LocalInvalid(anyhow::Error::new(e).context("open local clone")),
        };
        let mut remote = match repo.find_remote("origin") {
            Ok(r) => r,
            Err(e) => return Peek::LocalInvalid(anyhow::Error::new(e).context("find remote 'origin'")),
        };
        if budget.expired() {
            return Peek::Failed(DeadlineExceeded.into());
        }
        let callbacks = Self::build_remote_callbacks(&self.auth, budget);
        let connection = match remote.connect_auth(git2::Direction::Fetch, Some(callbacks), None) {
            Ok(c) => c,
            Err(e) => return Peek::Failed(anyhow::Error::new(e).context("connect to origin")),
        };
        let refs = match connection.list() {
            Ok(r) => r,
            Err(e) => return Peek::Failed(anyhow::Error::new(e).context("list remote refs")),
        };
        let target = format!("refs/heads/{}", self.git_ref);
        match refs.iter().find(|r| r.name() == target) {
            Some(r) => Peek::Revision(r.oid().to_string()),
            None => Peek::Failed(anyhow::anyhow!("{target} is not advertised by origin")),
        }
    }

    fn poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs
    }
}
```

Imports at the top of `git.rs`: `use stroem_common::budget::{DeadlineExceeded, LoadBudget};` and `use super::source::{LoadOutcome, Peek};`; drop `async_trait`.

- [ ] **Step 4: Convert the existing git tests mechanically**
- `source.load().await.unwrap()` / `source.load().await` → `source.load(&LoadBudget::unbounded()).unwrap()` / `source.load(&LoadBudget::unbounded())`; a destructuring `let (config, _) = ...` becomes `let config = ....config;`.
- `source.revision().unwrap()` after a load → the `.revision.unwrap()` of that load's `LoadOutcome` (bind it: `let out = source.load(..).unwrap(); let rev = out.revision.unwrap();`).
- delete `assert!(source.revision().is_none());` lines (no such state any more).
- `source.peek_revision().is_none()` before a clone → `matches!(source.peek_revision(&LoadBudget::unbounded()), Peek::LocalInvalid(_))`; `source.peek_revision().unwrap()` → `match source.peek_revision(&LoadBudget::unbounded()) { Peek::Revision(r) => r, other => panic!("{other:?}") }`.
- tests may stay `#[tokio::test(flavor = "multi_thread")]`.

- [ ] **Step 5: FolderSource** (`folder.rs`): remove the `revision` field; replace `compute_revision` with a classifying version and the trait impl:

```rust
    /// Hash every file's relative path + content (spec § 4.3 folder rows).
    fn compute_revision(path: &Path, budget: &LoadBudget) -> Peek {
        if !path.exists() {
            return Peek::LocalInvalid(anyhow::anyhow!(
                "workspace folder {} does not exist",
                path.display()
            ));
        }
        let mut hasher = Blake2s256::new();
        for entry in walkdir::WalkDir::new(path)
            .max_depth(10)
            .follow_links(true)
            .sort_by_file_name()
        {
            if budget.expired() {
                return Peek::Failed(DeadlineExceeded.into());
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return Peek::Failed(anyhow::Error::new(e).context("walk workspace folder")),
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry.path().strip_prefix(path).unwrap_or(entry.path()).to_string_lossy();
            hasher.update(relative.as_bytes());
            match std::fs::read(entry.path()) {
                Ok(content) => hasher.update(&content),
                Err(e) => {
                    return Peek::Failed(
                        anyhow::Error::new(e).context(format!("read {}", entry.path().display())),
                    )
                }
            }
        }
        Peek::Revision(hex::encode(hasher.finalize()))
    }
```

```rust
impl WorkspaceSource for FolderSource {
    fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome> {
        let (config, warnings) = load_folder_workspace_with(&self.path, budget)?;
        let revision = match Self::compute_revision(&self.path, budget) {
            Peek::Revision(r) => Some(r),
            Peek::Unsupported => None,
            Peek::Failed(e) | Peek::LocalInvalid(e) => {
                return Err(e.context("hash workspace folder"))
            }
        };
        Ok(LoadOutcome { config, warnings, revision })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn peek_revision(&self, budget: &LoadBudget) -> Peek {
        Self::compute_revision(&self.path, budget)
    }
}

/// Load a workspace folder under a [`LoadBudget`] (blocking).
pub fn load_folder_workspace_with(
    path: &Path,
    budget: &LoadBudget,
) -> Result<(WorkspaceConfig, Vec<String>)> {
    let (workspace, warnings) = stroem_common::workspace_loader::load_workspace_with(path, budget)?;
    tracing::debug!(
        "Loaded workspace: {} actions, {} tasks, {} triggers, {} secrets, {} connection_types, {} connections",
        workspace.actions.len(),
        workspace.tasks.len(),
        workspace.triggers.len(),
        workspace.secrets.len(),
        workspace.connection_types.len(),
        workspace.connections.len()
    );
    Ok((workspace, warnings))
}

/// Load workspace from a folder containing workflow YAML files.
/// Looks for .workflows/ subdirectory, or scans the folder itself if it contains YAML files.
/// Returns the config paired with per-file warnings for files that were skipped.
pub async fn load_folder_workspace(path: &str) -> Result<(WorkspaceConfig, Vec<String>)> {
    load_folder_workspace_with(Path::new(path), &LoadBudget::unbounded())
}
```

Convert the two folder tests that use `FolderSource` (~`:284-288`, ~`:462-477`) the same way as Step 4 (drop the "no revision before load" assertion). Add:

```rust
    #[test]
    fn folder_peek_on_a_missing_root_is_local_invalid() {
        let source = FolderSource::new("/nonexistent/stroem-peek-root");
        assert!(matches!(source.peek_revision(&LoadBudget::unbounded()), Peek::LocalInvalid(_)));
    }

    #[cfg(unix)]
    #[test]
    fn folder_peek_on_an_unreadable_file_is_failed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("w.yaml");
        std::fs::write(&file, "actions: {}\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&file).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let source = FolderSource::new(dir.path().to_str().unwrap());
        let peek = source.peek_revision(&LoadBudget::unbounded());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(peek, Peek::Failed(_)), "{peek:?}");
    }

    #[test]
    fn folder_peek_matches_the_loaded_revision() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("w.yaml"), "actions: {}\n").unwrap();
        let source = FolderSource::new(dir.path().to_str().unwrap());
        let loaded = source.load(&LoadBudget::unbounded()).unwrap().revision.unwrap();
        match source.peek_revision(&LoadBudget::unbounded()) {
            Peek::Revision(r) => assert_eq!(r, loaded),
            other => panic!("{other:?}"),
        }
    }
```

- [ ] **Step 6: Manager call sites** (`mod.rs`)
- `InMemorySource`: `config: WorkspaceConfig` (drop the tokio `RwLock`), `revision: Option<String>`; `impl WorkspaceSource for InMemorySource { fn load(&self, _b: &LoadBudget) -> Result<LoadOutcome> { Ok(LoadOutcome { config: self.config.clone(), warnings: Vec::new(), revision: self.revision.clone() }) } fn path(&self) -> &Path { Path::new("/dev/null") } }` — default `peek_revision` (`Unsupported`).
- `new()` spawned task: replace `let result = source.load().await;` with

```rust
                let budget = LoadBudget::from_now(settings.load_timeout);
                let loader = source.clone();
                let result = tokio::task::spawn_blocking(move || loader.load(&budget))
                    .await
                    .unwrap_or_else(|e| Err(anyhow::anyhow!("workspace load task panicked: {e}")));
```

  (capture `settings` by copy into the task), and in the result loop use `Ok(LoadOutcome { mut config, warnings, revision })` instead of reading `source.revision()`.
- `do_reload`: `let source = entry.source.clone(); let budget = LoadBudget::from_now(self.settings.load_timeout); let result = tokio::task::spawn_blocking(move || source.load(&budget)).await.unwrap_or_else(|e| Err(anyhow::anyhow!("workspace load task panicked: {e}")));`
- watcher: same `spawn_blocking` for the load; peek becomes `tokio::task::spawn_blocking(move || match source_clone.peek_revision(&LoadBudget::unbounded()) { Peek::Revision(r) => Some(r), _ => None })` (Task 12 replaces this whole loop).
- remove the `MAX_CONCURRENT_WORKSPACE_LOADS` doc sentence "Git sources block a whole worker thread each" and the long `JoinSet`/`block_in_place` comment at `:252-263`; replace with: `// Each load runs on the blocking pool (spawn_blocking), so no runtime worker thread is ever occupied by git or YAML work (spec § 4.5 (0)).`
- `use stroem_common::budget::LoadBudget;` at the top.

- [ ] **Step 7: Integration-test sources** — each of the 4 impls in `tests/integration_test.rs` becomes (keep its name and revision string):

```rust
    impl stroem_server::workspace::WorkspaceSource for InMemSource {
        fn load(
            &self,
            _budget: &stroem_common::budget::LoadBudget,
        ) -> Result<stroem_server::workspace::LoadOutcome> {
            Ok(stroem_server::workspace::LoadOutcome {
                config: self.0.clone(),
                warnings: Vec::new(),
                revision: Some("test-rev".to_string()),
            })
        }
        fn path(&self) -> &std::path::Path {
            std::path::Path::new("/dev/null")
        }
    }
```

(drop `#[async_trait::async_trait]` and the `revision()` fn; `InMemSourceWithRev` returns `Some(EXPECTED_REVISION.to_string())`).

- [ ] **Step 8: Verify**

Run: `cargo test -p stroem-server --lib workspace && cargo check --workspace --tests && cargo clippy --workspace --all-targets -- -D warnings`
Expected: pass, including the 8 new git tests and 3 new folder tests; no `block_in_place` left: `grep -n "block_in_place" crates/stroem-server/src/workspace/git.rs` prints nothing.

- [ ] **Step 9: Commit**

```bash
git add crates/stroem-server
git commit -m "feat(workspace): synchronous source contract with Peek classification and git deadlines"
```

---

### Task 10: `workspace_reload:` config, libgit2 global timeouts, `new_with_reload`

**Files:**
- Modify: `crates/stroem-server/src/config.rs` (new struct, field on `ServerConfig`, `validate`, tests)
- Modify: every `ServerConfig { .. }` literal (76 sites across `crates/stroem-server/{src,tests}` and `crates/stroem-e2e/tests/harness.rs`)
- Modify: `crates/stroem-server/src/workspace/availability.rs` (`From` impl)
- Modify: `crates/stroem-server/src/workspace/git.rs` (`configure_global_timeouts`)
- Modify: `crates/stroem-server/src/workspace/mod.rs` (`new_with_reload`)
- Modify: `crates/stroem-server/src/main.rs:98-104`

**Interfaces:**
- Produces:
  - `config::WorkspaceReloadConfig { peek_failure_threshold: u32 (5), peek_timeout_secs: u64 (30), load_timeout_secs: u64 (300), max_backoff_secs: u64 (900), git_connect_timeout_ms: u32 (10_000), git_read_timeout_ms: u32 (60_000) }` + `Default`; `ServerConfig.workspace_reload: WorkspaceReloadConfig` (`#[serde(default)]`)
  - `impl From<&WorkspaceReloadConfig> for ReloadSettings`
  - `workspace::git::configure_global_timeouts(connect_ms: u32, read_ms: u32) -> anyhow::Result<()>`
  - `WorkspaceManager::new_with_reload(defs, library_defs, git_auth, settings: ReloadSettings) -> Self` (async); `new(..)` delegates with `ReloadSettings::default()`

- [ ] **Step 1: Failing config tests** — append to `config.rs` `mod tests`:

```rust
    #[test]
    fn workspace_reload_defaults_when_absent() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: /tmp/logs
worker_token: "0123456789abcdef0123456789abcdef"
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let r = &cfg.workspace_reload;
        assert_eq!(r.peek_failure_threshold, 5);
        assert_eq!(r.peek_timeout_secs, 30);
        assert_eq!(r.load_timeout_secs, 300);
        assert_eq!(r.max_backoff_secs, 900);
        assert_eq!(r.git_connect_timeout_ms, 10_000);
        assert_eq!(r.git_read_timeout_ms, 60_000);
    }

    #[test]
    fn workspace_reload_overrides_parse() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: /tmp/logs
worker_token: "0123456789abcdef0123456789abcdef"
workspace_reload:
  peek_failure_threshold: 3
  load_timeout_secs: 120
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.workspace_reload.peek_failure_threshold, 3);
        assert_eq!(cfg.workspace_reload.load_timeout_secs, 120);
        assert_eq!(cfg.workspace_reload.peek_timeout_secs, 30);
        cfg.validate().unwrap();
    }

    #[test]
    fn workspace_reload_rejects_zero_and_overflowing_values() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: /tmp/logs
worker_token: "0123456789abcdef0123456789abcdef"
"#;
        let base: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        for mutate in [
            (|c: &mut ServerConfig| c.workspace_reload.peek_failure_threshold = 0) as fn(&mut ServerConfig),
            |c| c.workspace_reload.peek_timeout_secs = 0,
            |c| c.workspace_reload.load_timeout_secs = 0,
            |c| c.workspace_reload.max_backoff_secs = 0,
            |c| c.workspace_reload.git_connect_timeout_ms = 0,
            |c| c.workspace_reload.git_read_timeout_ms = u32::MAX,
        ] {
            let mut c = base.clone();
            mutate(&mut c);
            assert!(c.validate().is_err());
        }
    }
```

(If `ServerConfig` does not derive `Clone`, check with `grep -n "pub struct ServerConfig" -B2 config.rs`; it derives `Clone` — the e2e harness calls `server_config.clone()`.)

Run: `cargo test -p stroem-server --lib config::tests::workspace_reload`
Expected: compile error — no field `workspace_reload`.

- [ ] **Step 2: Implement the config** — in `config.rs` next to `RecoveryConfig`:

```rust
/// Workspace watcher reload tuning (spec § 4). Server-level so env overrides
/// (`STROEM__WORKSPACE_RELOAD__LOAD_TIMEOUT_SECS=…`) coerce normally.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceReloadConfig {
    /// K — consecutive failed peeks before a forced load (default 5).
    #[serde(default = "default_peek_failure_threshold")]
    pub peek_failure_threshold: u32,
    /// P — budget for one peek, seconds (default 30).
    #[serde(default = "default_peek_timeout_secs")]
    pub peek_timeout_secs: u64,
    /// L — budget for one load, seconds (default 300).
    #[serde(default = "default_load_timeout_secs")]
    pub load_timeout_secs: u64,
    /// Cap of the retry backoff for a workspace in load error, seconds (default 900).
    #[serde(default = "default_max_backoff_secs")]
    pub max_backoff_secs: u64,
    /// libgit2 TCP connect timeout, milliseconds, process-wide (default 10 000).
    #[serde(default = "default_git_connect_timeout_ms")]
    pub git_connect_timeout_ms: u32,
    /// libgit2 per-read socket timeout, milliseconds, process-wide (default 60 000).
    #[serde(default = "default_git_read_timeout_ms")]
    pub git_read_timeout_ms: u32,
}

fn default_peek_failure_threshold() -> u32 {
    5
}
fn default_peek_timeout_secs() -> u64 {
    30
}
fn default_load_timeout_secs() -> u64 {
    300
}
fn default_max_backoff_secs() -> u64 {
    900
}
fn default_git_connect_timeout_ms() -> u32 {
    10_000
}
fn default_git_read_timeout_ms() -> u32 {
    60_000
}

impl Default for WorkspaceReloadConfig {
    fn default() -> Self {
        Self {
            peek_failure_threshold: default_peek_failure_threshold(),
            peek_timeout_secs: default_peek_timeout_secs(),
            load_timeout_secs: default_load_timeout_secs(),
            max_backoff_secs: default_max_backoff_secs(),
            git_connect_timeout_ms: default_git_connect_timeout_ms(),
            git_read_timeout_ms: default_git_read_timeout_ms(),
        }
    }
}
```

Add to `ServerConfig` after `default_job_timeout`:

```rust
    /// Workspace watcher reload tuning (spec § 4). Defaults apply when absent.
    #[serde(default)]
    pub workspace_reload: WorkspaceReloadConfig,
```

Append to `validate()` (before its final `Ok(())`):

```rust
        let r = &self.workspace_reload;
        if r.peek_failure_threshold == 0 {
            anyhow::bail!("workspace_reload.peek_failure_threshold must be at least 1");
        }
        if r.peek_timeout_secs == 0 || r.load_timeout_secs == 0 || r.max_backoff_secs == 0 {
            anyhow::bail!("workspace_reload timeouts and max_backoff_secs must be at least 1");
        }
        let c_int_max = i32::MAX as u32;
        if r.git_connect_timeout_ms == 0
            || r.git_read_timeout_ms == 0
            || r.git_connect_timeout_ms > c_int_max
            || r.git_read_timeout_ms > c_int_max
        {
            anyhow::bail!("workspace_reload git timeouts must be between 1 and {c_int_max} ms");
        }
```

- [ ] **Step 3: Fix all `ServerConfig` literals**

Run: `cargo check --workspace --tests 2>&1 | grep -c "missing field \`workspace_reload\`"`
For every reported site add `workspace_reload: Default::default(),` after that literal's last field. Repeat until `cargo check --workspace --tests` is clean. (Mechanical — 76 sites; `crates/stroem-e2e/tests/harness.rs` is one of them.)

- [ ] **Step 4: Settings conversion + git timeouts + constructor**

`availability.rs`:

```rust
impl From<&crate::config::WorkspaceReloadConfig> for ReloadSettings {
    fn from(c: &crate::config::WorkspaceReloadConfig) -> Self {
        Self {
            peek_failure_threshold: c.peek_failure_threshold,
            peek_timeout: Duration::from_secs(c.peek_timeout_secs),
            load_timeout: Duration::from_secs(c.load_timeout_secs),
            max_backoff: Duration::from_secs(c.max_backoff_secs),
        }
    }
}
```

`git.rs` (module level, `pub`):

```rust
/// Process-wide libgit2 socket timeouts (spec § 4.5): bound each TCP connect
/// and each socket read — including `peek_revision`. Call once at startup,
/// before any thread uses libgit2.
pub fn configure_global_timeouts(connect_ms: u32, read_ms: u32) -> Result<()> {
    // SAFETY: libgit2 requires global options to be set before other threads
    // use it. `main` calls this before `WorkspaceManager::new`, the first
    // libgit2 user; values are validated to fit a C int by `ServerConfig::validate`.
    unsafe {
        git2::opts::set_server_connect_timeout_in_milliseconds(connect_ms as libc::c_int)
            .context("set libgit2 connect timeout")?;
        git2::opts::set_server_timeout_in_milliseconds(read_ms as libc::c_int)
            .context("set libgit2 server timeout")?;
    }
    Ok(())
}
```

If `libc` is not a dependency of `stroem-server`, use `i32` instead of `libc::c_int` (they are the same type on all supported targets). Add to `git.rs` tests:

```rust
    #[test]
    fn configure_global_timeouts_applies_values() {
        configure_global_timeouts(10_000, 60_000).unwrap();
        unsafe {
            assert_eq!(git2::opts::get_server_connect_timeout_in_milliseconds().unwrap(), 10_000);
            assert_eq!(git2::opts::get_server_timeout_in_milliseconds().unwrap(), 60_000);
        }
    }
```

`mod.rs`: rename `pub async fn new(defs, library_defs, git_auth) -> Self` to `pub async fn new_with_reload(defs, library_defs, git_auth, settings: ReloadSettings) -> Self` (delete its local `let settings = ReloadSettings::default();`, store `settings` in the returned struct), and add:

```rust
    /// [`Self::new_with_reload`] with default reload settings (tests, tools).
    pub async fn new(
        defs: HashMap<String, WorkspaceSourceDef>,
        library_defs: HashMap<String, LibraryDef>,
        git_auth: HashMap<String, GitAuthConfig>,
    ) -> Self {
        Self::new_with_reload(defs, library_defs, git_auth, ReloadSettings::default()).await
    }
```

`main.rs` — replace the `WorkspaceManager::new(...).await` call with:

```rust
    stroem_server::workspace::git::configure_global_timeouts(
        config.workspace_reload.git_connect_timeout_ms,
        config.workspace_reload.git_read_timeout_ms,
    )?;
    let workspace_manager = WorkspaceManager::new_with_reload(
        config.workspaces.clone(),
        config.libraries.clone(),
        config.git_auth.clone(),
        stroem_server::workspace::availability::ReloadSettings::from(&config.workspace_reload),
    )
    .await;
```

- [ ] **Step 5: Verify**

Run: `cargo test -p stroem-server --lib -- config:: workspace:: && cargo check --workspace --tests && cargo clippy --workspace --all-targets -- -D warnings`
Expected: 3 new config tests + the git timeout test pass; everything compiles; no warnings.

- [ ] **Step 6: Commit**

```bash
git add -A crates/
git commit -m "feat(config): workspace_reload tuning and libgit2 global timeouts"
```

---

### Task 11: Detached finalizer; external callers `try_lock` (spec § 4.5 (1), (8))

**Files:**
- Create: `crates/stroem-server/src/workspace/lifecycle.rs`
- Create: `crates/stroem-server/src/workspace/test_support.rs` (`#[cfg(test)]`)
- Modify: `crates/stroem-server/src/workspace/mod.rs` (`reload`, `reload_for_api`, delete `do_reload`, test hooks)
- Modify: `crates/stroem-server/src/events.rs:392-405`, `crates/stroem-server/src/scheduler.rs:351-366`, `crates/stroem-server/src/web/hooks.rs:68-83`
- Test: `crates/stroem-server/tests/integration_test.rs` (one new test)

**Interfaces:**
- Consumes: Tasks 7–10
- Produces:
  - `workspace::lifecycle::ReloadBusy` (pub, re-exported as `workspace::ReloadBusy`)
  - `workspace::lifecycle::ReloadNotifier` trait (`fn notify(&self, workspace: String) -> BoxFuture<'static, ()>`), implemented for `crate::events::EventBus`
  - `pub(crate) struct LoadRequest { entry, libs, policy, guard: OwnedMutexGuard<ReloadState>, permit: Option<OwnedSemaphorePermit>, caller, op_id: Option<u64>, budget, notifier: Option<Arc<dyn ReloadNotifier>> }`
  - `pub(crate) fn spawn_load(LoadRequest) -> JoinHandle<Result<LoadSuccess>>`
  - `pub(crate) fn spawn_peek(entry: Arc<WorkspaceEntry>, policy: Policy, op_id: u64, budget: LoadBudget) -> JoinHandle<Peek>`
  - `pub(crate) fn jitter_offset(name: &str, poll: Duration) -> Duration`
  - `WorkspaceManager::hold_exec_for_test(&self, name) -> OwnedMutexGuard<ReloadState>` (`#[doc(hidden)] pub`); `#[cfg(test)] pub(crate) fn with_settings(self, ReloadSettings) -> Self`
  - `test_support::{TestSource, TestLoad, TestPeek}`

- [ ] **Step 1: Test support** — create `test_support.rs` and add `#[cfg(test)] pub(crate) mod test_support;` to `mod.rs`:

```rust
//! Scriptable `WorkspaceSource` for lifecycle tests.

use super::source::{LoadOutcome, Peek, WorkspaceSource};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stroem_common::budget::LoadBudget;
use stroem_common::models::workflow::WorkspaceConfig;

#[derive(Debug, Clone, Copy)]
pub(crate) enum TestLoad {
    Ok { action: &'static str, revision: &'static str },
    Err(&'static str),
    Panic,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TestPeek {
    Revision(&'static str),
    Failed,
    LocalInvalid,
    Hang(Duration),
}

pub(crate) struct TestSource {
    pub loads: AtomicUsize,
    pub started: AtomicBool,
    load: Mutex<TestLoad>,
    load_sleep: Mutex<Duration>,
    peek: Mutex<TestPeek>,
    /// When set, `load` spins until the flag is true.
    pub gate: Option<Arc<AtomicBool>>,
    pub poll_secs: u64,
}

pub(crate) fn config_with(action: &str) -> WorkspaceConfig {
    serde_yaml::from_str(&format!(
        "actions:\n  {action}:\n    type: script\n    script: echo hi\n"
    ))
    .unwrap()
}

impl TestSource {
    pub fn new(load: TestLoad, peek: TestPeek) -> Self {
        Self {
            loads: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            load: Mutex::new(load),
            load_sleep: Mutex::new(Duration::ZERO),
            peek: Mutex::new(peek),
            gate: None,
            poll_secs: 60,
        }
    }
    pub fn gated(mut self, gate: Arc<AtomicBool>) -> Self {
        self.gate = Some(gate);
        self
    }
    pub fn set_load(&self, load: TestLoad) {
        *self.load.lock().unwrap_or_else(|e| e.into_inner()) = load;
    }
    pub fn set_load_sleep(&self, d: Duration) {
        *self.load_sleep.lock().unwrap_or_else(|e| e.into_inner()) = d;
    }
    pub fn set_peek(&self, peek: TestPeek) {
        *self.peek.lock().unwrap_or_else(|e| e.into_inner()) = peek;
    }
    pub fn load_count(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }
}

impl WorkspaceSource for TestSource {
    fn load(&self, _budget: &LoadBudget) -> Result<LoadOutcome> {
        self.started.store(true, Ordering::SeqCst);
        self.loads.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            while !gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let sleep = *self.load_sleep.lock().unwrap_or_else(|e| e.into_inner());
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
        let load = *self.load.lock().unwrap_or_else(|e| e.into_inner());
        match load {
            TestLoad::Ok { action, revision } => Ok(LoadOutcome {
                config: config_with(action),
                warnings: Vec::new(),
                revision: Some(revision.to_string()),
            }),
            TestLoad::Err(msg) => Err(anyhow!("{msg}")),
            TestLoad::Panic => panic!("test source panicked"),
        }
    }

    fn path(&self) -> &Path {
        Path::new("/dev/null")
    }

    fn peek_revision(&self, _budget: &LoadBudget) -> Peek {
        let peek = *self.peek.lock().unwrap_or_else(|e| e.into_inner());
        match peek {
            TestPeek::Revision(r) => Peek::Revision(r.to_string()),
            TestPeek::Failed => Peek::Failed(anyhow!("remote unreachable")),
            TestPeek::LocalInvalid => Peek::LocalInvalid(anyhow!("no clone")),
            TestPeek::Hang(d) => {
                std::thread::sleep(d);
                Peek::Failed(anyhow!("hung"))
            }
        }
    }

    fn poll_interval_secs(&self) -> u64 {
        self.poll_secs
    }
}
```

- [ ] **Step 2: Write `lifecycle.rs`** and add `pub mod lifecycle;` + `pub use lifecycle::ReloadBusy;` to `mod.rs`:

```rust
//! Worker / finalizer / observer execution of loads and peeks (spec § 4.5).
//!
//! - worker: `spawn_blocking` — the only layer that may block, hang or panic;
//! - finalizer: a detached task that owns the execution guard (and permit),
//!   applies the result through the single writer, then releases, then
//!   notifies. Cancelling whoever awaits it never cancels it;
//! - observer: the caller, which may stop waiting.

use super::availability::{Caller, Event, Policy};
use super::entry::{LoadSuccess, ReloadState, WorkspaceEntry};
use super::library::ResolvedLibrary;
use super::source::Peek;
use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stroem_common::budget::LoadBudget;
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit};
use tokio::task::JoinHandle;

/// A reload could not start: another load of this workspace holds its
/// execution mutex. Never a load outcome (spec § 4.5 (2), (8)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadBusy;

impl std::fmt::Display for ReloadBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a reload of this workspace is already in progress")
    }
}

impl std::error::Error for ReloadBusy {}

/// Announces a successful watcher reload to peer replicas — a seam so tests
/// can stall it.
pub trait ReloadNotifier: Send + Sync {
    fn notify(&self, workspace: String) -> BoxFuture<'static, ()>;
}

impl ReloadNotifier for crate::events::EventBus {
    fn notify(&self, workspace: String) -> BoxFuture<'static, ()> {
        let bus = self.clone();
        Box::pin(async move { bus.publish_workspace_reloaded(&workspace).await })
    }
}

pub(crate) struct LoadRequest {
    pub entry: Arc<WorkspaceEntry>,
    pub libs: Arc<HashMap<String, ResolvedLibrary>>,
    pub policy: Policy,
    pub guard: OwnedMutexGuard<ReloadState>,
    pub permit: Option<OwnedSemaphorePermit>,
    pub caller: Caller,
    pub op_id: Option<u64>,
    pub budget: LoadBudget,
    pub notifier: Option<Arc<dyn ReloadNotifier>>,
}

/// Start a load. The returned handle is the FINALIZER's: dropping it, or
/// timing out on it, never cancels the load or releases its guard early.
pub(crate) fn spawn_load(req: LoadRequest) -> JoinHandle<Result<LoadSuccess>> {
    tokio::spawn(async move {
        let LoadRequest { entry, libs, policy, mut guard, permit, caller, op_id, budget, notifier } = req;

        // 1. worker
        let source = Arc::clone(&entry.source);
        let outcome = match tokio::task::spawn_blocking(move || source.load(&budget)).await {
            Ok(result) => result,
            Err(join_err) => Err(anyhow!("workspace load panicked: {join_err}")),
        };

        // 2. the single writer
        let completed_at = Instant::now();
        let result = entry.apply_load_result(caller, op_id, outcome, &libs, &policy, completed_at);

        // 3. API cooldown bookkeeping, while the guard is still held
        if caller == Caller::External {
            guard.last_completed = Some(completed_at);
        }

        // 4. release BEFORE notifying — a stalled notification holds nothing
        drop(guard);
        drop(permit);

        // 5. best-effort, detached peer notification (today's rule: a
        //    successful watcher load that changed the revision)
        if let (Some(notifier), Ok(success)) = (notifier, &result) {
            if caller == Caller::Watcher && success.revision_changed {
                tokio::spawn(notifier.notify(entry.name.clone()));
            }
        }
        result
    })
}

/// Start a peek. The finalizer ALWAYS emits `PeekFinished` (clearing the
/// in-flight record), even after a panic; only the observer applies outcomes.
pub(crate) fn spawn_peek(
    entry: Arc<WorkspaceEntry>,
    policy: Policy,
    op_id: u64,
    budget: LoadBudget,
) -> JoinHandle<Peek> {
    tokio::spawn(async move {
        let source = Arc::clone(&entry.source);
        let peek = match tokio::task::spawn_blocking(move || source.peek_revision(&budget)).await {
            Ok(peek) => peek,
            Err(join_err) => Peek::Failed(anyhow!("workspace peek panicked: {join_err}")),
        };
        entry.transition(Event::PeekFinished { op_id }, &policy);
        peek
    })
}

/// Deterministic per-workspace watcher start offset in `[0, poll)`, so N
/// watchers do not tick together (spec § 4.4).
pub(crate) fn jitter_offset(name: &str, poll: Duration) -> Duration {
    let millis = poll.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    Duration::from_millis(hasher.finish() % millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_offsets_differ_and_stay_below_the_poll_interval() {
        let poll = Duration::from_secs(60);
        let offsets: Vec<_> = ["a", "b", "jobs", "jobs_beta", "ai_traffic_model"]
            .iter()
            .map(|n| jitter_offset(n, poll))
            .collect();
        assert!(offsets.iter().all(|o| *o < poll));
        let unique: std::collections::HashSet<_> = offsets.iter().collect();
        assert!(unique.len() >= 4, "offsets should spread out: {offsets:?}");
        assert_eq!(jitter_offset("a", poll), jitter_offset("a", poll), "deterministic");
        assert_eq!(jitter_offset("a", Duration::ZERO), Duration::ZERO);
    }
}
```

Check `futures-util` is a normal (not dev) dependency of `stroem-server` (`Cargo.toml:61-62` lists `futures-util = "0.3"`); if `BoxFuture` is not exported under that feature set, use `std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>` instead.

- [ ] **Step 3: External callers** — in `mod.rs` replace `reload`, `reload_for_api`, and delete `do_reload`:

```rust
    /// Reload a workspace now (peer notification, scheduler/webhook
    /// `force_refresh`, tests). Returns `Err(ReloadBusy)` — check with
    /// `err.downcast_ref::<ReloadBusy>()` — if another load of this
    /// workspace is running; never waits for it (spec § 4.5 (8)).
    pub async fn reload(&self, name: &str) -> Result<()> {
        let entry = self
            .entries
            .get(name)
            .with_context(|| format!("Workspace '{}' not found", name))?
            .clone();
        let guard = entry
            .exec()
            .try_lock_owned()
            .map_err(|_| anyhow::Error::new(ReloadBusy))?;
        self.run_external_load(entry, guard)
            .await
            .with_context(|| format!("Failed to reload workspace '{}'", name))
    }

    pub async fn reload_for_api(
        &self,
        name: &str,
        cooldown: Duration,
    ) -> std::result::Result<(), ReloadApiError> {
        let entry = self.entries.get(name).ok_or(ReloadApiError::NotFound)?.clone();
        // try_lock: an in-flight reload surfaces as a cooldown, never a queue.
        let guard = match entry.exec().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                return Err(ReloadApiError::Cooldown {
                    retry_after_secs: cooldown.as_secs().max(1),
                })
            }
        };
        if let Some(last) = guard.last_completed {
            let elapsed = last.elapsed();
            if elapsed < cooldown {
                let retry_after = (cooldown - elapsed).as_secs().max(1);
                return Err(ReloadApiError::Cooldown { retry_after_secs: retry_after });
            }
        }
        self.run_external_load(entry, guard)
            .await
            .map_err(ReloadApiError::Failed)
    }

    /// External load through the detached finalizer: no permit, no watchdog,
    /// but guard ownership and result application survive the caller being
    /// cancelled (spec § 4.5 (8)).
    async fn run_external_load(
        &self,
        entry: Arc<WorkspaceEntry>,
        guard: tokio::sync::OwnedMutexGuard<ReloadState>,
    ) -> Result<()> {
        let policy = self.settings.policy(entry.poll_interval());
        let handle = lifecycle::spawn_load(lifecycle::LoadRequest {
            entry,
            libs: Arc::clone(&self.resolved_libraries),
            policy,
            guard,
            permit: None,
            caller: Caller::External,
            op_id: None,
            budget: LoadBudget::from_now(self.settings.load_timeout),
            notifier: None,
        });
        handle
            .await
            .map_err(|e| anyhow::anyhow!("workspace reload task failed: {e}"))?
            .map(|_| ())
    }

    /// Test hook: occupy a workspace's execution mutex, as a running load does.
    #[doc(hidden)]
    pub async fn hold_exec_for_test(&self, name: &str) -> tokio::sync::OwnedMutexGuard<ReloadState> {
        self.entries
            .get(name)
            .expect("hold_exec_for_test: no entry")
            .exec()
            .lock_owned()
            .await
    }

    #[cfg(test)]
    pub(crate) fn with_settings(mut self, settings: ReloadSettings) -> Self {
        self.settings = settings;
        self
    }
```

- [ ] **Step 4: Caller handling of `ReloadBusy`**

`events.rs` — replace the `Ok(p) => { if let Err(e) = state.workspaces.reload(..) ... }` arm with:

```rust
                Ok(p) => match state.workspaces.reload(&p.workspace).await {
                    Ok(()) => tracing::debug!(
                        "Cross-replica reload applied for workspace '{}'",
                        p.workspace
                    ),
                    Err(e) if e.downcast_ref::<crate::workspace::ReloadBusy>().is_some() => {
                        // Convergence then relies on this replica's own watcher
                        // (spec § 4.5 (8)); peer reloads never rebroadcast.
                        tracing::debug!(
                            "Cross-replica reload of '{}' skipped: a reload is already in progress",
                            p.workspace
                        )
                    }
                    Err(e) => tracing::warn!(
                        "Cross-replica workspace reload failed for '{}': {:#}",
                        p.workspace,
                        e
                    ),
                },
```

`scheduler.rs` (and the same shape in `web/hooks.rs` with `"Webhook '{}'"`/`name` and `state.workspaces`/`state.event_bus`):

```rust
    if tstate.force_refresh {
        match app_state.workspaces.reload(&tstate.workspace).await {
            Ok(()) => {
                // Notify peer replicas that the workspace has been refreshed so
                // they converge without waiting for their own poll tick.
                app_state
                    .event_bus
                    .publish_workspace_reloaded(&tstate.workspace)
                    .await;
            }
            Err(e) if e.downcast_ref::<crate::workspace::ReloadBusy>().is_some() => {
                // New policy (spec § 4.5 (8)): fire from the published snapshot
                // if it is healthy; an errored workspace is still MISSED below.
                tracing::info!(
                    "Trigger '{}': force_refresh skipped — a reload is already in progress",
                    source_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Trigger '{}': force_refresh failed, continuing with cached revision: {:#}",
                    source_id,
                    e
                );
            }
        }
    }
```

- [ ] **Step 5: Lifecycle tests** — append to `mod.rs` `mod tests`:

```rust
    use crate::workspace::test_support::{config_with, TestLoad, TestPeek, TestSource};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn manager_with(source: Arc<TestSource>) -> Arc<WorkspaceManager> {
        let mut entries = HashMap::new();
        entries.insert(
            "ws".to_string(),
            WorkspaceEntry::new("ws", source, config_with("a"), Some("rev-a".to_string())),
        );
        Arc::new(WorkspaceManager::from_entries(entries))
    }

    async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Round-5 major: cancelling an external caller must not release the
    /// execution mutex mid-mutation or skip result application.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_api_reload_keeps_the_guard_and_still_applies_its_result() {
        let gate = Arc::new(AtomicBool::new(false));
        let source = Arc::new(
            TestSource::new(TestLoad::Ok { action: "b", revision: "rev-b" }, TestPeek::Failed)
                .gated(gate.clone()),
        );
        let mgr = manager_with(source.clone());

        let caller = tokio::spawn({
            let mgr = mgr.clone();
            async move { mgr.reload_for_api("ws", Duration::ZERO).await }
        });
        wait_until("load to start", || source.started.load(Ordering::SeqCst)).await;
        caller.abort();
        let _ = caller.await;

        let busy = mgr.reload("ws").await.unwrap_err();
        assert!(busy.downcast_ref::<ReloadBusy>().is_some(), "mutex must still be held: {busy:#}");
        assert!(matches!(
            mgr.reload_for_api("ws", Duration::ZERO).await,
            Err(ReloadApiError::Cooldown { .. })
        ));

        gate.store(true, Ordering::SeqCst);
        wait_until("result to be applied", || mgr.get_revision("ws").as_deref() == Some("rev-b")).await;
        wait_until("mutex release", || mgr.entry("ws").unwrap().exec().try_lock().is_ok()).await;

        // The cancelled request still started the completion-based cooldown...
        assert!(matches!(
            mgr.reload_for_api("ws", Duration::from_secs(3600)).await,
            Err(ReloadApiError::Cooldown { .. })
        ));
        // ...and once the window allows, a retry refreshes again.
        assert!(mgr.reload_for_api("ws", Duration::ZERO).await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn panicking_load_is_a_failed_load_and_releases_the_mutex() {
        let source = Arc::new(TestSource::new(TestLoad::Panic, TestPeek::Failed));
        let mgr = manager_with(source);
        let err = mgr.reload("ws").await.unwrap_err();
        assert!(format!("{err:#}").contains("panicked"), "{err:#}");
        assert!(mgr.get_config("ws").await.is_none());
        assert!(mgr.entry("ws").unwrap().availability().is_errored());
        let again = mgr.reload("ws").await.unwrap_err();
        assert!(again.downcast_ref::<ReloadBusy>().is_none(), "mutex must be free after a panic");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_reload_while_busy_returns_immediately() {
        let source = Arc::new(TestSource::new(TestLoad::Ok { action: "a", revision: "rev-a" }, TestPeek::Failed));
        let mgr = manager_with(source.clone());
        let _held = mgr.hold_exec_for_test("ws").await;
        let started = Instant::now();
        let err = mgr.reload("ws").await.unwrap_err();
        assert!(err.downcast_ref::<ReloadBusy>().is_some());
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(source.load_count(), 0);
    }
```

Integration test — append to `tests/integration_test.rs` next to `test_trigger_fire_on_unavailable_workspace_has_no_side_effects`:

```rust
/// Spec § 4.5 (8): a busy force_refresh fires from a healthy snapshot and is
/// still MISSED when the workspace is errored.
#[tokio::test]
async fn test_force_refresh_while_busy_fires_only_from_a_healthy_snapshot() -> Result<()> {
    use stroem_common::models::workflow::{ConcurrencyPolicy, TriggerDef};

    let (state, pool, _tmp, _container) = setup_recovery().await?;
    let mut cfg = (*state.get_workspace("default").await.unwrap()).clone();
    cfg.triggers.insert(
        "refresh-busy".to_string(),
        TriggerDef::Scheduler {
            cron: "0 1 * * *".to_string(),
            task: "hello-world".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: ConcurrencyPolicy::Allow,
            timezone: None,
            force_refresh: true,
        },
    );
    state.workspaces.replace_config_for_test("default", cfg.clone()).await;
    let remembered = WorkspaceManager::from_config("default", cfg);
    let source_id = "default/refresh-busy";
    let rows = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM job WHERE source_type = 'trigger' AND source_id = $1",
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await
    };

    let _busy = state.workspaces.hold_exec_for_test("default").await;
    stroem_server::scheduler::fire_trigger_once(&state, &state.workspaces, &remembered, source_id).await;
    assert_eq!(rows(pool.clone()).await?, 1, "busy + healthy ⇒ fires from the snapshot");

    state.workspaces.mark_unavailable_for_test("default");
    stroem_server::scheduler::fire_trigger_once(&state, &state.workspaces, &remembered, source_id).await;
    assert_eq!(rows(pool.clone()).await?, 1, "busy + errored ⇒ MISSED");
    Ok(())
}
```

- [ ] **Step 6: Verify**

Run: `cargo test -p stroem-server --lib workspace && cargo test -p stroem-server --test integration_test test_force_refresh_while_busy && cargo clippy --workspace --all-targets -- -D warnings`
Expected: pass (existing `test_reload_for_api_*` tests included); no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server
git commit -m "feat(workspace): detached load finalizer; external reloads try_lock and never queue"
```

---

### Task 12: The watcher — peek policy, admission, watchdog (spec §§ 4.2, 4.4, 4.5 (1)–(7))

**Files:**
- Create: `crates/stroem-server/src/workspace/watcher.rs`
- Modify: `crates/stroem-server/src/workspace/mod.rs` (`load_permits` field, `start_watchers`)
- Modify: `crates/stroem-server/src/metrics.rs` (two counter constants)

**Interfaces:**
- Consumes: Tasks 7–11
- Produces:
  - `metrics::STROEM_WORKSPACE_PEEK_FAILURES_TOTAL = "stroem_workspace_peek_failures_total"` (label `workspace`), `metrics::STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL = "stroem_workspace_load_admission_skipped_total"` (labels `workspace`, `reason` = `busy`|`saturated`)
  - `pub(crate) struct WatcherCtx { entry: Arc<WorkspaceEntry>, libs: Arc<HashMap<String, ResolvedLibrary>>, permits: Arc<Semaphore>, settings: ReloadSettings, notifier: Option<Arc<dyn ReloadNotifier>> }`
  - `pub(crate) async fn run_watcher(ctx: WatcherCtx, cancel: CancellationToken)`, `pub(crate) async fn watcher_tick(ctx: &WatcherCtx, policy: &Policy)`, `pub(crate) async fn attempt_watcher_load(ctx: &WatcherCtx, policy: &Policy)`
  - `WorkspaceManager.load_permits: Arc<Semaphore>` (`MAX_CONCURRENT_WORKSPACE_LOADS` permits, created in every constructor); `pub fn load_permits_available(&self) -> usize`

- [ ] **Step 1: Metric constants** — add to `metrics.rs` after `STROEM_BACKGROUND_TASK_LAST_TICK_AGE_SECONDS`, and add both to the `metric_name_constants_are_distinct` list:

```rust
/// `counter` — workspace peeks that failed or timed out. Label: workspace.
pub const STROEM_WORKSPACE_PEEK_FAILURES_TOTAL: &str = "stroem_workspace_peek_failures_total";
/// `counter` — watcher loads not admitted. Labels: workspace, reason (`busy`, `saturated`).
pub const STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL: &str =
    "stroem_workspace_load_admission_skipped_total";
```

- [ ] **Step 2: Write `watcher.rs`** (add `mod watcher;` to `mod.rs`):

```rust
//! Per-workspace watcher (spec § 4). One tick = at most one peek and at most
//! one admitted load; the loop never waits on a mutex or a permit.

use super::availability::{Caller, Effect, Event, PeekObservation, Policy, ReloadSettings};
use super::entry::WorkspaceEntry;
use super::library::ResolvedLibrary;
use super::lifecycle::{jitter_offset, spawn_load, spawn_peek, LoadRequest, ReloadNotifier};
use super::source::Peek;
use crate::metrics::{STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL, STROEM_WORKSPACE_PEEK_FAILURES_TOTAL};
use metrics::counter;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stroem_common::budget::LoadBudget;
use tokio::sync::Semaphore;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

pub(crate) struct WatcherCtx {
    pub entry: Arc<WorkspaceEntry>,
    pub libs: Arc<HashMap<String, ResolvedLibrary>>,
    pub permits: Arc<Semaphore>,
    pub settings: ReloadSettings,
    pub notifier: Option<Arc<dyn ReloadNotifier>>,
}

pub(crate) async fn run_watcher(ctx: WatcherCtx, cancel: CancellationToken) {
    let poll = ctx.entry.poll_interval();
    let policy = ctx.settings.policy(poll);
    let offset = jitter_offset(&ctx.entry.name, poll);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + offset, poll);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tracing::info!(
        "Watcher started for workspace '{}' (poll interval: {}s, first check in {:?})",
        ctx.entry.name,
        poll.as_secs(),
        offset
    );
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            () = cancel.cancelled() => {
                tracing::info!("Watcher for workspace '{}' stopping (shutdown)", ctx.entry.name);
                break;
            }
        }
        watcher_tick(&ctx, &policy).await;
    }
}

pub(crate) async fn watcher_tick(ctx: &WatcherCtx, policy: &Policy) {
    let entry = &ctx.entry;
    if entry.availability().peek_in_flight.is_some() {
        // The previous peek's observer gave up; this tick counts as a failure.
        counter!(STROEM_WORKSPACE_PEEK_FAILURES_TOTAL, "workspace" => entry.name.clone()).increment(1);
    }
    let effect = match entry.transition(Event::Tick { now: Instant::now() }, policy) {
        Effect::Peek => peek_once(ctx, policy).await,
        other => other,
    };
    match effect {
        Effect::AttemptLoad => attempt_watcher_load(ctx, policy).await,
        Effect::PeekFailureSkipped { first: true } => tracing::warn!(
            "Workspace '{}': could not check for changes; keeping the last loaded config \
             (a forced reload follows after {} consecutive failures)",
            entry.name,
            policy.peek_failure_threshold
        ),
        _ => {}
    }
}

/// Observer side of a peek (spec § 4.5 (7)).
async fn peek_once(ctx: &WatcherCtx, policy: &Policy) -> Effect {
    let entry = &ctx.entry;
    let op_id = entry.next_op_id();
    let started_at = Instant::now();
    let deadline = started_at + ctx.settings.peek_timeout;
    entry.transition(Event::PeekStarted { op_id, started_at, deadline }, policy);
    let handle = spawn_peek(Arc::clone(entry), *policy, op_id, LoadBudget::until(deadline));
    let event = match tokio::time::timeout_at(deadline.into(), handle).await {
        Ok(Ok(peek)) => {
            if let Peek::Failed(e) = &peek {
                tracing::debug!("Workspace '{}': peek failed: {:#}", entry.name, e);
            }
            Event::PeekCompleted { observation: observe(&peek, entry.published().revision.as_deref()) }
        }
        Ok(Err(join_err)) => {
            tracing::error!("Workspace '{}': peek finalizer failed: {}", entry.name, join_err);
            Event::PeekCompleted { observation: PeekObservation::Failed }
        }
        Err(_elapsed) => Event::PeekTimedOut,
    };
    if matches!(
        event,
        Event::PeekTimedOut | Event::PeekCompleted { observation: PeekObservation::Failed }
    ) {
        counter!(STROEM_WORKSPACE_PEEK_FAILURES_TOTAL, "workspace" => entry.name.clone()).increment(1);
    }
    entry.transition(event, policy)
}

fn observe(peek: &Peek, published: Option<&str>) -> PeekObservation {
    match peek {
        Peek::Revision(r) if Some(r.as_str()) == published => PeekObservation::Matches,
        Peek::Revision(_) => PeekObservation::Differs,
        Peek::Unsupported | Peek::LocalInvalid(_) => PeekObservation::NeedsLoad,
        Peek::Failed(_) => PeekObservation::Failed,
    }
}

/// Ordered, non-blocking admission; then observe the load (spec § 4.5 (1)–(2)).
pub(crate) async fn attempt_watcher_load(ctx: &WatcherCtx, policy: &Policy) {
    let entry = &ctx.entry;
    let Ok(guard) = entry.exec().try_lock_owned() else {
        counter!(STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL, "workspace" => entry.name.clone(), "reason" => "busy").increment(1);
        return;
    };
    let Ok(permit) = Arc::clone(&ctx.permits).try_acquire_owned() else {
        drop(guard);
        counter!(STROEM_WORKSPACE_LOAD_ADMISSION_SKIPPED_TOTAL, "workspace" => entry.name.clone(), "reason" => "saturated").increment(1);
        return;
    };
    let op_id = entry.next_op_id();
    let started_at = Instant::now();
    let deadline = started_at + ctx.settings.load_timeout;
    entry.transition(Event::LoadStarted { op_id, started_at, deadline }, policy);
    tracing::info!("Workspace '{}': reloading", entry.name);

    let handle = spawn_load(LoadRequest {
        entry: Arc::clone(entry),
        libs: Arc::clone(&ctx.libs),
        policy: *policy,
        guard,
        permit: Some(permit),
        caller: Caller::Watcher,
        op_id: Some(op_id),
        budget: LoadBudget::until(deadline),
        notifier: ctx.notifier.clone(),
    });
    match tokio::time::timeout_at(deadline.into(), handle).await {
        Ok(Ok(Ok(success))) => tracing::info!(
            "Workspace '{}' reloaded (revision: {:?}, changed: {})",
            entry.name,
            entry.published().revision.as_deref().map(|s| &s[..8.min(s.len())]),
            success.revision_changed
        ),
        Ok(Ok(Err(e))) => tracing::warn!("Failed to reload workspace '{}': {:#}", entry.name, e),
        Ok(Err(join_err)) => tracing::error!("Workspace '{}': load finalizer failed: {}", entry.name, join_err),
        Err(_elapsed) => tracing::warn!(
            "Workspace '{}': reload exceeded {:?}; it keeps running and still holds its slot (overdue)",
            entry.name,
            ctx.settings.load_timeout
        ),
    }
}

// Tests: Step 4.
```

`deadline.into()`: std `Instant` → `tokio::time::Instant` via `tokio::time::Instant::from_std(deadline)` if `From` is not implemented; use `tokio::time::Instant::from_std(deadline)` explicitly in both places.

- [ ] **Step 3: Manager wiring** (`mod.rs`)
- add field `load_permits: Arc<Semaphore>` to `WorkspaceManager`; every constructor sets `load_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_WORKSPACE_LOADS))`. Update the constant's doc: `/// Upper bound on concurrent loads: startup loads, and every watcher load (a permit is held until the load really finishes, spec § 4.5). External reloads take no permit.` In `new_with_reload` create the semaphore once and use it for BOTH the startup `JoinSet` (replace its local `semaphore`) and the stored field.
- `pub fn load_permits_available(&self) -> usize { self.load_permits.available_permits() }`
- replace the whole `start_watchers` body:

```rust
    pub fn start_watchers(
        &self,
        cancel_token: CancellationToken,
        event_bus: Option<crate::events::EventBus>,
    ) {
        let notifier: Option<Arc<dyn lifecycle::ReloadNotifier>> =
            event_bus.map(|bus| Arc::new(bus) as Arc<dyn lifecycle::ReloadNotifier>);
        for entry in self.entries.values() {
            let ctx = watcher::WatcherCtx {
                entry: Arc::clone(entry),
                libs: Arc::clone(&self.resolved_libraries),
                permits: Arc::clone(&self.load_permits),
                settings: self.settings,
                notifier: notifier.clone(),
            };
            tokio::spawn(watcher::run_watcher(ctx, cancel_token.clone()));
        }
    }
```

  and update its doc comment: peek failures skip the tick (spec § 4.2); errored workspaces retry on the backoff ladder; a successful reload that changed the revision notifies peers.

- [ ] **Step 4: Watcher tests** — append to `watcher.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::availability::Freshness;
    use crate::workspace::test_support::{config_with, TestLoad, TestPeek, TestSource};
    use std::sync::atomic::Ordering;

    fn settings() -> ReloadSettings {
        ReloadSettings {
            peek_failure_threshold: 3,
            peek_timeout: Duration::from_millis(100),
            load_timeout: Duration::from_millis(200),
            max_backoff: Duration::from_secs(900),
        }
    }

    fn ctx_with(source: Arc<TestSource>, permits: usize) -> (WatcherCtx, Policy) {
        let entry = Arc::new(WorkspaceEntry::new("ws", source, config_with("a"), Some("rev-a".to_string())));
        let s = settings();
        let policy = s.policy(entry.poll_interval());
        let ctx = WatcherCtx {
            entry,
            libs: Arc::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(permits)),
            settings: s,
            notifier: None,
        };
        (ctx, policy)
    }

    fn src(load: TestLoad, peek: TestPeek) -> Arc<TestSource> {
        Arc::new(TestSource::new(load, peek))
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_peek_skips_and_keeps_serving() {
        let source = src(TestLoad::Ok { action: "b", revision: "rev-b" }, TestPeek::Failed);
        let (ctx, policy) = ctx_with(source.clone(), 8);
        watcher_tick(&ctx, &policy).await;
        assert_eq!(source.load_count(), 0, "a peek failure must not trigger a load");
        assert!(ctx.entry.published().config.actions.contains_key("a"));
        assert_eq!(ctx.entry.availability().freshness, Freshness::Fresh { consecutive_peek_failures: 1 });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn k_consecutive_peek_failures_force_exactly_one_load() {
        let source = src(TestLoad::Ok { action: "a", revision: "rev-a" }, TestPeek::Failed);
        let (ctx, policy) = ctx_with(source.clone(), 8);
        for _ in 0..3 {
            watcher_tick(&ctx, &policy).await;
        }
        assert_eq!(source.load_count(), 1);
        assert_eq!(ctx.entry.availability().freshness, Freshness::Fresh { consecutive_peek_failures: 0 });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_revision_reloads_and_a_matching_one_does_not() {
        let source = src(TestLoad::Ok { action: "b", revision: "rev-b" }, TestPeek::Revision("rev-a"));
        let (ctx, policy) = ctx_with(source.clone(), 8);
        watcher_tick(&ctx, &policy).await;
        assert_eq!(source.load_count(), 0);
        source.set_peek(TestPeek::Revision("rev-b"));
        watcher_tick(&ctx, &policy).await;
        assert_eq!(source.load_count(), 1);
        assert_eq!(ctx.entry.published().revision.as_deref(), Some("rev-b"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hung_peek_times_out_blocks_new_peeks_and_escalates() {
        let source = src(TestLoad::Ok { action: "a", revision: "rev-a" }, TestPeek::Hang(Duration::from_millis(600)));
        let (ctx, policy) = ctx_with(source.clone(), 8);
        watcher_tick(&ctx, &policy).await; // times out after 100 ms → failure 1
        assert!(ctx.entry.availability().peek_in_flight.is_some());
        watcher_tick(&ctx, &policy).await; // still in flight → failure 2, no new peek
        watcher_tick(&ctx, &policy).await; // failure 3 = K → forced load
        assert_eq!(source.load_count(), 1);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(ctx.entry.availability().peek_in_flight.is_none(), "finalizer clears the record");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn busy_and_saturated_admission_change_nothing() {
        let source = src(TestLoad::Ok { action: "b", revision: "rev-b" }, TestPeek::Revision("rev-b"));
        let (ctx, policy) = ctx_with(source.clone(), 8);
        let held = ctx.entry.exec().lock_owned().await;
        let before = ctx.entry.availability();
        attempt_watcher_load(&ctx, &policy).await;
        assert_eq!(ctx.entry.availability(), before, "Busy is not a transition");
        drop(held);

        let (sat, policy) = ctx_with(source.clone(), 0);
        let before = sat.entry.availability();
        attempt_watcher_load(&sat, &policy).await;
        assert_eq!(sat.entry.availability(), before, "Saturated is not a transition");
        assert!(sat.entry.exec().try_lock().is_ok(), "Saturated must release the mutex");
        assert_eq!(source.load_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_overdue_load_keeps_its_slot_and_its_late_success_publishes() {
        let source = src(TestLoad::Ok { action: "b", revision: "rev-b" }, TestPeek::Failed);
        source.set_load_sleep(Duration::from_millis(500));
        let (ctx, policy) = ctx_with(source.clone(), 1);
        attempt_watcher_load(&ctx, &policy).await; // observer gives up at 200 ms
        assert!(ctx.entry.availability().load_overdue(Instant::now()));
        assert_eq!(ctx.permits.available_permits(), 0, "permit held until real completion");
        assert!(ctx.entry.exec().try_lock().is_err(), "mutex held until real completion");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(ctx.entry.published().revision.as_deref(), Some("rev-b"));
        assert!(!ctx.entry.availability().load_overdue(Instant::now()));
        assert_eq!(ctx.permits.available_permits(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_failure_enters_errored() {
        let source = src(TestLoad::Err("boom"), TestPeek::Failed);
        source.set_load_sleep(Duration::from_millis(400));
        let (ctx, policy) = ctx_with(source, 8);
        attempt_watcher_load(&ctx, &policy).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(ctx.entry.availability().is_errored());
        assert!(ctx.entry.published().error.is_some());
    }

    /// Round-3 regression: the watchdog must fire on a ONE-worker runtime
    /// while the load blocks (it runs on the blocking pool).
    #[test]
    fn the_observer_times_out_on_a_single_worker_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let source = src(TestLoad::Ok { action: "b", revision: "rev-b" }, TestPeek::Failed);
            source.set_load_sleep(Duration::from_secs(2));
            let (ctx, policy) = ctx_with(source, 8);
            let started = Instant::now();
            attempt_watcher_load(&ctx, &policy).await;
            assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
        });
        rt.shutdown_timeout(Duration::from_millis(10));
    }

    struct StalledNotifier;
    impl ReloadNotifier for StalledNotifier {
        fn notify(&self, _ws: String) -> futures_util::future::BoxFuture<'static, ()> {
            Box::pin(std::future::pending())
        }
    }

    /// Round-5 major: a stalled notification must not hold the permit or mutex.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_notification_holds_nothing() {
        let source = src(TestLoad::Ok { action: "b", revision: "rev-b" }, TestPeek::Failed);
        let (mut ctx, policy) = ctx_with(source, 1);
        ctx.notifier = Some(Arc::new(StalledNotifier));
        attempt_watcher_load(&ctx, &policy).await;
        settle().await;
        assert_eq!(ctx.entry.published().revision.as_deref(), Some("rev-b"));
        assert_eq!(ctx.permits.available_permits(), 1);
        assert!(ctx.entry.exec().try_lock().is_ok());
    }

    /// Round-2 regression, end to end: an external failure is retried by the watcher.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_external_failure_is_retried_on_the_backoff_ladder() {
        let source = src(TestLoad::Err("secret render failed"), TestPeek::Revision("rev-a"));
        let (ctx, policy) = ctx_with(source.clone(), 8);
        let _ = ctx.entry.apply_load_result(
            Caller::External,
            None,
            Err(anyhow::anyhow!("secret render failed")),
            &HashMap::new(),
            &policy,
            Instant::now() - policy.poll_interval, // next_attempt is now
        );
        source.set_load(TestLoad::Ok { action: "a", revision: "rev-a" });
        watcher_tick(&ctx, &policy).await;
        assert_eq!(source.load_count(), 1, "errored workspace must be retried without peeking");
        assert!(!ctx.entry.availability().is_errored());
    }

    /// Spec § 11 E2E substitute: a failing peek keeps serving the last config.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_workspace_whose_peek_keeps_failing_stays_servable() {
        let source = src(TestLoad::Err("fetch failed"), TestPeek::Failed);
        let (ctx, policy) = ctx_with(source, 8);
        for _ in 0..2 {
            watcher_tick(&ctx, &policy).await;
        }
        assert!(ctx.entry.is_healthy(), "config must still be served below K");
        assert!(ctx.entry.published().config.actions.contains_key("a"));
    }
}
```

If `Instant::now() - policy.poll_interval` underflows on the host, use `Instant::now().checked_sub(policy.poll_interval).unwrap_or_else(Instant::now)`.

- [ ] **Step 5: Verify**

Run: `cargo test -p stroem-server --lib workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: all watcher tests pass (11); nothing else regresses.

Run the timing-sensitive tests 5 times to check for flakiness: `for i in 1 2 3 4 5; do cargo test -p stroem-server --lib workspace::watcher -q 2>&1 | tail -1; done`
Expected: `test result: ok` five times. If any flakes, widen the sleeps (never shrink the assertions).

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server
git commit -m "feat(workspace): watcher skips failed peeks, admits loads without waiting, watchdogs overdue loads"
```

---

### Task 13: Freshness gauges, `WorkspaceInfo` fields, metrics docs (spec § 4.8)

**Files:**
- Modify: `crates/stroem-server/src/metrics.rs` (3 gauge constants, `gather_gauges`, distinct-names list)
- Modify: `crates/stroem-server/src/workspace/mod.rs` (`WatchStatus`, `watch_statuses`, `WorkspaceInfo`)
- Modify: `ui/src/lib/types.ts:1-11`
- Modify: `docs/src/content/docs/operations/metrics.md`
- Test: `crates/stroem-server/tests/metrics_test.rs`

**Interfaces:**
- Produces:
  - `metrics::STROEM_WORKSPACE_LAST_SUCCESSFUL_LOAD_AGE_SECONDS`, `STROEM_WORKSPACE_LOAD_OVERDUE`, `STROEM_WORKSPACE_LOAD_PERMITS_AVAILABLE`
  - `workspace::WatchStatus { pub name: String, pub load_overdue: bool, pub last_successful_load_age: Option<Duration> }`; `WorkspaceManager::watch_statuses(&self, now: Instant) -> Vec<WatchStatus>`
  - `WorkspaceInfo.last_successful_load: Option<DateTime<Utc>>`, `WorkspaceInfo.availability: String` (`"fresh"` | `"errored"`)

- [ ] **Step 1: Failing metrics test** — append to `tests/metrics_test.rs` (reuse the helpers used by `background_task_last_tick_age_reflects_heartbeat`: `boot`, `empty_config`, `global_test_handle`, `scrape`):

```rust
#[tokio::test]
async fn workspace_watch_gauges_are_exported() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true, ..Default::default() });
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(
        h.pool.clone(),
        WorkspaceManager::from_config("gauged", WorkspaceConfig::new()),
        config,
        log_storage,
        HashMap::new(),
        None,
    )
    .with_event_bus(EventBus::noop());
    let router = build_router(state, CancellationToken::new()).layer(Extension(global_test_handle()));

    let body = scrape(&router).await?;
    let line = |name: &str| {
        body.lines()
            .find(|l| l.starts_with(name) && l.contains(r#"workspace="gauged""#))
            .map(str::to_string)
    };
    assert!(
        line(stroem_server::metrics::STROEM_WORKSPACE_LOAD_OVERDUE).is_some_and(|l| l.ends_with(" 0")),
        "overdue gauge missing or non-zero:\n{body}"
    );
    assert!(
        line(stroem_server::metrics::STROEM_WORKSPACE_LAST_SUCCESSFUL_LOAD_AGE_SECONDS).is_some(),
        "last-successful-load age missing:\n{body}"
    );
    assert!(
        body.lines().any(|l| l.starts_with(stroem_server::metrics::STROEM_WORKSPACE_LOAD_PERMITS_AVAILABLE)
            && l.ends_with(" 8")),
        "permits gauge missing or not 8:\n{body}"
    );
    Ok(())
}
```

Run: `cargo test -p stroem-server --test metrics_test workspace_watch_gauges_are_exported`
Expected: compile error (constants missing).

- [ ] **Step 2: Constants** — add to `metrics.rs` (and to `metric_name_constants_are_distinct`, together with the two Task 12 counters):

```rust
/// `gauge` — seconds since a workspace last loaded successfully. Label: workspace.
pub const STROEM_WORKSPACE_LAST_SUCCESSFUL_LOAD_AGE_SECONDS: &str =
    "stroem_workspace_last_successful_load_age_seconds";
/// `gauge` — 1 while a watcher load has exceeded its budget and still runs.
/// Derived at scrape time, never stored. Label: workspace.
pub const STROEM_WORKSPACE_LOAD_OVERDUE: &str = "stroem_workspace_load_overdue";
/// `gauge` — free watcher-load permits on THIS replica. 0 also occurs with
/// eight healthy loads in progress; alert only together with overdue > 0.
pub const STROEM_WORKSPACE_LOAD_PERMITS_AVAILABLE: &str = "stroem_workspace_load_permits_available";
```

- [ ] **Step 3: Manager accessors** (`mod.rs`):

```rust
/// Scrape-time freshness of one workspace (spec § 4.8).
#[derive(Debug, Clone)]
pub struct WatchStatus {
    pub name: String,
    pub load_overdue: bool,
    pub last_successful_load_age: Option<Duration>,
}

    // in impl WorkspaceManager:
    pub fn watch_statuses(&self, now: Instant) -> Vec<WatchStatus> {
        self.entries
            .iter()
            .map(|(name, entry)| WatchStatus {
                name: name.clone(),
                load_overdue: entry.availability().load_overdue(now),
                last_successful_load_age: entry
                    .published()
                    .loaded_at
                    .map(|t| now.saturating_duration_since(t)),
            })
            .collect()
    }
```

`gather_gauges` — in the synchronous section after the background-task loop:

```rust
    let now = std::time::Instant::now();
    for status in state.workspaces.watch_statuses(now) {
        gauge!(STROEM_WORKSPACE_LOAD_OVERDUE, "workspace" => status.name.clone())
            .set(f64::from(status.load_overdue));
        // Absent until the first successful load — never a fake 0.
        if let Some(age) = status.last_successful_load_age {
            gauge!(STROEM_WORKSPACE_LAST_SUCCESSFUL_LOAD_AGE_SECONDS, "workspace" => status.name)
                .set(age.as_secs_f64());
        }
    }
    gauge!(STROEM_WORKSPACE_LOAD_PERMITS_AVAILABLE)
        .set(state.workspaces.load_permits_available() as f64);
```

- [ ] **Step 4: `WorkspaceInfo`** — add fields:

```rust
    /// When this workspace last loaded successfully (UTC); absent if never.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_successful_load: Option<chrono::DateTime<chrono::Utc>>,
    /// `"fresh"` or `"errored"` — the watcher's view (spec § 4.8).
    pub availability: String,
```

In `list_workspace_info` set them from `p.loaded_at_utc` and `if entry.availability().is_errored() { "errored" } else { "fresh" }`; for source-construction failures use `None` / `"errored"`. Fix any other `WorkspaceInfo { .. }` literal the compiler reports (e.g. `test_workspace_info_serializes_triggers_enabled`).

`ui/src/lib/types.ts` — add to `WorkspaceInfo`:

```ts
  /** "fresh" or "errored" — the server watcher's view of this workspace */
  availability?: "fresh" | "errored";
  /** ISO timestamp of the last successful load, absent if never */
  last_successful_load?: string;
```

- [ ] **Step 5: Docs** — append to `docs/src/content/docs/operations/metrics.md` a "Workspace refresh" section listing the five metrics (two counters from Task 12, three gauges) with type, labels and meaning as in the constants' doc comments, plus:

```markdown
**Alerting.** Alert on `stroem_workspace_load_permits_available == 0` **and**
`stroem_workspace_load_overdue > 0` for the same `replica_id`, sustained for a
few minutes: every watcher slot on that replica is held by a load stuck in a
phase that cannot be interrupted, and that replica has stopped refreshing
workspaces. Neither signal fails `/livez` — a stale-but-serving server is
healthy; recovering is an operator decision (restart the pod).
`stroem_workspace_last_successful_load_age_seconds` growing past a few poll
intervals means refreshes are being skipped — usually the remote is
unreachable (see `stroem_workspace_peek_failures_total`).
```

- [ ] **Step 6: Verify**

Run: `cargo test -p stroem-server --test metrics_test && cargo test -p stroem-server --lib && cargo clippy --workspace --all-targets -- -D warnings && cd ui && bunx tsc --noEmit && bun run lint`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server ui/src/lib/types.ts docs/src/content/docs/operations/metrics.md
git commit -m "feat(metrics): workspace freshness, overdue and permit gauges"
```

---

### Task 14: Documentation (CLAUDE.md, user docs, TODO, spec status)

**Files:**
- Modify: `CLAUDE.md` (§ Multi-Workspace, § Health Check note)
- Modify: `docs/src/content/docs/getting-started/configuration.md`
- Modify: `docs/src/content/docs/guides/multi-workspace.md`
- Modify: `docs/internal/TODO.md`
- Modify: `docs/superpowers/specs/2026-09-17-workspace-scale-design.md` (status line)

- [ ] **Step 1: CLAUDE.md** — in § Multi-Workspace, replace the bullet starting `- Smart polling via \`peek_revision()\`` with:

```markdown
- **Workspace refresh (spec `docs/superpowers/specs/2026-09-17-workspace-scale-design.md` § 4).** `WorkspaceSource` is SYNCHRONOUS (`load(&LoadBudget) -> LoadOutcome`, `peek_revision(&LoadBudget) -> Peek`) and always runs on `spawn_blocking`; `block_in_place` is gone from `GitSource`. A `Peek::Failed` (network/auth/timeout) SKIPS the tick and keeps serving the last loaded config — a new policy, not last-good serving after a failed reload (still reverted); K=5 consecutive failures force one load. Per-entry state is split three ways: the execution mutex (`WorkspaceEntry::exec`, held by a load's finalizer until the worker REALLY returns; every acquirer uses `try_lock`), `Availability` (µs std mutex, written only by the pure `availability::transition`), and the published snapshot (`WorkspaceEntry::published()`, what every getter reads). `WorkspaceEntry::apply_load_result` is the ONLY writer of the snapshot and of load-completion state — every load path (watcher, `reload`, `reload_for_api`, startup) goes through it, so an external failure lands in `Errored` and the watcher retries it on a doubling backoff (cap `workspace_reload.max_backoff_secs`). Loads run worker (`spawn_blocking`) → finalizer (detached; owns guard + permit; applies result; releases; THEN spawns the best-effort peer notification) → observer (may time out; never cancels). Watcher loads take one of `MAX_CONCURRENT_WORKSPACE_LOADS` permits via non-blocking admission; `Busy`/`Saturated` is never a transition. External callers get `ReloadBusy` (downcast it) instead of queueing; a busy `force_refresh` fires from a healthy snapshot (errored ⇒ still MISSED). Deadlines: `LoadBudget` (stroem-common) reaches `sops`/`vals` (killed at the deadline via `budget::run_with_deadline`), the YAML scan (expiry aborts the whole load, never a per-file warning), git `transfer_progress` and checkout-planning `notify`; libgit2 connect/read timeouts are process-wide (`workspace::git::configure_global_timeouts`, set in `main`). DNS, a slow-drip remote, one blocked read and the checkout write phase stay uninterruptible — that is what the watchdog (`stroem_workspace_load_overdue`, derived at scrape time) is for. Tuning: `workspace_reload:` server config. NOT covered (spec § 4.10): peer-reload listener stall, notification origin, startup saturation, signal-to-exit bound.
- **Tarball retention keep-set** (`JobRepo::tarball_keep_revisions`): active jobs' revisions, cross-workspace `job_step.action_revision`s of active jobs, and failed top-level jobs still owed a task retry (1-hour window).
- **Runtime**: `main` builds tokio explicitly with `worker_threads = max(4, available_parallelism)` (`runtime::worker_threads`) — a sub-core CPU quota used to yield ONE worker.
```

- [ ] **Step 2: configuration.md** — add a `workspace_reload` section:

```markdown
### `workspace_reload`

Tunes how the server refreshes workspaces. All fields are optional.

| Field | Default | Meaning |
|---|---|---|
| `peek_failure_threshold` | `5` | Consecutive failed change checks before a forced reload |
| `peek_timeout_secs` | `30` | Budget for one change check (`ls-remote` / folder hash) |
| `load_timeout_secs` | `300` | Budget for one reload; an overdue reload keeps running and is reported by `stroem_workspace_load_overdue` |
| `max_backoff_secs` | `900` | Cap of the retry backoff for a workspace whose reload failed |
| `git_connect_timeout_ms` | `10000` | libgit2 TCP connect timeout (process-wide) |
| `git_read_timeout_ms` | `60000` | libgit2 per-read socket timeout (process-wide) |

Environment overrides use the usual form, e.g. `STROEM__WORKSPACE_RELOAD__LOAD_TIMEOUT_SECS=600`.
```

- [ ] **Step 3: multi-workspace.md** — add a "How workspaces are refreshed" section: each watcher checks its source every `poll_interval_secs` (git: `ls-remote`; folder: content hash); a changed revision triggers a reload; a check that **fails** (network, auth, timeout) is skipped and the last loaded config keeps serving, with a forced reload after `peek_failure_threshold` consecutive failures; a failed reload makes the workspace unavailable until a retry succeeds (backoff doubles to `max_backoff_secs`); watcher start times are spread across the poll interval; at most 8 reloads run at once per server. Link to the configuration and metrics pages.

- [ ] **Step 4: TODO.md** — mark `[x]` (append ` — done on \`feat/workspace-scale-peek-policy\``) for: "Envelope first" (chart defaults only — note that the prod helmfile bump + re-measurement is still open), "Peek failure must not trigger a full fetch", "Deadline/cancellation contract for workspace load", "Jitter watcher start", "Serialize watcher reloads with the API path", "Backoff for errored workspaces", "`worker_threads` floor", "Watcher freshness observability". Leave § 4.10 items, worker eviction, tarball, startup, shallow clones, fan-outs and claim-time `vals` open.

- [ ] **Step 5: Spec status** — replace the `Status:` line with `Status: revision 6 — § 4 APPROVED WITH NITS (Codex round 6) and IMPLEMENTED on \`feat/workspace-scale-peek-policy\` (plan \`docs/superpowers/plans/2026-09-18-workspace-scale-peek-policy.md\`); §§ 3, 6, 7 are problem statements and are **not** approved`.

- [ ] **Step 6: Verify and commit**

Run: `cd docs && bun run build 2>&1 | tail -2 && bun run generate-llms 2>&1 | tail -1`
Expected: build OK.

```bash
git add CLAUDE.md docs
git commit -m "docs: workspace refresh policy, workspace_reload config, retention keep-set"
```

---

### Task 15: Full verification and Codex implementation review

- [ ] **Step 1: Full CI suite** (user's pre-push rule)

```bash
cargo fmt --check --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked" | tail -40
cd ui && bun run lint && bunx tsc --noEmit && cd ..
```

Expected: fmt/clippy clean; every `test result: ok` except failures that ALSO occur on `origin/main` (known flaky: `log_storage::tests`; 4 pre-existing integration failures recorded in memory) — verify any failure against `origin/main` before dismissing it. Disk is tight (~10 GiB free): if the build dies on "No space", stop and report.

- [ ] **Step 2: E2E harness**

Run: `cargo test -p stroem-e2e 2>&1 | tail -3`
Expected: pass.

- [ ] **Step 3: Codex implementation review** — invoke the `codex:rescue` skill with `--resume` (thread `01a0afcd`) asking for an implementation review of `feat/workspace-scale-peek-policy` against spec § 4 as scoped by § 4.10, focusing on: single-writer discipline (`apply_load_result`/`transition` as the only writers), guard/permit lifetime in `spawn_load`, the peek observer/finalizer split, `try_lock` at every acquirer, publication boundary, and the two retention SQL changes. Fix CONFIRMED findings; re-review until APPROVE / APPROVE WITH NITS.

- [ ] **Step 4: Finish** — use `superpowers:finishing-a-development-branch`. Do NOT push or merge without the user's go-ahead. Remind the user that the prod envelope (their helmfile: `limits.cpu: 2`, `limits.memory: 2Gi`, startup probe) and the § 10 step 1 re-measurement are theirs to apply.

