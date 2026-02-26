# Phase 5 — Advanced Flow Control

## Context

Strøm workflows currently have no flow control beyond linear dependencies and `continue_on_failure`. Phase 5 adds the building blocks that make Strøm competitive with platforms like Windmill and Temporal: conditionals, loops, and human-in-the-loop approval gates.

## Phase 5 features (in implementation order)

### 5a. Conditionals (`when`)
### 5b. For-each loops (fan-out/fan-in)
### 5c. While loops (retry-until patterns)
### 5d. Suspend/Resume + Approval gates

---

## 5a. Conditionals (`when`) — Detailed Implementation Plan

### YAML syntax

```yaml
flow:
  check:
    action: check-condition

  # Branch A (multi-step)
  branch-a-1:
    action: process-data
    depends_on: [check]
    when: "{{ check.output.has_data }}"
  branch-a-2:
    action: insert-results
    depends_on: [branch-a-1]

  # Branch B (default)
  branch-b:
    action: log-empty
    depends_on: [check]
    when: "{{ not check.output.has_data }}"

  # Convergence
  collect:
    action: collect-results
    depends_on: [branch-a-2, branch-b]
    continue_on_failure: true
```

### Key design decisions

1. **Tera template syntax** — `when` is a Tera expression rendered against the same context as step inputs (job input, completed step outputs, secrets). Result truthiness: `"true"`, non-empty (excluding `"false"`, `"0"`) → run; otherwise → skip.
2. **Condition skips cascade normally** — a condition-skipped step is just `skipped`. Its dependents cascade-skip as today. No `skip_reason` column needed.
3. **`continue_on_failure` extended to accept `skipped` deps** — this is the convergence mechanism. A step with `continue_on_failure: true` is promoted when all deps are terminal (completed, failed, **or skipped**). One-line change in `promote_ready_steps()`.
4. **Condition evaluation errors → step failure** — no silent skipping on bad templates.

### Multi-step branch trace (`has_data = true`)

1. `check` completes
2. `branch-a-1`: when true → ready → runs → completes
3. `branch-b`: when false → **skipped**
4. `branch-a-2`: dep completed → ready → runs → completes
5. `collect`: deps = completed + skipped, `continue_on_failure: true` → **promoted** → runs

### Changes

#### 1. `crates/stroem-common/src/models/workflow.rs` — Add `when` field

