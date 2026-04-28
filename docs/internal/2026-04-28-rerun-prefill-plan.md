# Re-run Prefill & Job Lineage — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the **Re-run** button on the job detail page pre-populate the task form with the original job's input values — including connection-type fields (by name) and non-default secret fields (replayed server-side, never exposed to the UI). Also lay the foundation for Restart-from-failed-step (feature B) by introducing a unified job-lineage model.

**Architecture:** Persist a new `raw_input` JSONB column on the `job` table — the user's submission verbatim, before defaults are merged or connections resolved. Add a `source_job_id` lineage pointer plus extended `source_type` values (`'rerun'`, `'restart'`). The UI sends `source_job_id` plus form values on Re-run; for fields the user didn't touch, the UI submits the redaction sentinel `••••••` and the server replaces it with `source.raw_input[field]` before running the existing `merge_defaults` → `resolve_connection_inputs` pipeline.

**Tech Stack:** Rust (sqlx, axum, anyhow), TypeScript (React 19, react-router 6+), PostgreSQL. Spec at `docs/internal/2026-04-28-rerun-prefill-design.md`.

---

## File Structure

**Server (Rust):**
- Create: `crates/stroem-db/migrations/032_job_raw_input_and_lineage.sql`
- Modify: `crates/stroem-db/src/repos/job.rs` — extend `JobRow`, `JOB_COLUMNS`, and the `create_with_parent_tx_id` INSERT to carry the new fields.
- Modify: `crates/stroem-common/src/template.rs` — add `resolve_rerun_sentinels` helper + unit tests.
- Modify: `crates/stroem-server/src/job_creator.rs` — accept optional `source_job_id` arg, run sentinel resolution, persist `raw_input`.
- Modify: `crates/stroem-server/src/web/api/jobs.rs` — extend `JobDetailResponse` with `raw_input`, `source_job_id`, `restart_from_step`; redact `raw_input` in `redact_response`.
- Modify: `crates/stroem-server/src/web/api/tasks.rs` — `ExecuteTaskRequest` accepts optional `source_job_id`; handler validates source job and forwards.
- Create: `crates/stroem-server/tests/rerun_integration_test.rs` — end-to-end Re-run flow against a test PG database.

**UI (TypeScript):**
- Modify: `ui/src/components/task/constants.ts` — add `REDACTED_SENTINEL`.
- Modify: `ui/src/lib/types.ts` — extend `JobDetail`.
- Modify: `ui/src/lib/api.ts` — extend `executeTask` signature with `sourceJobId` option.
- Modify: `ui/src/pages/job-detail.tsx` — Re-run Link carries `{sourceJobId, rawInput}`; lineage InfoGrid item.
- Modify: `ui/src/pages/task-detail.tsx` — read prefill from router state, apply precedence, translate sentinels on submit, legacy banner.
- Modify: `ui/src/components/task/input-field-row.tsx` — helper text under secret fields when prefilled with `REDACTED_SENTINEL`.
- Modify: `ui/e2e/jobs.spec.ts` — Playwright e2e for plain-field prefill + lineage.

**Documentation:**
- Modify: `CLAUDE.md` — short subsection under "Database" describing `raw_input` / `source_job_id` / extended `source_type` and the retry-vs-rerun-vs-restart distinction.

---

## Phase 1 — Server foundation (DB + types)

### Task 1: DB migration

**Files:**
- Create: `crates/stroem-db/migrations/032_job_raw_input_and_lineage.sql`

- [ ] **Step 1: Write the migration**

Create `crates/stroem-db/migrations/032_job_raw_input_and_lineage.sql`:

```sql
-- Re-run Prefill & Job Lineage (feature A) + reserved fields for Restart-from-step (feature B).
-- See docs/internal/2026-04-28-rerun-prefill-design.md.

ALTER TABLE job
    ADD COLUMN raw_input JSONB,
    ADD COLUMN source_job_id UUID REFERENCES job(job_id) ON DELETE SET NULL,
    ADD COLUMN restart_from_step TEXT;

-- Extend source_type CHECK to accept 'rerun' and 'restart'.
-- Previous values (after migration 031): api, trigger, webhook, task, hook, user, mcp,
-- agent_tool, event_source, retry, upload.
ALTER TABLE job DROP CONSTRAINT IF EXISTS job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN (
        'api', 'trigger', 'webhook', 'task', 'hook',
        'user', 'mcp', 'agent_tool', 'event_source',
        'retry', 'upload',
        'rerun', 'restart'
    ));

-- Lineage lookup index for "what jobs are re-runs/restarts of X".
CREATE INDEX idx_job_source_job_id
    ON job(source_job_id)
    WHERE source_job_id IS NOT NULL;
```

- [ ] **Step 2: Verify the migration applies cleanly**

The migrations directory is loaded by `sqlx::migrate!()` at server startup. Run the existing test that boots a Postgres testcontainer to verify nothing breaks:

```bash
cargo test -p stroem-db --lib repos::job::tests
```

Expected: PASS (existing tests still pass after schema change). The new columns are nullable, so they don't affect existing INSERTs.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-db/migrations/032_job_raw_input_and_lineage.sql
git commit -m "Add migration for job.raw_input and lineage columns"
```

---

### Task 2: Extend `JobRepo` to persist and return new columns

**Files:**
- Modify: `crates/stroem-db/src/repos/job.rs`

- [ ] **Step 1: Update `JOB_COLUMNS` constant**

Edit `crates/stroem-db/src/repos/job.rs` line 8. Replace:

```rust
const JOB_COLUMNS: &str = "job_id, workspace, task_name, mode, input, output, status, source_type, source_id, worker_id, revision, created_at, started_at, completed_at, log_path, parent_job_id, parent_step_name, timeout_secs, retry_of_job_id, retry_job_id, retry_attempt, max_retries";
```

With:

```rust
const JOB_COLUMNS: &str = "job_id, workspace, task_name, mode, input, output, status, source_type, source_id, worker_id, revision, created_at, started_at, completed_at, log_path, parent_job_id, parent_step_name, timeout_secs, retry_of_job_id, retry_job_id, retry_attempt, max_retries, raw_input, source_job_id, restart_from_step";
```

- [ ] **Step 2: Add fields to `JobRow`**

In the same file, extend the `JobRow` struct (currently ending at line 43) with:

```rust
    pub raw_input: Option<JsonValue>,
    pub source_job_id: Option<Uuid>,
    pub restart_from_step: Option<String>,
