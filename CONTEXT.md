# Strøm — Domain Vocabulary

Terms used in code, specs and CLAUDE.md. CLAUDE.md holds how things work; this file
holds what the words mean. Keep both in sync when a term is added or sharpened.

- **Job** — one execution of a task; owns a set of step rows.
- **Step** — one row of a job; moves `pending → ready → claimed → running → {completed, failed, skipped, cancelled}` (or `suspended` for approval gates).
- **Step cascade** — the fixpoint that moves a job's non-running steps after a change: rollup and sequential advance of loops, promotion, cascade-skip, skip-unreachable, placeholder retirement and expansion. Implemented once in `crates/stroem-server/src/cascade.rs` (`run` pure, `apply` in one transaction, `execute` the entry point).
- **Placeholder** — the `for_each` step row that stands in for its instances; `for_each_expr` is set.
- **Instance** — a row created by expanding a placeholder: `name[i]`, `loop_source = name`, `loop_index = i`.
- **Retirement** — a placeholder leaving `pending` without expanding (skipped or failed).
- **Adoption** — a `pending` placeholder whose instances already exist being moved to `running` without re-expanding.
- **Rollup** — a `running` placeholder becoming `completed` (array of instance outputs) or `failed` once every instance is terminal.
- **Plan** — the ordered list of changes one cascade `run` produced.
- **Change** — one state transition the cascade wants applied (`Promote`, `Skip`, `Fail`, `Expand`, `Adopt`, `Rollup`).
- **Guard miss** — an apply statement that affected fewer rows than the plan named; the plan is rolled back and re-run.
- **Settlement** — deciding a job's terminal status from its steps once every step is terminal: any untolerated `failed` → `failed`; else any `cancelled` → `cancelled`; else `completed` with the aggregated output of the flow steps nothing depends on. Implemented in `crates/stroem-server/src/settlement/` (`settle::decide` pure, `settle_if_all_terminal` the write).
- **Terminal handling** — the once-only side effects after a job row is terminal: propagation to the parent step, task-level retry or hooks, completion notification, log close and archive.
- **Claim** — the exactly-once gate on terminal handling: the `metrics_recorded_at` compare-and-set. Fail-closed.
- **Drain gate** — no terminal handling while a step of the job is `running` or `claimed`. Fail-open.
- **Reconcile** — finding descendants that settled at creation under a parent step that is still `running`, and advancing them. Bounded by task depth.
- **Advance** — the settlement module's one body: move a job as far as its rows allow, from cascade through terminal handling.
- **Action owner (O)** — the workspace that owns a `type: task` action: the caller's own workspace for a local action, or `step.action_workspace` for a cross-workspace action reference (`owner.action`). A `type: task` action's own `input` defaults and any secrets they reference are read from `O`.
- **Task owner (T)** — the workspace whose `tasks` config actually holds the task named by a `type: task` action's `task:` field, resolved relative to `O` (a hit in `O`'s own `tasks` first, else `workspace.task` split on the first `.`). The child job runs as `T`'s job: `T`'s files, secrets, connections, current revision, ACL and hooks. `A` (caller), `O` and `T` can be three different workspaces.
- **Provenance bucket** — which of the two upstream sources produced a value handed to a `type: task` child's connection-typed input: the caller's rendered flow-step `input:` (bucket `C`, from `A`), or the action's persisted defaults (bucket `D`, from `O`). Each bucket resolves connection names against the task's own schema, gated by its own workspace of origin — `C` falls back to `T` only if `A ≠ T`, `D` only if `O ≠ T` — so a caller-local and an owner-foreign value can appear side by side in one child's resolved input.
- **Tail read** — the newest whole lines of a log, at most `tail_bytes` (256 KiB by default), with `truncated` (exact when `false`) and `total_bytes` (an upper bound). What every log reader gets by default.
- **Full read** — the whole log as an NDJSON stream (`?full=true`); for a finished job the in-memory union of local and archive while it fits `merge_max_bytes`/`merge_max_lines`, else one source.
- **Log source** — which source answered a read: `local`, `archive`, `merged` or `none` (`X-Stroem-Log-Source`).

See `CLAUDE.md` for how these pieces fit together (architecture, conventions, key patterns).