Add to `FlowStep`:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub when: Option<String>,
```

All `FlowStep` struct literals across the codebase need `when: None` (test helpers in `dag.rs`, `job_recovery.rs`, `scheduler.rs`, `workspace/mod.rs`, `web/api/jobs.rs`).

#### 2. `crates/stroem-common/src/template.rs` — Add `evaluate_condition()`

```rust
pub fn evaluate_condition(template: &str, context: &serde_json::Value) -> Result<bool> {
    let rendered = render_template(template, context)?;
    let trimmed = rendered.trim();
    Ok(!trimmed.is_empty() && trimmed != "false" && trimmed != "0")
}
```

#### 3. `crates/stroem-common/src/validation.rs` — Validate `when` expressions

In the flow-step validation loop: if `when` is `Some`, try parsing as a Tera template (catch syntax errors at YAML parse time, same pattern as cron validation).

#### 4. `crates/stroem-db/migrations/008_when_conditions.sql`

```sql
ALTER TABLE job_step ADD COLUMN when_condition TEXT;
```

Stores the raw `when` expression for display in UI/API. Not used for orchestration logic (that uses the flow definition).

#### 5. `crates/stroem-db/src/repos/job_step.rs` — Core changes

**Structs**: Add `when_condition: Option<String>` to `JobStepRow` and `NewJobStep`. Update all SELECT queries and the INSERT in `create_steps()`.

**`promote_ready_steps()`** — Signature adds `job_input` and `workspace_config` params (for building Tera context). Two changes to the dep-met check:

```rust
let deps_met = flow_step.depends_on.iter().all(|dep| {
    status_map.get(dep).map(|status| {
        if flow_step.continue_on_failure {
            // CHANGED: also accept "skipped" for convergence
            status == "completed" || status == "failed" || status == "skipped"
        } else {
            status == "completed"
        }
    }).unwrap_or(false)
});
```

After `deps_met`, evaluate `when` condition before promoting:

```rust
if deps_met {
    if let Some(ref when_expr) = flow_step.when {
        let ctx = build_render_context(job_input, &steps, workspace_config);
        match evaluate_condition(when_expr, &ctx) {
            Ok(true)  => { /* promote to ready */ }
            Ok(false) => { mark_skipped(pool, job_id, &step.step_name).await?; }
            Err(e)    => { mark_failed(pool, job_id, &step.step_name, &e.to_string()).await?; }
        }
    } else {
        /* promote to ready (existing path) */
    }
}
```

**`skip_unreachable_steps()`** — No changes needed. Current cascade logic already works: skipped deps (any reason) block non-`continue_on_failure` dependents.

**`mark_skipped()`** — No signature change needed.

#### 6. `crates/stroem-server/src/orchestrator.rs`

`on_step_completed()` signature adds `job_input` and `workspace_config`. Pass to `promote_ready_steps()`. After promotion, if any steps were conditionally skipped, loop promote + skip-unreachable again (a conditional skip may cascade, unblocking other steps).

```rust
loop {
    let promoted = promote_ready_steps(pool, job_id, &task.flow, job_input, ws_config).await?;
    let skipped = skip_unreachable_steps(pool, job_id, &task.flow).await?;
    if promoted.is_empty() && skipped.is_empty() { break; }
}
```

#### 7. `crates/stroem-server/src/job_creator.rs`

**Step creation**: Add `when_condition: flow_step.when.clone()` to `NewJobStep`.

**Root steps with `when`** (no dependencies): Evaluate condition at creation time using job input + secrets context. If false → create as `skipped`. If true → `ready`. If error → `failed`.

**Post-creation orchestration**: If any root steps were conditionally skipped, run a promote/skip loop to cascade effects, then `handle_task_steps()`.

**`build_step_render_context()`**: Also include skipped steps with `{ "output": null }` so downstream `when` expressions referencing them get a falsy value instead of a Tera undefined-variable error.

#### 8. `crates/stroem-server/src/job_recovery.rs`

Pass `job.input` and `workspace_config` through to `on_step_completed()`. Add `when: None` to `build_minimal_task_def()` FlowStep literal.

#### 9. API + UI

**API** (`web/api/jobs.rs`): Add `when_condition` to step JSON response.

**Types** (`ui/src/lib/types.ts`): Add `when_condition: string | null` to `JobStep`, `when?: string` to `FlowStep`.

**Step timeline** (`ui/src/components/step-timeline.tsx`): Show "Skipped (condition)" when `when_condition` is set and status is `skipped`.

**Task detail** (`ui/src/pages/task-detail.tsx`): Show `when` badge on flow steps with conditions.

#### Implementation order

1. Migration + model (`FlowStep.when`, DB column, struct updates)
2. `evaluate_condition()` in template.rs + validation
3. `promote_ready_steps()` change (accept skipped for c_o_f + when evaluation)
4. `orchestrator.rs` + `job_creator.rs` wiring
5. `job_recovery.rs` context passing
6. API + UI
7. Tests + docs

#### Tests

- **Unit**: `evaluate_condition()` truthiness (`"true"`, `"false"`, `"0"`, `""`, `"1"`, errors)
- **Unit**: validation rejects invalid `when` Tera syntax
- **Integration**: branching — step with `when: false` skipped, downstream cascade-skipped, convergence step with `continue_on_failure` runs
- **Integration**: multi-step branch — only the first step has `when`, rest cascade
- **Integration**: root step with `when` referencing input — evaluated at creation time
- **Integration**: bad template → step fails with error message
- **Integration**: recovery sweeper handles condition-skipped steps correctly

---

## 5b. For-each loops (fan-out/fan-in) — Design Outline

Iterate over a list, running a step (or sub-DAG) once per item in parallel.

### YAML syntax

```yaml
flow:
  fetch-items:
    action: get-items

  process:
    action: process-item
    depends_on: [fetch-items]
    for_each: "{{ fetch_items.output.items }}"
    input:
      item: "{{ item }}"
      index: "{{ index }}"

  aggregate:
    action: combine-results
    depends_on: [process]