```

So the struct now ends with:

```rust
    pub max_retries: Option<i32>,
    pub raw_input: Option<JsonValue>,
    pub source_job_id: Option<Uuid>,
    pub restart_from_step: Option<String>,
}
```

- [ ] **Step 3: Extend `create_with_parent_tx_id` signature and INSERT**

In the same file, modify `create_with_parent_tx_id` (starts line 161). Add three new params just before the where-clause:

```rust
        revision: Option<&str>,
        raw_input: Option<JsonValue>,
        source_job_id: Option<Uuid>,
        restart_from_step: Option<&str>,
    ) -> Result<Uuid>
```

Update the INSERT inside the function body to:

```rust
        sqlx::query(
            r#"
            INSERT INTO job (job_id, workspace, task_name, mode, input, source_type, source_id, parent_job_id, parent_step_name, timeout_secs, revision, raw_input, source_job_id, restart_from_step)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            "#,
        )
        .bind(job_id)
        .bind(workspace)
        .bind(task_name)
        .bind(mode)
        .bind(input)
        .bind(source_type)
        .bind(source_id)
        .bind(parent_job_id)
        .bind(parent_step_name)
        .bind(timeout_secs)
        .bind(revision)
        .bind(raw_input)
        .bind(source_job_id)
        .bind(restart_from_step)
        .execute(executor)
        .await
        .context("Failed to create job")?;
```

- [ ] **Step 4: Thread the new args through `create`, `create_with_parent`, `create_with_parent_tx`**

The same file has three thinner wrappers that delegate. Update each to accept and forward the three new fields. For `create_with_parent_tx` (line 126), add the three params before the where-clause and pass them to `create_with_parent_tx_id`. For `create_with_parent` (line 91), do the same. For `create` (line 63), add only `raw_input: Option<JsonValue>` (the simple top-level path doesn't need lineage), and pass `None, None` for `source_job_id` and `restart_from_step`.

This is a mechanical change. Each wrapper just adds the new params and forwards.

- [ ] **Step 5: Compile-check**

```bash
cargo check -p stroem-db
```

Expected: PASS. Fails will be in the same crate (none expected since we only changed signatures internally so far).

- [ ] **Step 6: Run existing repo tests**

```bash
cargo test -p stroem-db --lib repos::job
```

Expected: PASS. If a test calls one of the modified `create*` wrappers, update it to pass `None` for the new parameters.

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-db/src/repos/job.rs
git commit -m "Extend JobRepo to persist raw_input and lineage fields"
```

---

### Task 3: Add `resolve_rerun_sentinels` helper (TDD)

**Files:**
- Modify: `crates/stroem-common/src/template.rs`

- [ ] **Step 1: Write failing tests**

Add inside the existing `mod tests { ... }` block in `crates/stroem-common/src/template.rs`:

```rust
    #[test]
    fn test_resolve_rerun_sentinels_secret_with_source_value() {
        let mut schema = HashMap::new();
        schema.insert(
            "password".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: true,
                ..Default::default()
            },
        );
        let incoming = json!({"password": "••••••"});
        let source_raw = json!({"password": "real-secret-value"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        assert_eq!(result, json!({"password": "real-secret-value"}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_secret_without_source_value() {
        let mut schema = HashMap::new();
        schema.insert(
            "password".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: true,
                ..Default::default()
            },
        );
        let incoming = json!({"password": "••••••"});
        let source_raw = json!({}); // source didn't override the secret
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        // Field removed so merge_defaults will fill from schema default.
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_connection_with_source_value() {
        let mut schema = HashMap::new();
        schema.insert(
            "db".to_string(),
            InputFieldDef {
                field_type: "Postgres".to_string(), // non-primitive => connection type
                secret: false,
                ..Default::default()
            },
        );
        let incoming = json!({"db": "••••••"});
        let source_raw = json!({"db": "production-db"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        assert_eq!(result, json!({"db": "production-db"}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_non_sentinel_passthrough() {
        let mut schema = HashMap::new();
        schema.insert(
            "name".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: false,
                ..Default::default()
            },
        );
        let incoming = json!({"name": "alice"});
        let source_raw = json!({"name": "bob"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        // Non-secret, non-connection field with non-sentinel value: untouched.
        assert_eq!(result, json!({"name": "alice"}));
    }

    #[test]
    fn test_resolve_rerun_sentinels_plain_field_with_sentinel_untouched() {
        let mut schema = HashMap::new();
        schema.insert(
            "note".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                secret: false,
                ..Default::default()
            },
        );
        // User literally typed bullets into a plain string field. Server should
        // store as-is — sentinel only meaningful for secret/connection types.
        let incoming = json!({"note": "••••••"});
        let source_raw = json!({"note": "previous"});
        let result = resolve_rerun_sentinels(&incoming, &source_raw, &schema).unwrap();
        assert_eq!(result, json!({"note": "••••••"}));
    }
```

Note: this requires `InputFieldDef` to derive or implement `Default`. If it doesn't, replace `..Default::default()` with explicit fields. Run `cargo check` if unsure; the type is in `crates/stroem-common/src/models/workflow.rs`.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p stroem-common --lib template::tests::test_resolve_rerun_sentinels
```

Expected: FAIL — `resolve_rerun_sentinels` doesn't exist yet.

- [ ] **Step 3: Implement the function**

Add to `crates/stroem-common/src/template.rs`, alongside `merge_defaults` and `resolve_connection_inputs` (e.g., after `resolve_connection_inputs`):

```rust
/// Sentinel string used by the server to redact secret values in API responses.
/// The UI sends this exact byte sequence on Re-run for fields the user did not
/// edit, signalling "replay this from the source job's `raw_input`".
pub const REDACTED_SENTINEL: &str = "••••••";

