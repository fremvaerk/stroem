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

See `CLAUDE.md` for how these pieces fit together (architecture, conventions, key patterns).