```

### Architecture sketch

- `for_each: Option<String>` on `FlowStep` — Tera expression that evaluates to a JSON array
- At promotion time: evaluate expression, create N step instances (`process[0]`, `process[1]`, ...)
- Each instance gets `item` and `index` in its template context
- DB: dynamic step creation (INSERT new rows at orchestration time, not at job creation)
- `aggregate` step promoted when ALL instances of `process` are terminal
- Needs max-parallelism limit (e.g., `max_parallel: 5`) to avoid overwhelming workers
- Step naming: `{step_name}[{index}]` convention in DB
- Output: array of individual step outputs, available as `{{ process.output }}` (array)

### Key challenges

- Dynamic step creation after job start (current model creates all steps upfront)
- DAG validation at parse time can't know array length
- Fan-in: `depends_on: [process]` must resolve to "all instances of process"
- UI: show iteration progress (5/20 items processed)

---

## 5c. While loops — Design Outline

Repeat a step until a condition becomes false (or true), with iteration limits.

### YAML syntax

```yaml
flow:
  poll:
    action: check-status
    while: "{{ poll.output.status == 'pending' }}"
    max_iterations: 100
    delay_seconds: 30

  handle-result:
    action: process-result
    depends_on: [poll]
```

### Architecture sketch

- `while: Option<String>` on `FlowStep` — condition evaluated after each execution
- `max_iterations: Option<u32>` — safety limit (default 100)
- `delay_seconds: Option<u32>` — pause between iterations
- On step completion: evaluate `while` condition. If true → re-queue (reset to `ready` with incremented iteration counter). If false → finalize as `completed`.
- DB: `iteration: i32` column on `job_step`, output accumulates or overwrites per iteration
- Downstream steps see the final iteration's output
- Delay implemented via scheduled re-promotion (tokio sleep in orchestrator, or a "scheduled_at" timestamp on the step)

### Key challenges

- Re-running a step (status `completed` → `ready`) is a new transition
- Step output: last iteration wins, or accumulate all?
- Delay mechanism: in-memory timer vs DB scheduled_at column
- Interaction with recovery: don't re-run if server restarts mid-delay

---

## 5d. Suspend/Resume + Approval Gates — Design Outline

Pause workflow execution and wait for an external signal (human approval, webhook callback, or API call).

### YAML syntax

```yaml
flow:
  deploy-staging:
    action: deploy
    input:
      env: staging

  approve:
    action: _approval
    depends_on: [deploy-staging]
    input:
      message: "Staging deploy complete. Approve production deploy?"
      notify:
        - email: "ops@company.com"
        - slack: "#deployments"
      timeout_hours: 24

  deploy-prod:
    action: deploy
    depends_on: [approve]
    input:
      env: production
```

### Architecture sketch

- New built-in action type: `_approval` (server-side, like `type: task`)
- When step becomes ready, server creates an **approval token** (signed UUID) and sends notifications
- Step stays in a new status: `"suspended"` (waiting for external input)
- Approval link: `GET /hooks/approve/{token}` — validates token, resumes step
- On approval: step marked `completed` with `output: { approved_by, approved_at }`
- On rejection/timeout: step marked `failed`
- DB: new status `"suspended"` in CHECK constraint, `approval_token` column or separate table
- API: `POST /api/jobs/{id}/steps/{step}/approve` for programmatic approval
- UI: show approval button on suspended steps, link in notifications

### Notification channels

- Email via configured SMTP (or webhook to external service)
- Slack via webhook URL
- Generic webhook (POST to configured URL with job/step context)
- Configurable per approval step via `notify` input

### Key challenges

- Token security: signed, single-use, expiring
- Notification delivery: pluggable notification system
- Timeout: background sweeper marks expired approvals as failed
- Multiple approvers: require N of M approvals?
