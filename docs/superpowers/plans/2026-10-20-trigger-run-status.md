# Trigger Run Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show each trigger's last run, its result and missed scheduled runs on the workspace page, the task page and a new Dashboard card.

**Architecture:** One batched DB lookup (`TriggerRunRepo::last_runs`, two new partial indexes, migration 053) returns, per `(source_type, source_id)`, the newest run, the newest `skipped` fire and the newest task-retry job. A server module `trigger_status` turns that into `LastRun` / `MissedRuns` on `TriggerInfo` (pure missed computation from cron + timezone), applying the trigger ACL and the job-row ACL. Two existing endpoints gain the fields; a new `GET /api/triggers/attention` serves the Dashboard. The UI renders shared components in three places.

**Tech Stack:** Rust (axum, sqlx 0.9 runtime queries, croner via `stroem_common::cron`, chrono / chrono-tz), PostgreSQL, React 19 + TypeScript + shadcn/ui, vitest.

**Spec:** `docs/superpowers/specs/2026-10-20-trigger-run-status-design.md` (revision 5, Codex APPROVE). Read it before each task; section numbers below refer to it.

## Global Constraints

- Grace: `5 minutes`. Cap: `1000` (show `1000+`). A constant each, no config (§5.3).
- `source_type` per trigger type: scheduler → `trigger`, webhook → `webhook`, event source → `event_source`. `source_id` = `"{defining workspace}/{trigger map key}"` (§5.1).
- A run = scheduler/webhook job with `status <> 'skipped'`, or event-source job with `input ? '_fingerprint'` (consumer marker). Never filter on the trigger's current `task` / `target_task` (§5.1).
- Last fire `L` = newest row of any status (incl. `skipped`). Last run shown = newest run as defined above. Result = status of the newest job with `retry_of_job_id = run.job_id` (highest `retry_attempt`) if any, else the run's status (§5.2).
- Manual runs, Re-run, Restart never count (§5.1, user decision).
- Missed mark only for enabled scheduler triggers in a workspace with `triggers_enabled`; never without a last fire (§5.3).
- Needs attention = fires here AND (missed OR result `failed`) (§5.4).
- Responses carry identifiers, timestamps and statuses only — never job input/output (§7.4).
- ACL: trigger visible unless its task is `Deny` (today's `list_triggers` rule); last-run fields only when the job row's ACL path is not `Deny`, else `last_run: null, last_run_hidden: true`; an ACL-context load error is an error response, never fall-open (§7.3).
- Migration 053: `CREATE INDEX IF NOT EXISTS` under new names, header with the manual `CREATE INDEX CONCURRENTLY` pre-run, like `050_git_refs_indexes.sql` (§7.2, CLAUDE.md § Database).
- Every cargo command is prefixed `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`; DB tests also need `DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true`. Never set `CARGO_TARGET_DIR`. Never run any docker prune / rm / volume or image removal — stop and report instead. Never start a workspace-wide or background test run. Before building stroem-server in a fresh worktree: `mkdir -p crates/stroem-server/static` (never commit it).
- Conventional commits; no AI co-author or "Generated with" trailer.

## Review Focus

1. **DB clock vs replica clock.** `created_at` is the DB's `NOW()`; a replica whose clock runs ahead/behind by minutes would show or hide marks wrongly. `last_runs` returns the DB's `NOW()` and the missed computation uses it (test in Task 1, used in Task 4).
2. **DST gap double count.** An hourly `0 * * * *` Europe/Copenhagen trigger across 2026-03-29 must count each UTC hour once; occurrences are deduplicated to a strictly increasing UTC sequence (test in Task 2).
3. **Same trigger name in two workspaces.** `etl/nightly` and `ops/nightly` must never share a last run (test in Task 4).
4. **A workspace in load error.** The Dashboard endpoint skips it and still answers 200 for the rest (test in Task 5).
5. **Dashboard resilience.** The new card is absent while loading and when its call fails; the counters and Recent Jobs still render (test in Task 7).

---

### Task 1: Migration 053 and `TriggerRunRepo`

**Files:**
- Create: `crates/stroem-db/migrations/053_trigger_run_indexes.sql`
- Create: `crates/stroem-db/src/repos/trigger_run.rs`
- Modify: `crates/stroem-db/src/repos/mod.rs`, `crates/stroem-db/src/lib.rs` (export `TriggerRunRepo, TriggerRunKey, TriggerRunRow`)
- Create: `crates/stroem-db/tests/trigger_runs_test.rs`; Modify: `crates/stroem-db/tests/main.rs` (`mod trigger_runs_test;`)
- Modify: `crates/stroem-db/tests/migration_test.rs` (new `test_053_trigger_run_indexes`)

**Interfaces:**
- Produces:
  ```rust
  pub struct TriggerRunKey { pub source_type: String, pub source_id: String }
  #[derive(Debug, Clone, sqlx::FromRow)]
  pub struct TriggerRunRow {
      pub source_type: String, pub source_id: String,
      pub run_job_id: Option<Uuid>, pub run_workspace: Option<String>, pub run_task_name: Option<String>,
      pub run_task_folder: Option<String>, pub run_git_ref: Option<String>,
      pub run_status: Option<String>, pub run_created_at: Option<DateTime<Utc>>,
      pub skipped_at: Option<DateTime<Utc>>,          // newest `skipped` fire (scheduler only)
      pub retry_job_id: Option<Uuid>, pub retry_status: Option<String>,
  }
  pub struct TriggerRunRepo;
  impl TriggerRunRepo {
      pub const LAST_RUNS_SQL: &'static str;   // the exact SQL, so the EXPLAIN test runs the same text
      /// One row per key, in input order. Returns the DB's NOW() with the rows.
      pub async fn last_runs(pool: &PgPool, keys: &[TriggerRunKey]) -> Result<(DateTime<Utc>, Vec<TriggerRunRow>)>;
  }
  ```

- [ ] **Step 1: Write the migration test** — `test_053_trigger_run_indexes` asserts both indexes exist and are partial with the predicates below:
  ```rust
  let defs: Vec<(String, String)> = sqlx::query_as(
      "SELECT indexname::text, indexdef FROM pg_indexes WHERE tablename = 'job' \
       AND indexname IN ('idx_job_trigger_runs', 'idx_job_trigger_skipped') ORDER BY indexname").fetch_all(&pool).await?;
  assert_eq!(defs.len(), 2);
  assert!(defs[0].1.contains("'_fingerprint'") && defs[0].1.contains("<> 'skipped'"), "{}", defs[0].1); // idx_job_trigger_runs
  assert!(defs[1].1.contains("status = 'skipped'"), "{}", defs[1].1);                                  // idx_job_trigger_skipped
  ```

- [ ] **Step 2: Write the repo tests** in `trigger_runs_test.rs`, with a local helper `insert_job(pool, source_type, source_id, task, status, created_at, input: Option<Value>) -> Uuid` (plain `INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id, input, created_at) VALUES (…, 'distributed', …)`):
  - `last_runs_returns_one_row_per_key_in_order_with_nulls_for_never` — keys `[trigger/etl/a, webhook/etl/b, trigger/etl/none]`; third row has every `run_*` and `skipped_at` `None`.
  - `last_run_skips_skipped_rows_but_last_fire_sees_them` — `completed` run at T−3h, then 3 `skipped` rows at T−2h..T; `run_created_at == T−3h`, `run_status == "completed"`, `skipped_at == T`.
  - `event_source_last_run_is_the_newest_consumer_not_an_emitted_job` — consumer (`input = {"_fingerprint":"f","_event_source":{}}`, task `consume`) at T−1h, emitted jobs (task `handle`, input `{"x":1}`) at T−30m and T; `run_task_name == "consume"`.
  - `event_source_legacy_consumer_row_counts` — v0.14.1 shape: task `_event_source:feed`, input `{"_fingerprint":"f"}` → found.
  - `event_source_consumer_counts_whatever_its_task_name` — consumer rows for tasks `a` (older) and `c` (newer) → `c`.
  - `retry_chain_result_is_the_highest_attempt` — failed run R, retries with `retry_of_job_id = R`, `retry_attempt` 1 (failed) and 2 (completed) → `retry_status == "completed"`, `retry_job_id` = attempt 2.
  - `last_runs_reports_db_now` — returned instant within 5 s of `SELECT NOW()`.
  - `last_run_probes_use_the_partial_indexes` — seed 3 000 `skipped` rows and 3 000 emitted rows newer than the run, `ANALYZE job`, then in a tx with `SET LOCAL enable_seqscan = off` run `EXPLAIN` + `TriggerRunRepo::LAST_RUNS_SQL` with binds for one trigger key and one event-source key; assert the plan text contains `idx_job_trigger_runs`, `idx_job_trigger_skipped` and `idx_job_retry_of`, and no `Seq Scan on job` (pattern: `templated_refs_test::claim_and_unmatched_sweep_still_use_a_ready_partial_index`).

- [ ] **Step 3: Run the tests to verify they fail**
  Run: `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true cargo test -p stroem-db --test integration -- trigger_runs_test:: migration_test::test_053`
  Expected: compile error (`TriggerRunRepo` not found).

- [ ] **Step 4: Write migration 053.** Header comment in the style of `050_git_refs_indexes.sql`: why (spec 2026-10-20 § 7.2), the manual pre-run (`CREATE INDEX CONCURRENTLY IF NOT EXISTS …` for both, one at a time, autocommit), that the migration is a no-op afterward, and that without it each index scans all of `job` under a SHARE lock. Body:
  ```sql
  CREATE INDEX IF NOT EXISTS idx_job_trigger_runs
      ON job (source_type, source_id, created_at DESC)
      WHERE (source_type IN ('trigger', 'webhook') AND status <> 'skipped')
         OR (source_type = 'event_source' AND input ? '_fingerprint');
  CREATE INDEX IF NOT EXISTS idx_job_trigger_skipped
      ON job (source_id, created_at DESC)
      WHERE source_type = 'trigger' AND status = 'skipped';
  ```

- [ ] **Step 5: Implement `TriggerRunRepo::last_runs`.** `LAST_RUNS_SQL` selects `NOW() AS db_now` as its first column (add `pub db_now: DateTime<Utc>` to `TriggerRunRow`); `last_runs` returns the first row's `db_now` with the rows, and for an empty `keys` runs only `SELECT NOW()` and returns `(now, vec![])`. The lookup shape (the `WHERE` clauses must repeat each partial-index predicate literally so the planner proves it):
  ```sql
  SELECT NOW() AS db_now, k.source_type, k.source_id, r.job_id AS run_job_id, r.workspace AS run_workspace, …,
         s.created_at AS skipped_at, t.job_id AS retry_job_id, t.status AS retry_status
  FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS k(source_type, source_id, ord)
  LEFT JOIN LATERAL (SELECT … FROM job j WHERE j.source_type = k.source_type AND j.source_id = k.source_id
        AND ((j.source_type IN ('trigger','webhook') AND j.status <> 'skipped')
             OR (j.source_type = 'event_source' AND j.input ? '_fingerprint'))
        ORDER BY j.created_at DESC LIMIT 1) r ON true
  LEFT JOIN LATERAL (SELECT j.created_at FROM job j WHERE k.source_type = 'trigger'
        AND j.source_type = 'trigger' AND j.source_id = k.source_id AND j.status = 'skipped'
        ORDER BY j.created_at DESC LIMIT 1) s ON true
  LEFT JOIN LATERAL (SELECT j.job_id, j.status FROM job j WHERE j.retry_of_job_id = r.job_id
        ORDER BY j.retry_attempt DESC LIMIT 1) t ON true
  ORDER BY k.ord
  ```
  `LAST_RUNS_SQL` is a `&'static str` (sqlx 0.9 `SqlSafeStr`), no `AssertSqlSafe`.

- [ ] **Step 6: Run the tests to verify they pass** (same command as Step 3). Expected: all PASS.

- [ ] **Step 7: Commit**
  ```bash
  git add crates/stroem-db
  git commit -m "feat(db): trigger last-run lookup with partial indexes (migration 053)"
  ```

---

### Task 2: Pure missed-run computation

**Files:**
- Create: `crates/stroem-server/src/trigger_status.rs` (module root; `#[cfg(test)] mod tests;` → `crates/stroem-server/src/trigger_status/tests.rs`)
- Modify: `crates/stroem-server/src/lib.rs` (`pub mod trigger_status;`)

**Interfaces:**
- Produces:
  ```rust
  pub const MISSED_GRACE_SECS: i64 = 300;
  pub const MISSED_CAP: u32 = 1000;
  #[derive(Debug, Clone, PartialEq, Serialize)]
  pub struct MissedRuns { pub count: u32, pub capped: bool, pub first: DateTime<Utc> }
  pub fn source_type_of(def: &TriggerDef) -> &'static str;   // "trigger" | "webhook" | "event_source"
  /// `None` for: unparsable cron, no occurrence in (last_fire, now − grace].
  pub fn missed_runs(cron: &str, timezone: Option<&str>, last_fire: DateTime<Utc>, now: DateTime<Utc>) -> Option<MissedRuns>;
  ```

- [ ] **Step 1: Write the failing tests** in `trigger_status/tests.rs` (UTC unless noted; `t(“2026-10-20T01:00:00Z”)` helper):
  - `on_time_daily_run_is_not_missed` — `0 1 * * *`, last fire 10-20 01:00:40Z, now 10-20 09:00Z → `None`.
  - `late_run_within_grace_is_not_missed` — `0 1 * * *`, last fire 10-19 01:00:40Z, now 10-20 01:04:59Z → `None`.
  - `one_missed_daily_run` — same, now 10-20 01:05:00Z → `count 1, first 10-20 01:00Z, capped false`.
  - `outage_counts_every_missed_fire` — `*/5 * * * *`, last fire 10-19 09:55:00Z, now 10-20 02:17:00Z → `count == 195`, `first == 10-19 10:00Z` (09:55 exclusive … 02:10 inclusive at grace 5 min: 10:00→02:10 = 16 h 10 m / 5 m + 1 = 195).
  - `count_stops_at_the_cap` — `* * * * * *`, last fire T, now T + 2 h → `count 1000, capped true, first T+1s`.
  - `six_field_cron_is_accepted` — `*/10 * * * * *`, last fire T, now T + 6 min → `count == 6` ((T, T+60s] … grace leaves T+10s..T+60s).
  - `timezone_is_applied` — `0 3 * * *` `Europe/Copenhagen`, last fire 10-18 01:00Z (03:00 CEST), now 10-20 09:00Z → `count 2, first 10-19 01:00Z`.
  - `dst_gap_counts_each_utc_hour_once` — `0 * * * *` `Europe/Copenhagen`, last fire 2026-03-28T23:00:00Z, now 2026-03-29T05:05:00Z → `count == 6`.
  - `bad_timezone_falls_back_to_utc` — `0 1 * * *`, tz `Mars/Base`, last fire 10-19 01:00Z, now 10-20 02:00Z → `count 1, first 10-20 01:00Z`.
  - `unparsable_cron_gives_no_mark` — `"not a cron"` → `None`.
  - `source_type_mapping` — one `TriggerDef` of each variant.

- [ ] **Step 2: Run to verify they fail**
  Run: `mkdir -p crates/stroem-server/static && CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib trigger_status::`
  Expected: compile error (module missing).

- [ ] **Step 3: Implement.** Parse with `stroem_common::cron::parse`; timezone via `s.parse::<chrono_tz::Tz>().ok()` (bad → UTC, like `scheduler::load_triggers`); iterate `cron.iter_after(last_fire.with_timezone(&tz))` (as `web/api/triggers.rs::compute_next_runs`) converted to UTC, keep only values strictly greater than the previous kept value, stop at the first value `> now − MISSED_GRACE_SECS` or after `MISSED_CAP` kept values (then `capped = true` only if one more in-range value exists).

- [ ] **Step 4: Run to verify they pass** (same command). Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(server): pure missed-run computation for scheduler triggers"`

---

### Task 3: One job-row ACL path helper

**Files:**
- Modify: `crates/stroem-server/src/acl.rs` (around `job_folder` / `job_task_path`, lines ~186–211)
- Test: in-module tests of `acl.rs` (follow the file's existing test placement)

**Interfaces:**
- Produces:
  ```rust
  /// THE job-row ACL path, from the row's coordinates (spec 2026-10-02 § 7.8).
  pub async fn job_coords_task_path(state: &AppState, workspace: &str, task_name: &str,
                                    git_ref: Option<&str>, task_folder: Option<&str>) -> String;
  ```
  `job_folder` / `job_task_path` keep their signatures and delegate to it.

- [ ] **Step 1: Write the failing tests** — `job_coords_task_path_uses_pinned_folder` (git_ref `Some`, task_folder `Some("nightlies")` → `"nightlies/nightly"` even when the live task has another folder) and `job_coords_task_path_uses_live_folder_when_unpinned` (live folder `public` → `"public/manifest"`; task missing live → bare `"manifest"`). Build `AppState` the way existing `acl.rs` / `web/api` unit tests do (`crate::test_support`).

- [ ] **Step 2: Run to verify they fail** — `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-server --lib acl::` → compile error.

- [ ] **Step 3: Implement** `job_coords_task_path` (live folder read only when `git_ref` is `None`, then `acl_folder` + `make_task_path`); make `job_folder` call it with the row's fields.

- [ ] **Step 4: Run** the same command plus `cargo test -p stroem-server --lib web::api::jobs::` → PASS.

- [ ] **Step 5: Commit** — `git commit -m "refactor(acl): job-row ACL path from coordinates"`

---

### Task 4: Last run and missed on the trigger list and task detail

**Files:**
- Modify: `crates/stroem-server/src/trigger_status.rs` (async half)
- Modify: `crates/stroem-server/src/web/api/triggers.rs` (`TriggerInfo`, `list_triggers`)
- Modify: `crates/stroem-server/src/web/api/tasks.rs:277-282` (`get_task` triggers)
- Create: `crates/stroem-server/tests/trigger_status_test.rs`; Modify: `crates/stroem-server/tests/main.rs` (`mod trigger_status_test;`)

**Interfaces:**
- Consumes: `TriggerRunRepo::last_runs` (Task 1), `missed_runs`, `source_type_of`, `MissedRuns` (Task 2), `acl::job_coords_task_path` (Task 3).
- Produces:
  ```rust
  // trigger_status.rs
  #[derive(Debug, Clone, PartialEq, Serialize)]
  pub struct LastRun { pub job_id: Uuid, pub created_at: DateTime<Utc>,
                       pub status: String,                       // the RESULT (§5.2)
                       #[serde(skip_serializing_if = "Option::is_none")] pub retry_job_id: Option<Uuid> }
  pub struct Viewer { pub email: String, pub groups: HashSet<String>, pub is_admin: bool }
  /// `Ok(None)` = no filtering (no auth user or ACL not configured). Errors are never swallowed.
  pub async fn viewer_for(state: &AppState, auth: &Option<AuthUser>) -> anyhow::Result<Option<Viewer>>;
  /// Today's `list_triggers` rule: hidden only when the trigger's task (live folder in the defining workspace) is Deny.
  pub fn trigger_visible(state: &AppState, viewer: Option<&Viewer>, workspace: &str, config: &WorkspaceConfig, task: &str) -> bool;
  pub struct TriggerStatusItem { pub workspace: String, pub fires_here: bool, pub def: TriggerDef, pub info: TriggerInfo }
  /// One `last_runs` call for all items (any number of workspaces); fills `info.last_run`, `last_run_hidden`, `missed`.
  pub async fn attach_run_status(state: &AppState, viewer: Option<&Viewer>, items: &mut [TriggerStatusItem]) -> anyhow::Result<()>;

  // web/api/triggers.rs — TriggerInfo gains (always serialized):
  pub last_run: Option<LastRun>, pub last_run_hidden: bool, pub missed: Option<MissedRuns>,
  ```
  `fires_here` = `def.enabled() && state.workspaces.triggers_enabled(workspace)`. `missed` is computed only when `fires_here` and the def is a scheduler; `L = max(run_created_at, skipped_at)`; `now` = the DB instant from `last_runs`. Last-run ACL: one check on the run row's coordinates (a retry job copies the run's workspace, task and ref — `settlement/retry.rs::create_retry_job` — so it shares the path); `Deny` → `last_run = None, last_run_hidden = true`. `TriggerInfo::from_def` stays synchronous and sets the new fields to `None` / `false`.

- [ ] **Step 1: Write the failing integration tests** in `trigger_status_test.rs`. Own setup modelled on `duration_stats_test.rs::setup` but with two in-memory workspaces (`WorkspaceManager::from_entries` + `workspace::in_memory_entry`) `etl` and `ops`, parsed from YAML; optional `acl` + `auth`; API keys for users as in `common/pinned.rs::PinnedFixture::api_key`. YAML for `etl`: tasks `nightly` (folder `public`), `secret-task` (folder `private`), `consume`, `handle`; triggers `nightly` (scheduler `0 1 * * *`, task `nightly`), `every5` (scheduler `*/5 * * * *`, `concurrency: skip`), `hook` (webhook, name `etl-hook`, task `nightly`), `feed` (event_source, task `consume`, target_task `handle`), `off` (scheduler, `enabled: false`), `cross` (scheduler, task `ops.report`); `ops`: task `report`, trigger `nightly` (scheduler `0 1 * * *`). Seed jobs with SQL (`created_at` relative to `NOW()`). Tests:
  - `trigger_list_shows_last_run_result_and_missed_mark` — `nightly` last run `completed` at `NOW() - interval '2 days'` → `last_run.status == "completed"`, `last_run.job_id` = that job, `missed.count >= 1`, `missed.capped == false`, and `missed.first` is a `…T01:00:00Z` instant later than `last_run.created_at`.
  - `trigger_never_run_has_no_mark` — `last_run == null`, `missed == null`.
  - `run_after_the_gap_resets_the_mark` — add a run 1 min ago → `missed == null`.
  - `manual_rerun_and_restart_do_not_count` — jobs with `source_type` `user`, `rerun`, `restart` for task `nightly` after the trigger run → unchanged `last_run.job_id` and mark.
  - `task_retry_gives_the_result` — failed run + retry (`retry_of_job_id`, attempt 1, `completed`) → `status == "completed"`, `retry_job_id` set, `job_id` = the trigger's run.
  - `skipped_rows_count_as_fires_not_as_the_run` — `every5`: `running` run 1 h ago + `skipped` rows every 5 min up to 1 min ago → `last_run.status == "running"`, `missed == null`.
  - `webhook_and_event_source_never_get_a_mark` — old runs for `hook` and `feed` (+ newer emitted `handle` job) → `missed == null`; `feed.last_run.job_id` = the consumer.
  - `disabled_trigger_and_triggers_false_workspace_get_no_mark` — `off` with an old run; and `etl` with `with_triggers_disabled("etl")` → no marks, last runs still shown.
  - `cross_workspace_target_matched_by_source_id` — job in workspace `ops`, task `report`, `source_id = "etl/cross"` → shown on `etl`'s `cross`.
  - `same_trigger_name_in_two_workspaces_stays_apart` — `etl/nightly` and `ops/nightly` with different statuses.
  - `task_detail_triggers_carry_the_same_fields` — `GET /api/workspaces/etl/tasks/nightly` → `triggers[]` entries equal the trigger-list entries for `nightly` and `hook`.
  - `acl_hides_denied_trigger_and_withholds_a_denied_jobs_run` — ACL: viewer may View `etl` `public/*` only; a pinned run of `nightly` (`git_ref` set, `task_folder = 'private'`) → trigger listed, `last_run == null`, `last_run_hidden == true`, missed still present; trigger on `secret-task` not listed; admin sees the run.
  - `responses_never_carry_job_input_or_output` — seed run with `input`/`output` containing `"s3cr3t"`; the response body text does not contain it.

- [ ] **Step 2: Run to verify they fail**
  Run: `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true cargo test -p stroem-server --test integration trigger_status_test::`
  Expected: FAIL (fields absent / compile errors).

- [ ] **Step 3: Implement** the async half of `trigger_status.rs`, the `TriggerInfo` fields, and the handlers: `list_triggers` builds `viewer_for` (map error to `AppError::Internal` — this replaces today's silent fall-open `if let Ok(...)`), filters with `trigger_visible`, then `attach_run_status`; `get_task` (task ACL already checked there) builds the items for its triggers and calls `attach_run_status` with the same viewer.

- [ ] **Step 4: Run to verify they pass** (same command) and run `cargo test -p stroem-server --lib web::api::triggers::` → PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(server): last run, result and missed runs on trigger lists"`

---

### Task 5: `GET /api/triggers/attention`

**Files:**
- Modify: `crates/stroem-server/src/web/api/triggers.rs` (new handler `list_attention_triggers`)
- Modify: `crates/stroem-server/src/web/api/mod.rs` (route `/triggers/attention` in the `protected` router)
- Modify: `crates/stroem-server/src/trigger_status.rs` (`needs_attention`, `sort_attention`)
- Test: `crates/stroem-server/tests/trigger_status_test.rs`; unit tests in `trigger_status/tests.rs`

**Interfaces:**
- Consumes: Task 4's `viewer_for`, `trigger_visible`, `TriggerStatusItem`, `attach_run_status`.
- Produces:
  ```rust
  #[derive(Debug, Serialize)]
  pub struct AttentionTrigger { pub workspace: String, #[serde(flatten)] pub trigger: TriggerInfo, pub last_run_failed: bool }
  pub fn needs_attention(item: &TriggerStatusItem) -> bool;       // fires_here && (missed || last_run.status == "failed")
  pub fn sort_attention(items: &mut [AttentionTrigger]);          // missed first by count desc; then failed by last_run.created_at desc; ties workspace, name
  ```
  The handler iterates `state.workspaces.get_all_configs()` (a workspace in load error has no config and is skipped), keeps visible triggers, calls `attach_run_status` once for all, filters `needs_attention`, sorts. A hidden last run never yields `last_run_failed`.

- [ ] **Step 1: Write the failing tests** — unit: `needs_attention_rules` (missed → yes; failed → yes; cancelled/skipped → no; `fires_here = false` with failed → no), `attention_order` (missed 5, missed 2, failed newer, failed older). Integration: `attention_lists_missed_and_failed_only` (one missed, one failed, one completed, one cancelled → two rows, correct order, `last_run_failed` flags); `attention_skips_a_workspace_in_load_error` (`WorkspaceManager::mark_unavailable_for_test("ops")` → 200, only `etl` rows); `attention_respects_acl` (viewer does not see the `secret-task` trigger nor a failed run it may not view).

- [ ] **Step 2: Run to verify they fail** — unit `cargo test -p stroem-server --lib trigger_status::` and integration `… --test integration trigger_status_test::attention` (env as in Task 4) → FAIL.

- [ ] **Step 3: Implement** the two functions, the handler and the route.

- [ ] **Step 4: Run to verify they pass** (same commands) → PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(server): triggers that need attention endpoint"`

---

### Task 6: UI — last run and missed on the workspace page and the task page

**Files:**
- Modify: `ui/src/lib/types.ts` (`TriggerInfo` + new types), `ui/src/lib/api.ts` (`listAttentionTriggers`, used in Task 7)
- Create: `ui/src/lib/trigger-status.ts`, `ui/src/lib/__tests__/trigger-status.test.ts`
- Create: `ui/src/components/trigger-run-status.tsx`, `ui/src/components/__tests__/trigger-run-status.test.tsx`
- Modify: `ui/src/pages/workspace-detail.tsx` (Triggers card ~229-290), `ui/src/pages/task-detail.tsx` (trigger card ~335-395)
- Modify: `ui/src/pages/__tests__/workspace-detail.test.tsx`

**Interfaces:**
- Produces:
  ```ts
  export interface TriggerLastRun { job_id: string; created_at: string; status: string; retry_job_id?: string }
  export interface MissedRuns { count: number; capped: boolean; first: string }
  // TriggerInfo gains: last_run?: TriggerLastRun | null; last_run_hidden?: boolean; missed?: MissedRuns | null
  export interface AttentionTrigger extends TriggerInfo { workspace: string; last_run_failed: boolean }
  export function listAttentionTriggers(): Promise<AttentionTrigger[]>          // GET /api/triggers/attention
  // lib/trigger-status.ts
  export function missedLabel(m: MissedRuns): string        // `${m.capped ? "1000+" : m.count} missed · ${formatTime(m.first)}`
  export function triggersWithMissed<T extends TriggerInfo>(ts: T[]): T[]
  // components/trigger-run-status.tsx
  export function LastRunCell({ trigger }: { trigger: TriggerInfo }): JSX.Element  // StatusBadge + <Link to=`/jobs/${id}`>formatTime</Link>; "never"; "no access"; running event source: "running since <time>"
  export function MissedRunsPill({ missed }: { missed: MissedRuns }): JSX.Element      // amber Badge: bg-amber-100 text-amber-800 dark:bg-amber-900/30 dark:text-amber-400
  export function MissedRunsWarning({ triggers }: { triggers: TriggerInfo[] }): JSX.Element | null
  ```
  Warning copy (exact): `{N} trigger(s) missed scheduled runs — {names joined ", "}. Run the tasks manually if you need the data. The mark stays until each trigger's next scheduled run.` with "trigger"/"triggers" by N, an `AlertTriangle` icon, amber border/background; `null` when no trigger has `missed`.

- [ ] **Step 1: Write the failing tests:**
  - `trigger-status.test.ts`: `missedLabel({count:3,capped:false,first})` starts with `"3 missed · "`; capped → starts with `"1000+ missed · "`; `triggersWithMissed` keeps only entries with `missed`.
  - `trigger-run-status.test.tsx`: last run renders the status text and a link `href="/jobs/<id>"`; `last_run: null` → `never`; `last_run_hidden: true` → `no access`; event source `running` → `running since`; `MissedRunsWarning` lists two names with `2 triggers missed`, renders nothing for none.
  - `workspace-detail.test.tsx`: table has a `Last run` header between `Schedule` and `Next run`; a trigger with `missed` shows the pill and the warning box above the table; the existing `trigger` fixture (no new fields) still renders `never`.

- [ ] **Step 2: Run to verify they fail** — `cd ui && bun install && bun run test -- src/lib/__tests__/trigger-status.test.ts src/components/__tests__/trigger-run-status.test.tsx src/pages/__tests__/workspace-detail.test.tsx` → FAIL.

- [ ] **Step 3: Implement** types, api function, lib, components; add the column (`LastRunCell` + `MissedRunsPill` on a second line) and the warning above the `Table`; on the task page show `LastRunCell` and `MissedRunsPill` in each trigger entry (no warning box).

- [ ] **Step 4: Run to verify they pass** (same command), then `bun run lint && bun run build` → PASS, no type errors.

- [ ] **Step 5: Commit** — `git commit -m "feat(ui): last run and missed runs on workspace and task pages"`

---

### Task 7: UI — Dashboard "Triggers that need attention"

**Files:**
- Create: `ui/src/components/triggers-attention-card.tsx`, `ui/src/components/__tests__/triggers-attention-card.test.tsx`
- Modify: `ui/src/pages/dashboard.tsx` (between the counters grid and the Recent Jobs card)

**Interfaces:**
- Consumes: `listAttentionTriggers`, `AttentionTrigger`, `LastRunCell`, `MissedRunsPill` (Task 6).
- Produces: `export function TriggersAttentionCard(): JSX.Element | null` — own fetch on mount and every 30 s (its own interval, not the Dashboard's 5 s), renders nothing while loading, on error, or for an empty list; table columns `Workspace` (link `/workspaces/<ws>`), `Trigger` (name, link to the workspace page), `Problem` (`MissedRunsPill` and/or red `last run failed` badge using the `failed` style of `status-badge.tsx`), `Last run` (`LastRunCell`); rows in server order.

- [ ] **Step 1: Write the failing tests** (mock `@/lib/api`): renders title `Triggers that need attention` and one row per item with both problem badges when an item is missed and failed; renders nothing for `[]`; renders nothing when the call rejects; dashboard page test (new, `src/pages/__tests__/dashboard.test.tsx`, mocks `getStats`, `listJobs`, `listAttentionTriggers` rejecting) still shows `Recent Jobs`.

- [ ] **Step 2: Run to verify they fail** — `cd ui && bun run test -- src/components/__tests__/triggers-attention-card.test.tsx src/pages/__tests__/dashboard.test.tsx` → FAIL.

- [ ] **Step 3: Implement** the card and place it in `dashboard.tsx`.

- [ ] **Step 4: Run to verify they pass**, then `bun run lint && bun run build` → PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(ui): dashboard card for triggers that need attention"`

---

### Task 8: Documentation

**Files:**
- Modify: `docs/src/content/docs/guides/triggers.md` (new section "Last run and missed runs" after "How the scheduler works"; webhook/event-source notes)
- Create: `docs/src/content/docs/operations/migration-053.md`; Modify: `docs/astro.config.mjs` (sidebar entry next to `operations/migration-051-052`)
- Modify: `CLAUDE.md` (§ Scheduler (Cron Triggers): new bullet for trigger run status — module `trigger_status`, `TriggerRunRepo::last_runs`, consumer marker, grace/cap, endpoints; § Database: migration 053 pre-run line; § React UI: the three UI parts)
- Modify: `docs/internal/TODO.md` (new section "Trigger run status (2026-10-20)" with the §8 limitations as open follow-ups: re-enabled trigger, changed cron, Dashboard skips load-error workspaces, manual run does not reset)
- Modify: `CONTEXT.md` (glossary entries "Last fire" and "Missed run", worded from spec §5.2–5.3; CLAUDE.md § Scheduler points at them)
- Modify: `docs/superpowers/specs/2026-10-20-trigger-run-status-design.md` status line → implemented

**Interfaces:** none.

- [ ] **Step 1: Write the guide section** — what "Last run" shows (result, task retry, `skipped` rows, event-source consumer), the missed rule (grace 5 min, cap 1000+, reset only by the trigger's own next run, manual/Re-run/Restart do not clear it), where it shows (workspace page, task page, Dashboard), and the §8 limitations.
- [ ] **Step 2: Write `migration-053.md`** — what the two indexes are for, the exact `CREATE INDEX CONCURRENTLY IF NOT EXISTS` statements from the migration header, "run them one at a time, not in a transaction", and that the migration is a no-op afterward.
- [ ] **Step 3: Update CLAUDE.md, CONTEXT.md and TODO.md** as listed above.
- [ ] **Step 4: Verify the docs build** — `cd docs && bun install && bun run build` → exits 0 (this also regenerates `llms.txt`; commit it if it changed).
- [ ] **Step 5: Commit** — `git commit -m "docs: trigger run status guide, migration 053 note, CLAUDE.md"`