/// Resolve Re-run "reuse from source" sentinels in the incoming input.
///
/// For each field in the schema that is a secret or connection type, if the
/// incoming value equals [`REDACTED_SENTINEL`]:
/// - If the source job's `raw_input` has that field, replace the sentinel with
///   the source value.
/// - Otherwise, remove the field entirely so `merge_defaults` fills it from
///   the schema default (supports secret rotation).
///
/// All other fields pass through unchanged. The sentinel only has meaning for
/// secret and connection-typed fields — a user typing literal bullets into a
/// plain field is preserved verbatim.
pub fn resolve_rerun_sentinels(
    incoming: &serde_json::Value,
    source_raw_input: &serde_json::Value,
    input_schema: &HashMap<String, InputFieldDef>,
) -> Result<serde_json::Value> {
    let empty = serde_json::Map::new();
    let incoming_map = incoming.as_object().unwrap_or(&empty);
    let source_map = source_raw_input.as_object().unwrap_or(&empty);

    let mut result = incoming_map.clone();

    for (field_name, field_def) in input_schema {
        let is_connection = !PRIMITIVE_TYPES.contains(&field_def.field_type.as_str());
        let is_secret_or_connection = field_def.secret || is_connection;
        if !is_secret_or_connection {
            continue;
        }

        let value = match result.get(field_name) {
            Some(v) => v,
            None => continue,
        };
        if value.as_str() != Some(REDACTED_SENTINEL) {
            continue;
        }

        // Sentinel detected on a secret/connection field — replay or drop.
        match source_map.get(field_name) {
            Some(source_value) => {
                result.insert(field_name.clone(), source_value.clone());
            }
            None => {
                result.remove(field_name);
            }
        }
    }

    Ok(serde_json::Value::Object(result))
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p stroem-common --lib template::tests::test_resolve_rerun_sentinels
```

Expected: PASS (all 5 new tests).

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/template.rs
git commit -m "Add resolve_rerun_sentinels helper"
```

---

## Phase 2 — Server logic (job_creator + API)

### Task 4: `job_creator.rs` accepts `source_job_id`, persists `raw_input`

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs`

- [ ] **Step 1: Add `source_job_id` parameter to public functions**

In `crates/stroem-server/src/job_creator.rs`:

- `create_job_for_task` (line 26): add `source_job_id: Option<Uuid>` as the last argument before `agents_config`. Pass it through to `create_job_for_task_inner`.
- `create_child_job_for_task` (line 58): unchanged externally; child paths never set `source_job_id`. Pass `None` to inner.
- `create_job_for_task_inner` (line 88): add `source_job_id: Option<Uuid>` parameter.

- [ ] **Step 2: Update inner to fetch source raw_input, run sentinel resolution, persist raw_input**

Inside `create_job_for_task_inner` body, replace the block from line 110 (`let secrets_ctx = ...`) up to (and including) the call to `JobRepo::create_with_parent_tx_id` (line 199–215) with:

```rust
        // Re-run flow: if the caller named a source job, resolve any "reuse from source"
        // sentinels in the incoming input by looking up the source's raw_input. Done
        // BEFORE merge_defaults so a sentinel that the source did not override falls
        // through to the schema default (supports secret rotation).
        let mut effective_input = input;
        if let Some(src_id) = source_job_id {
            let source_job = stroem_db::JobRepo::get(pool, src_id)
                .await
                .context("fetch source job for re-run")?
                .ok_or_else(|| anyhow::anyhow!("Source job {} not found", src_id))?;
            if source_job.workspace != workspace_name {
                bail!(
                    "Source job {} belongs to workspace '{}', cannot Re-run into '{}'",
                    src_id,
                    source_job.workspace,
                    workspace_name
                );
            }
            let source_raw = source_job.raw_input.unwrap_or_else(|| serde_json::json!({}));
            if source_raw.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                bail!(
                    "Source job {} predates Re-run prefill (no raw_input)",
                    src_id
                );
            }
            effective_input = stroem_common::template::resolve_rerun_sentinels(
                &effective_input,
                &source_raw,
                &task.input,
            )
            .context("resolve re-run sentinels")?;
        }

        // Capture the user's submission verbatim before defaults/connections are merged.
        let raw_input_to_persist = Some(effective_input.clone());

        // Merge input defaults from the task schema
        let secrets_ctx = serde_json::json!({ "secret": workspace_config.secrets });
        let merged_input = merge_defaults(&effective_input, &task.input, &secrets_ctx)
            .context("Failed to merge input defaults")?;

        // Resolve connection inputs (replace connection names with full objects)
        let resolved_input =
            resolve_connection_inputs(&merged_input, &task.input, workspace_config)
                .context("Failed to resolve connection inputs")?;
