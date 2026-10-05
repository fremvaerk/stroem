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
- **Settlement** — deciding a job's terminal status from its steps once every step is terminal: any uncaught `failed` row (see *Caught failure*) → `failed`; else any `cancelled` → `cancelled`; else `completed` with the aggregated output of the flow steps nothing depends on. Skipped rows never decide job status, whatever their reason. Implemented in `crates/stroem-server/src/settlement/` (`settle::decide` pure, `settle_if_all_terminal` the write).
- **Dependency gate** — the rule deciding whether a step may run, from each dependency's final state and that dependency's own `continue_on_failure` / `continue_when_skipped` flags; a step's own flags never make it run. Strict AND: a step runs only when every dependency's verdict is Pass (completed, or not completed but carrying the matching flag on itself) — there is no automatic convergence for a choice-skipped dependency. Implemented in `stroem_common::gate` (`verdict`, `gate`), shared by the server cascade and `stroem run`. Spec `docs/superpowers/specs/2026-09-26-dependency-gate-design.md`.
- **Caught failure** — a failure at step `f` where every path from `f` to the end of the flow passes through a step with `continue_on_failure` (`f` included); such a failure does not fail the job, and is reported as `tolerated` in `hook.failed_steps`. Computed structurally over the current flow by `stroem_common::gate::caught_steps`, independent of which rows actually ran.
- **Terminal handling** — the once-only side effects after a job row is terminal: propagation to the parent step, task-level retry or hooks, completion notification, log close and archive.
- **Claim** — the exactly-once gate on terminal handling: the `metrics_recorded_at` compare-and-set. Fail-closed.
- **Drain gate** — no terminal handling while a step of the job is `running` or `claimed`. Fail-open.
- **Reconcile** — finding descendants that settled at creation under a parent step that is still `running`, and advancing them. Bounded by task depth.
- **Advance** — the settlement module's one body: move a job as far as its rows allow, from cascade through terminal handling.
- **Action owner (O)** — the workspace that owns a `type: task` action: the caller's own workspace for a local action, or `step.action_workspace` for a cross-workspace action reference (`owner.action`). A `type: task` action's own `input` defaults and any secrets they reference are read from `O`.
- **Task owner (T)** — the workspace whose `tasks` config actually holds the task named by a `type: task` action's `task:` field, resolved relative to `O` (a hit in `O`'s own `tasks` first, else `workspace.task` split on the first `.`). The child job runs as `T`'s job: `T`'s files, secrets, connections, current revision, ACL and hooks. `A` (caller), `O` and `T` can be three different workspaces.
- **Provenance bucket** — which of the two upstream sources produced a value handed to a `type: task` child's connection-typed input: the caller's rendered flow-step `input:` (bucket `C`, from `A`), or the action's persisted defaults (bucket `D`, from `O`). Each bucket resolves connection names against the task's own schema, gated by its own workspace of origin — `C` falls back to `T` only if `A ≠ T`, `D` only if `O ≠ T` — so a caller-local and an owner-foreign value can appear side by side in one child's resolved input.
- **Ref** — the `ref:` written next to an action or task name (flow-step `action`, `type: task` `task`, scheduler/webhook trigger `task`): a branch, a tag or a full commit SHA of the name's owner workspace, which must be a git workspace. Never templated. Spec `docs/superpowers/specs/2026-10-02-git-refs-design.md`.
- **Pin** — a resolved `(workspace, ref, commit)`: the ref as written plus the 40-hex commit it resolved to when the job containing the reference was created. Stored on a job (`git_ref` + `revision`) or a step (`action_ref` + `action_workspace` + `action_revision`; `task_workspace` + `task_ref` + `task_revision`). In code: `PinRef { git_ref, commit }`, with the workspace held by the row.
- **Pinned job** — a job with `git_ref IS NOT NULL`: created in owner@ref (a ref'd `type: task` child, an inherited-pin child, a ref'd trigger's job, or a hook / task retry / re-run / restart / agent task-tool child of a pinned job). Every definition it reads comes from its one commit; its ACL folder is `job.task_folder`; its state is partitioned by the ref string; task stats leave it out.
- **Pin inheritance** — an unqualified name without `ref:` written inside a pinned config resolves at that same commit; a qualified `ws.name` without `ref:` stays live. Decided at creation and stamped on the step; dispatch never infers it from the parent job.
- **PinStore** — the replica-local store of pinned commits (`workspace/pins.rs`): a bare repo per git workspace, immutable per-commit checkouts and configs, lease-aware eviction. Separate from the default-branch watcher; one process per `pin_store.dir`.
- **Pin release** — handing a claimed step back to `ready` because its pin could not be loaded on the claiming replica in time (a transient pin error); bounded by `MAX_PIN_RELEASES` (30), never a failure or a retry attempt.
- **Redaction closure** — the jobs whose pins a job's redaction set must include: the whole job tree of every job in its source lineage (hook source, restart source, task-retry original). Bounded; a closure cut by a bound fails closed (everything masked).
- **Tail read** — the newest whole lines of a log, at most `tail_bytes` (256 KiB by default), with `truncated` (exact when `false`) and `total_bytes` (an upper bound). What every log reader gets by default.
- **Full read** — the whole log as an NDJSON stream (`?full=true`); for a finished job the in-memory union of local and archive while it fits `merge_max_bytes`/`merge_max_lines`, else one source.
- **Log source** — which source answered a read: `local`, `archive`, `merged` or `none` (`X-Stroem-Log-Source`).

See `CLAUDE.md` for how these pieces fit together (architecture, conventions, key patterns).