```

Then leave the existing `for (step_name, flow_step) in &task.flow {` block (which starts at line 125) and the surrounding step-creation logic unchanged. When you reach the `JobRepo::create_with_parent_tx_id` call, update it to pass the new fields:

```rust
        JobRepo::create_with_parent_tx_id(
            &mut *tx,
            job_id,
            workspace_name,
            task_name,
            &task.mode,
            Some(resolved_input),
            source_type,
            source_id,
            parent_job_id,
            parent_step_name,
            task.timeout
                .map(|d| i32::try_from(d.as_secs()).expect("timeout validated to fit i32")),
            revision,
            raw_input_to_persist,
            source_job_id,
            None, // restart_from_step (feature B)
        )
        .await
        .context("Failed to create job")?;
```

Also: the function signature for `input` was `serde_json::Value`. We renamed the local var to `effective_input` so the original `input` is consumed once at the top — no other code referenced the original after this point.

- [ ] **Step 3: Update the recursive `create_job_for_task_inner` call**

Inside `job_creator.rs` around line 407 (the recursive call inside `handle_task_steps`), add `None` for the new `source_job_id` argument so type-task children always get `None`:

```rust
        match create_job_for_task_inner(
            pool,
            workspace_config,
            workspace_name,
            sub_task_name,
            sub_input_value,
            "task",
            Some(&format!("{parent_job_id}/{step_name}")),
            Some(parent_job_id),
            Some(step_name),
            revision,
            None, // agents_config — children dispatch through the orchestrator
            None, // source_job_id
        )
```

(Adjust to whatever exact call shape exists at that line; the only change is appending `None` for `source_job_id` after the existing `None` for agents_config.)

- [ ] **Step 4: Update all external call sites of `create_job_for_task`**

Pass `None` for `source_job_id` from every caller except the one we'll change next (the API handler):

- `crates/stroem-server/src/scheduler.rs:386`
- `crates/stroem-server/src/event_source.rs:585`
- `crates/stroem-server/src/web/worker_api/event_source.rs:95`
- `crates/stroem-server/src/web/hooks.rs:95`
- `crates/stroem-server/src/hooks.rs:372`
- `crates/stroem-server/src/mcp/tools.rs:398`
- `crates/stroem-server/src/job_recovery.rs:840`

Each is a simple addition of `None` after the existing `agents_config` argument (or wherever the new argument lands per Step 1).

- [ ] **Step 5: Compile**

```bash
cargo check --workspace
```

Expected: PASS. Any callers not yet updated will surface here.

- [ ] **Step 6: Run server unit tests**

```bash
cargo test -p stroem-server --lib
```

Expected: PASS. (No behavioral test on `create_job_for_task_inner` exists yet at the unit level; integration tests cover it in Task 7.)

- [ ] **Step 7: Commit**

```bash
git add crates/stroem-server/src/job_creator.rs \
        crates/stroem-server/src/scheduler.rs \
        crates/stroem-server/src/event_source.rs \
        crates/stroem-server/src/web/worker_api/event_source.rs \
        crates/stroem-server/src/web/hooks.rs \
        crates/stroem-server/src/hooks.rs \
        crates/stroem-server/src/mcp/tools.rs \
        crates/stroem-server/src/job_recovery.rs
git commit -m "Plumb source_job_id and persist raw_input on job creation"
```

---

### Task 5: Extend `JobDetailResponse` and redact `raw_input`

**Files:**
- Modify: `crates/stroem-server/src/web/api/jobs.rs`

- [ ] **Step 1: Add new fields to `JobDetailResponse`**

In `crates/stroem-server/src/web/api/jobs.rs` line ~256, extend the struct:

```rust
pub struct JobDetailResponse {
    pub job_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub mode: String,
    pub input: Option<serde_json::Value>,
    pub raw_input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub status: String,
    pub source_type: String,
    pub source_id: Option<String>,
    pub source_job_id: Option<Uuid>,
    pub restart_from_step: Option<String>,
    pub revision: Option<String>,
    pub worker_id: Option<Uuid>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub steps: Vec<serde_json::Value>,
    pub retry_of_job_id: Option<Uuid>,
    pub retry_job_id: Option<Uuid>,
    pub retry_attempt: i32,
    pub max_retries: Option<i32>,
}
```

- [ ] **Step 2: Populate new fields in the `get_job` handler**

In the same file at line ~432 (the `let mut response = JobDetailResponse { ... }` block), add:

```rust
    let mut response = JobDetailResponse {
        job_id: job.job_id,
        workspace: job.workspace,
        task_name: job.task_name,
        mode: job.mode,
        input: job.input,
        raw_input: job.raw_input,
        output: job.output,
        status: job.status,
        source_type: job.source_type,
        source_id: job.source_id,
        source_job_id: job.source_job_id,
        restart_from_step: job.restart_from_step,
        revision: job.revision,
        worker_id: job.worker_id,
        created_at: job.created_at.to_rfc3339(),
        started_at: job.started_at.map(|dt| dt.to_rfc3339()),
        completed_at: job.completed_at.map(|dt| dt.to_rfc3339()),
        steps: steps_json,
        retry_of_job_id: job.retry_of_job_id,
        retry_job_id: job.retry_job_id,
        retry_attempt: job.retry_attempt,
        max_retries: job.max_retries,
    };
```

- [ ] **Step 3: Extend `redact_response` to redact `raw_input`**

In the same file at line ~522, locate `fn redact_response`. Add a redaction call for `raw_input`:

```rust
fn redact_response(response: &mut JobDetailResponse, secret_values: &[String]) {
    if let Some(ref mut input) = response.input {
        redact_json(input, secret_values);
    }
    if let Some(ref mut raw_input) = response.raw_input {
        redact_json(raw_input, secret_values);
    }
    if let Some(ref mut output) = response.output {
        redact_json(output, secret_values);
    }
    // ... existing per-step redaction loop
```

(Keep the rest of the function body unchanged.)

- [ ] **Step 4: Add a unit test for raw_input redaction**

In the existing tests module of the same file (where `test_redact_response` lives at line ~1131), add:

```rust
    #[test]
    fn test_redact_response_redacts_raw_input() {
        let secrets = vec!["my-secret-token".to_string()];
        let mut response = JobDetailResponse {
            job_id: Uuid::nil(),
            workspace: "default".to_string(),
            task_name: "t".to_string(),
            mode: "distributed".to_string(),
            input: None,
            raw_input: Some(json!({"token": "my-secret-token", "name": "alice"})),
            output: None,
            status: "completed".to_string(),
            source_type: "rerun".to_string(),
            source_id: None,
            source_job_id: Some(Uuid::nil()),
            restart_from_step: None,
            revision: None,
            worker_id: None,
            created_at: "".to_string(),
            started_at: None,
            completed_at: None,
            steps: vec![],
            retry_of_job_id: None,
            retry_job_id: None,
            retry_attempt: 0,
            max_retries: None,
        };
        redact_response(&mut response, &secrets);
        let raw = response.raw_input.unwrap();
        assert_eq!(raw["token"], json!(REDACTED));
        assert_eq!(raw["name"], json!("alice")); // non-secret untouched
    }
```

- [ ] **Step 5: Run the test**

```bash
cargo test -p stroem-server --lib web::api::jobs::tests
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/web/api/jobs.rs
git commit -m "Expose raw_input and lineage in JobDetailResponse with redaction"
```

---

### Task 6: `/execute` accepts `source_job_id`, validates and authorizes

**Files:**
- Modify: `crates/stroem-server/src/web/api/tasks.rs`

- [ ] **Step 1: Extend `ExecuteTaskRequest`**

At line ~54 of `crates/stroem-server/src/web/api/tasks.rs`:

```rust
#[derive(Debug, Deserialize)]
pub struct ExecuteTaskRequest {
    #[serde(default)]
    pub input: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub source_job_id: Option<Uuid>,
}
```

Add `use uuid::Uuid;` if not already imported in this file.

- [ ] **Step 2: Validate source job (if present) and select source_type**

In `execute_task` (line ~278), insert this block right after the existing ACL-check block (line ~320), before `let input_value = ...`:

```rust
    // 4. Re-run validation: source_job_id must reference a job in this workspace
    //    that the user is allowed to view. Authorization mirrors GET /api/jobs/{id}.
    let mut effective_source_type = source_type;
    if let Some(src_id) = req.source_job_id {
        let source_job = stroem_db::JobRepo::get(&state.pool, src_id)
            .await
            .context("load source job for re-run")?
            .ok_or_else(|| AppError::BadRequest(format!("Source job {} not found", src_id)))?;
        if source_job.workspace != ws {
            return Err(AppError::BadRequest(
                "Source job belongs to a different workspace".into(),
            ));
        }
        if source_job.raw_input.is_none() {
            return Err(AppError::BadRequest(
                "Source job predates Re-run prefill (no raw_input)".into(),
            ));
        }
        // Authorization: user must have at least View on the source job's task path.
        let perm = check_source_job_permission(
            &state,
            &auth_user,
            &source_job.workspace,
            &source_job.task_name,
        )
        .await?;
        if matches!(perm, TaskPermission::Deny) {
            return Err(AppError::Forbidden(
                "Not authorized to read source job".into(),
            ));
        }
        effective_source_type = "rerun";
    }
```

`check_source_job_permission` is the same logic as the private `check_job_acl` already in `web/api/jobs.rs`. To avoid duplication, **export** that function: change its visibility to `pub(crate)` in `crates/stroem-server/src/web/api/jobs.rs` line ~950 (`async fn check_job_acl` → `pub(crate) async fn check_job_acl`), and import it here:

```rust
use crate::web::api::jobs::check_job_acl as check_source_job_permission;
```

- [ ] **Step 3: Pass `source_job_id` and `effective_source_type` to `create_job_for_task`**

Update the call at line ~326:

```rust
    let job_id = create_job_for_task(
        &state.pool,
        &workspace,
        &ws,
        &name,
        input_value,
        effective_source_type,
        source_id.as_deref(),
        revision.as_deref(),
        state.config.agents.as_ref(),
        req.source_job_id,
    )
```

- [ ] **Step 4: Compile**

```bash
cargo check --workspace
```

Expected: PASS.

- [ ] **Step 5: Verify ACL test compiles + passes**

```bash
cargo test -p stroem-server --lib
```

Expected: PASS. (Detailed e2e in Task 7.)

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/web/api/tasks.rs crates/stroem-server/src/web/api/jobs.rs
git commit -m "Accept source_job_id on /execute with workspace + ACL gating"
```

---

## Phase 3 — Server integration test

### Task 7: End-to-end Re-run flow against test Postgres

**Files:**
- Create: `crates/stroem-server/tests/rerun_integration_test.rs`

- [ ] **Step 1: Look at how existing integration tests are wired**

Read the first 80 lines of `crates/stroem-server/tests/integration_test.rs` to find the `setup_test_app()` (or equivalent) helper and how `data-pipeline` or similar test tasks are defined inline. The new test follows the same harness — testcontainers Postgres, in-memory workspace config, etc.

- [ ] **Step 2: Write the integration test**

Create `crates/stroem-server/tests/rerun_integration_test.rs`. Use the existing harness to:

1. Define a test task with three input fields: a plain string, a connection-typed field (e.g., `db: { type: "Postgres" }` with `connections: { production-db: { type: "Postgres", values: { host: "..." } } }`), and a secret-typed string field (`api_key: { type: "string", secret: true, default: "{{ secret.api_key }}" }`) with the secret in workspace config (`secrets: { api_key: "schema-default" }`).
2. Trigger an initial job via `POST /api/workspaces/default/tasks/<task>/execute` with custom values for all three fields (e.g. `data: "hello"`, `db: "production-db"`, `api_key: "custom-secret"`).
3. Wait for the job to complete (or use `set_job_status_completed` if available — check existing test helpers).
4. Fetch the job via `GET /api/jobs/{id}` and assert:
   - `raw_input.data == "hello"`
   - `raw_input.db == "production-db"` (connection name preserved)
   - `raw_input.api_key == "••••••"` (redacted)
   - `source_job_id == null`
5. Trigger a Re-run via `POST /api/workspaces/default/tasks/<task>/execute` with body `{"input": {"data": "hello", "db": "••••••", "api_key": "••••••"}, "source_job_id": <first_job_id>}`. (The UI sends `••••••` for connection/secret fields the user didn't touch.)
6. Fetch the second job and assert:
   - `source_job_id == <first_job_id>`
   - `source_type == "rerun"`
   - `input.db` resolves to the full connection object (proving the `••••••` was replaced with `"production-db"` then resolved).
   - `input.api_key == "custom-secret"` (or `••••••` post-redaction — verify against the API response, which redacts).
7. Negative cases: Re-run with `source_job_id` from a different workspace → 400; Re-run with `source_job_id` of a legacy job (manually NULL out `raw_input` via raw SQL, or omit) → 400.

A skeletal version:

```rust
use axum::http::StatusCode;
// ... existing test imports

#[tokio::test(flavor = "multi_thread")]
async fn rerun_replays_connection_and_secret_from_source() {
    let harness = TestHarness::new().await;
    harness
        .add_task(/* task YAML with plain + connection + secret inputs */)
        .await;

    // First job: custom values for all three fields.
    let first_id = harness
        .execute_task(
            "default",
            "rerun-fixture",
            json!({"data": "hello", "db": "production-db", "api_key": "custom-secret"}),
            None,
        )
        .await;
    harness.wait_for_job(first_id).await;

    let first_resp = harness.get_job(first_id).await;
    assert_eq!(first_resp["raw_input"]["data"], json!("hello"));
    assert_eq!(first_resp["raw_input"]["db"], json!("production-db"));
    assert_eq!(first_resp["raw_input"]["api_key"], json!("••••••"));
    assert!(first_resp["source_job_id"].is_null());

    // Re-run: UI sends sentinels for connection + secret fields.
    let second_id = harness
        .execute_task(
            "default",
            "rerun-fixture",
            json!({"data": "hello", "db": "••••••", "api_key": "••••••"}),
            Some(first_id),
        )
        .await;
    harness.wait_for_job(second_id).await;

    let second_resp = harness.get_job(second_id).await;
    assert_eq!(second_resp["source_job_id"], json!(first_id.to_string()));
    assert_eq!(second_resp["source_type"], json!("rerun"));
    // db sentinel got replaced with "production-db" then resolved to the values object.
    assert!(second_resp["input"]["db"].is_object());
    // raw_input shows the same shape as the source.
    assert_eq!(second_resp["raw_input"]["db"], json!("production-db"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_with_unknown_source_job_returns_400() {
    let harness = TestHarness::new().await;
    let res = harness
        .execute_task_raw(
            "default",
            "any-task",
            json!({}),
            Some(uuid::Uuid::new_v4()),
        )
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
```

Adapt method names (`add_task`, `execute_task`, `wait_for_job`, `execute_task_raw`) to whatever the existing harness exposes — they may be different names. The point is: 1 happy-path test + 2 rejection tests.

- [ ] **Step 3: Run the integration test**

```bash
cargo test -p stroem-server --test rerun_integration_test
```

Expected: PASS. Requires Docker (testcontainers).

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/tests/rerun_integration_test.rs
git commit -m "Add integration test for Re-run prefill end-to-end"
```

---

## Phase 4 — UI types & constants

### Task 8: Add `REDACTED_SENTINEL` UI constant

**Files:**
- Modify: `ui/src/components/task/constants.ts`

- [ ] **Step 1: Add the constant**

In `ui/src/components/task/constants.ts`:

```typescript
export const SECRET_SENTINEL = "********";
// Server-side redaction marker used in API responses (and on the wire when the UI
// asks the server to "replay this from the source job's raw_input"). Must match
// the Rust REDACTED constant in crates/stroem-server/src/web/api/jobs.rs.
export const REDACTED_SENTINEL = "••••••";
// Must match PRIMITIVE_TYPES in crates/stroem-common/src/template.rs
export const PRIMITIVE_TYPES = new Set([
  "string",
  "text",
  "integer",
  "number",
  "boolean",
  "date",
  "datetime",
]);
```

- [ ] **Step 2: Type-check**

```bash
cd ui && bunx tsc --noEmit
```

Expected: PASS (no usages yet).

- [ ] **Step 3: Commit**

```bash
git add ui/src/components/task/constants.ts
git commit -m "Add REDACTED_SENTINEL constant matching server REDACTED"
```

---

### Task 9: Update `JobDetail` type and `executeTask` signature

**Files:**
- Modify: `ui/src/lib/types.ts`
- Modify: `ui/src/lib/api.ts`

- [ ] **Step 1: Extend `JobDetail`**

In `ui/src/lib/types.ts`, add three fields to the `JobDetail` interface:

```typescript
export interface JobDetail {
  job_id: string;
  workspace: string;
  task_name: string;
  mode: string;
  input: Record<string, unknown> | null;
  raw_input: Record<string, unknown> | null;
  output: Record<string, unknown> | null;
  status: string;
  source_type: string;
  source_id: string | null;
  source_job_id: string | null;
  restart_from_step: string | null;
  revision: string | null;
  worker_id: string | null;
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
  retry_of_job_id: string | null;
  retry_job_id: string | null;
  retry_attempt: number;
  max_retries: number | null;
  steps: JobStep[];
}
```

- [ ] **Step 2: Extend `executeTask`**

Find the existing `executeTask` function in `ui/src/lib/api.ts`. Locate it with:

```bash
grep -n "executeTask" ui/src/lib/api.ts
```

Modify the signature so the body optionally includes `source_job_id`. Likely shape:

```typescript
export async function executeTask(
  workspace: string,
  taskId: string,
  input: Record<string, unknown>,
  opts?: { sourceJobId?: string },
): Promise<ExecuteTaskResponse> {
  const body: Record<string, unknown> = { input };
  if (opts?.sourceJobId) body.source_job_id = opts.sourceJobId;
  return apiFetch<ExecuteTaskResponse>(
    `/api/workspaces/${encodeURIComponent(workspace)}/tasks/${encodeURIComponent(taskId)}/execute`,
    { method: "POST", body: JSON.stringify(body) },
  );
}
```

(Use whatever fetch wrapper currently exists in `api.ts` — don't introduce a new one. The change is: build the body conditionally instead of always `{ input }`.)

- [ ] **Step 3: Type-check**

```bash
cd ui && bunx tsc --noEmit
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add ui/src/lib/types.ts ui/src/lib/api.ts
git commit -m "Extend JobDetail type and executeTask signature for Re-run"
```

---

## Phase 5 — UI implementation

### Task 10: Re-run Link passes prefill state + lineage InfoGrid

**Files:**
- Modify: `ui/src/pages/job-detail.tsx`

- [ ] **Step 1: Update the Re-run Link**

In `ui/src/pages/job-detail.tsx`, locate the existing Re-run button (around line 125):

```tsx
<Button variant="outline" asChild>
  <Link to={`/workspaces/${encodeURIComponent(job.workspace)}/tasks/${encodeURIComponent(job.task_name)}`}>
    Re-run
  </Link>
</Button>
```

Replace with:

```tsx
<Button variant="outline" asChild>
  <Link
    to={`/workspaces/${encodeURIComponent(job.workspace)}/tasks/${encodeURIComponent(job.task_name)}`}
    state={{ sourceJobId: job.job_id, rawInput: job.raw_input }}
  >
    Re-run
  </Link>
</Button>
```

- [ ] **Step 2: Add lineage InfoGrid item**

In the same file, locate the existing `InfoGrid items` array (around line 148–207, where `retry_of_job_id` and `retry_job_id` items are conditionally appended). Add a similar conditional for re-runs after the existing retry entries:

```tsx
          ...(job.source_job_id && job.source_type === "rerun"
            ? [
                {
                  label: "Re-run of",
                  value: (
                    <Link
                      to={`/jobs/${job.source_job_id}`}
                      className="font-mono text-xs text-primary hover:underline"
                    >
                      {job.source_job_id.substring(0, 8)}
                    </Link>
                  ),
                },
              ]
            : []),
```

- [ ] **Step 3: Type-check**

```bash
cd ui && bunx tsc --noEmit
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add ui/src/pages/job-detail.tsx
git commit -m "Re-run Link carries prefill state and shows lineage badge"
```

---

### Task 11: Task detail page reads prefill, applies precedence, submits with sourceJobId

**Files:**
- Modify: `ui/src/pages/task-detail.tsx`

- [ ] **Step 1: Read prefill from router state**

At the top of `ui/src/pages/task-detail.tsx`:

```typescript
import { useParams, Link, useNavigate, useLocation } from "react-router";
import { SECRET_SENTINEL, REDACTED_SENTINEL, PRIMITIVE_TYPES } from "@/components/task/constants";
```

Inside `TaskDetailPage`, after `const navigate = useNavigate();`:

```typescript
  const location = useLocation();
  const prefill = (location.state as { sourceJobId?: string; rawInput?: Record<string, unknown> | null } | null);
  const sourceJobId = prefill?.sourceJobId;
  const rawInput = prefill?.rawInput;
  const isLegacySource = !!sourceJobId && !rawInput;
```

- [ ] **Step 2: Apply prefill precedence in the load effect**

In the existing `load()` effect (currently around line 99–132 in the file before any edit), replace the `defaults` building loop with:

```typescript
          const defaults: Record<string, unknown> = {};
          for (const [key, field] of Object.entries(data.input)) {
            const prefillVal = rawInput && Object.prototype.hasOwnProperty.call(rawInput, key)
              ? rawInput[key]
              : undefined;

            if (prefillVal !== undefined) {
              // Plain, connection, or redacted-secret value from raw_input.
              defaults[key] = prefillVal;
            } else if (field.secret && field.default !== undefined) {
              defaults[key] = SECRET_SENTINEL;
            } else if (field.default !== undefined) {
              defaults[key] = field.default;
            } else if (field.type === "boolean") {
              defaults[key] = false;
            } else {
              defaults[key] = "";
            }
          }
          setValues(defaults);

          // Clear router state so a refresh shows the form without prefill.
          if (rawInput || sourceJobId) {
            window.history.replaceState(null, "", window.location.pathname + window.location.search);
          }
```

Add `// eslint-disable-next-line react-hooks/exhaustive-deps` above the closing `}, [workspace, name]);` of the effect, since `rawInput` / `sourceJobId` are intentionally captured at mount.

- [ ] **Step 3: Translate sentinels on submit**

In `handleSubmit` (currently lines ~155–178), update the field-mapping loop:

```typescript
      const input: Record<string, unknown> = {};
      for (const [key, val] of Object.entries(values)) {
        const field = task.input[key];
        // Secret defaults: if the user kept the SECRET_SENTINEL untouched, behavior depends on mode.
        if (field?.secret && val === SECRET_SENTINEL) {
          // Normal run: drop so server uses schema default. Re-run: same — schema default still wins
          // when the user hasn't touched the field. The replay sentinel is REDACTED_SENTINEL, set by
          // prefill above only when the source overrode the secret.
          continue;
        }
        if (field?.type === "number") {
          input[key] = Number(val);
        } else {
          input[key] = val;
        }
      }
      const res = await executeTask(workspace, task.id, input, sourceJobId ? { sourceJobId } : undefined);
```

The reason this works for the two-sentinel rule: when prefill set the form value to `REDACTED_SENTINEL` (Section 4 of the spec), the user either kept it (passes through to the server as `••••••` → server replays from source) or typed something new (overrides). When prefill set the form value to `SECRET_SENTINEL` (source used schema default), keeping it untouched still means "use default" and is dropped — the server has no source override to replay anyway.

- [ ] **Step 4: Render legacy-source banner**

After the existing top-level `<div className="space-y-6">` opener (around line 200), insert:

```tsx
      {isLegacySource && (
        <div className="rounded-md border border-yellow-300 bg-yellow-50 px-3 py-2 text-sm text-yellow-800 dark:border-yellow-700 dark:bg-yellow-950 dark:text-yellow-200">
          This job predates Re-run prefill — defaults shown.
        </div>
      )}
```

- [ ] **Step 5: Type-check + lint**

```bash
cd ui && bunx tsc --noEmit && bun run lint
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add ui/src/pages/task-detail.tsx
git commit -m "Prefill task form from job raw_input on Re-run"
```

---

### Task 12: Helper text on secret fields when prefilled with `REDACTED_SENTINEL`

**Files:**
- Modify: `ui/src/components/task/input-field-row.tsx`

- [ ] **Step 1: Update the secret-field branch**

In `ui/src/components/task/input-field-row.tsx`, locate the existing secret-field branch (around line 71–93):

```tsx
  if (field.secret) {
    return (
      <div className="space-y-2">
        <Label htmlFor={id}>
          {displayLabel}
          {field.required && !field.default && (
            <span className="ml-1 text-destructive">*</span>
          )}
        </Label>
        <Input
          id={id}
          type="password"
          value={String(value ?? "")}
          onChange={(e) => onChange(e.target.value)}
          placeholder={field.description || fieldKey}
          required={field.required && !field.default}
        />
        {field.description && (
          <p className="text-xs text-muted-foreground">{field.description}</p>
        )}
      </div>
    );
  }
```

Add an import for the constant at the top of the file:

```typescript
import { PRIMITIVE_TYPES, REDACTED_SENTINEL } from "@/components/task/constants";
```

(`PRIMITIVE_TYPES` is already imported.)

Then replace the secret branch with:

```tsx
  if (field.secret) {
    const isReplayPrefill = value === REDACTED_SENTINEL;
    return (
      <div className="space-y-2">
        <Label htmlFor={id}>
          {displayLabel}
          {field.required && !field.default && (
            <span className="ml-1 text-destructive">*</span>
          )}
        </Label>
        <Input
          id={id}
          type="password"
          value={String(value ?? "")}
          onChange={(e) => onChange(e.target.value)}
          placeholder={field.description || fieldKey}
          required={field.required && !field.default}
        />
        {isReplayPrefill && (
          <p className="text-xs text-muted-foreground">
            Using value from previous run — type to override.
          </p>
        )}
        {field.description && !isReplayPrefill && (
          <p className="text-xs text-muted-foreground">{field.description}</p>
        )}
      </div>
    );
  }
```

The replay text replaces the description text when both apply; the description is less informative once we're pre-filling.

- [ ] **Step 2: Type-check**

```bash
cd ui && bunx tsc --noEmit
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add ui/src/components/task/input-field-row.tsx
git commit -m "Show 'using value from previous run' helper on Re-run secret fields"
```

---

## Phase 6 — E2E test

### Task 13: Playwright test for plain-field prefill + lineage

**Files:**
- Modify: `ui/e2e/jobs.spec.ts`

- [ ] **Step 1: Add the test**

In `ui/e2e/jobs.spec.ts`, before the existing `test("job detail shows graph toggle for multi-step job", ...)` (search for that string to find the insertion point), insert:

```typescript
  test("re-run button prefills form and links lineage", async ({
    page,
    baseURL,
  }) => {
    // data-pipeline has a single string input `data` (default: "test").
    const customValue = "rerun-prefill-value";
    const sourceJobId = await triggerJob(baseURL!, "data-pipeline", {
      data: customValue,
    });

    await waitForJob(baseURL!, sourceJobId);

    await page.goto(`/jobs/${sourceJobId}`);
    await expect(
      page.getByRole("heading", { name: "data-pipeline" }),
    ).toBeVisible();

    // Click Re-run.
    await page.getByRole("link", { name: "Re-run" }).click();
    await page.waitForURL(/\/workspaces\/default\/tasks\/data-pipeline/);

    // The data input should be prefilled with the original value, not the default ("test").
    await expect(page.locator("#input-data")).toHaveValue(customValue);

    // Submit the form to create the rerun job.
    await page.getByRole("button", { name: "Run Task" }).click();
    await page.waitForURL(/\/jobs\/.+/);

    // Lineage badge: "Re-run of" should appear with a link back to the source job.
    const rerunOfLabel = page.locator("p").filter({ hasText: /^Re-run of$/ });
    await expect(rerunOfLabel).toBeVisible();
    const rerunOfCard = rerunOfLabel.locator("..");
    await expect(rerunOfCard.locator("a")).toContainText(
      sourceJobId.substring(0, 8),
    );
  });
```

Note: this exercises the API end-to-end through the UI. The existing helper `triggerJob` posts directly to `/execute` and does NOT include `source_job_id`, so the source job has `source_type: api`/`user`. The Re-run is performed via the UI's submit, which DOES include `source_job_id` thanks to Task 11.

- [ ] **Step 2: Run e2e in Docker**

```bash
docker compose -f docker-compose.yml -f docker-compose.test.yml up --build --abort-on-container-exit playwright
```

Expected: the new test passes alongside existing ones.

If running locally (server already up), shorter loop:

```bash
cd ui && bunx playwright test jobs.spec.ts -g "re-run button prefills form"
```

- [ ] **Step 3: Commit**

```bash
git add ui/e2e/jobs.spec.ts
git commit -m "Add e2e test for Re-run prefill and lineage badge"
```

---

## Phase 7 — Documentation

### Task 14: CLAUDE.md update

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add a "Re-run / Job Lineage" subsection**

Find the existing "### Database" subsection in `CLAUDE.md`. After the existing bullets (around the line that ends with `Sub-jobs and hook jobs inherit parent's revision.`), insert a new subsection:

```markdown
### Job Lineage (retry / rerun / restart)

- **`retry_of_job_id`** — server-initiated retry of a failed job. Same logical run, attempt N+1.
- **`source_job_id`** + **`source_type = 'rerun'`** — user clicked **Re-run** in the UI. New job uses `source.raw_input` to prefill the form; UI sends `••••••` for fields the user didn't touch and the server replaces it with the source value before merging defaults / resolving connections (see `crates/stroem-common/src/template.rs::resolve_rerun_sentinels`).
- **`source_job_id`** + **`source_type = 'restart'`** + **`restart_from_step`** — *reserved* for the Restart-from-failed-step feature.
- **`raw_input`** — verbatim user submission stored on every job, before `merge_defaults` and `resolve_connection_inputs`. Returned by `GET /api/jobs/{id}` with secret values redacted to `••••••`. NULL for jobs created before migration 032.
```

- [ ] **Step 2: Sanity-check that no existing instructions contradict the new subsection**

```bash
grep -n "raw_input\|source_job_id" CLAUDE.md
```

Expected: only the new occurrences (no stale references).

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "Document Re-run prefill and job lineage in CLAUDE.md"
```

---

## Final verification

- [ ] **Step 1: Run the full Rust CI checks**

```bash
cargo fmt --check --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

Expected: all PASS.

- [ ] **Step 2: Run the full UI CI checks**

```bash
cd ui && bun run lint
cd ui && bunx tsc --noEmit
```

Expected: PASS.

- [ ] **Step 3: Run e2e once more end-to-end**

```bash
docker compose -f docker-compose.yml -f docker-compose.test.yml up --build --abort-on-container-exit playwright
```

Expected: PASS.

- [ ] **Step 4: Final review of the diff**

```bash
git log --oneline main..HEAD
```

Each commit should map to one task in this plan.

---

## Notes for implementers

- **Order matters:** Phase 1 lays the schema and types; Phase 2 wires the server logic; Phase 3 verifies it; Phases 4–5 build the UI on top of stable types; Phase 6 verifies end-to-end. Skipping ahead — e.g., starting on the UI before the API is ready — is possible but risks reworking against an incomplete API contract.
- **TDD where it pays:** the `resolve_rerun_sentinels` helper (Task 3) is the only logic with branching that justifies isolated unit tests. Plumbing changes (Tasks 2, 4, 5, 6) compile-and-integration-test from above. UI changes (Tasks 10–12) are covered by the Playwright e2e (Task 13).
- **No half-finished states between commits:** every commit listed leaves the workspace compiling and tests passing. If you can't satisfy that, split the work, don't bypass the rule.
- **Documentation first when stuck:** if any step looks ambiguous, re-read `docs/internal/2026-04-28-rerun-prefill-design.md` for the design rationale before guessing. The plan is the *what*; the spec is the *why*.
