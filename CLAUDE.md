# Strøm v2 -- Development Guide

## Project Overview

Strøm is a workflow/task orchestration platform. Backend in Rust, frontend in React.
Domain vocabulary lives in `CONTEXT.md`; read it alongside this file.
Phase 1 (MVP) complete: end-to-end workflow execution via API and CLI.
Phase 2a complete: JWT authentication backend + WebSocket log streaming.
Phase 2b complete: React UI with shadcn/ui, embedded in Rust binary via rust-embed.
Phase 3 complete: Multi-workspace support, tarball distribution, Docker and Kubernetes runners, libraries.
Phase 4 complete: Advanced features (pod actions, secrets, connections, DAG visualization, ACL/RBAC).
Phase 5a complete: Conditional flow steps (`when` expressions).
Phase 5b complete: For-each loops (`for_each` + `sequential`).
Phase 5c: While loops.
Phase 5d complete: Approval gates (`type: approval`, `suspended` status, approve/reject API).
Phase 5e complete: Event source triggers (long-running queue consumers via stdout JSON-line protocol).
Phase 5f complete: Retry mechanism (step/action in-place retry + task-level job retry).
Phase 6a complete: Task state snapshots (immutable state persistence across runs via `STATE:` protocol + `/state` mount).
Phase 6a.2 complete: Global workspace state (`GLOBAL_STATE:` protocol + `/global-state` mount, shared across all tasks).
Phase 6c complete: Re-run prefill (`source_job_id` lineage + `raw_input` persistence; UI Re-run button replays connections + non-default secrets).
Phase 6b: Worker affinity.
Phase 7: AI agent actions & MCP integration.

## Architecture

- **stroem-common**: Shared types, models, DAG walker, Tera templating, validation
- **stroem-db**: PostgreSQL layer via sqlx (runtime queries), migrations, repositories
- **stroem-runner**: Execution backends (ShellRunner, DockerRunner via bollard, KubeRunner via kube). ShellRunner handles multi-language scripts (shell, Python, JS/TS, Go). All runners enabled by default.
- **stroem-agent**: Shared LLM dispatch logic (rig-core, MCP client), used by workers. Config types shared with server.
- **stroem-server**: Axum API server, orchestrator, multi-workspace manager (folder + git sources), log storage, embedded UI via rust-embed
- **stroem-worker**: Worker process: polls server, downloads workspace tarballs, executes steps, streams logs, handles agent step dispatch
- **stroem-cli**: Two CLI binaries from one crate:
  - `stroem` — Local workspace tool: `run`, `validate`, `tasks`, `actions`, `triggers`, `inspect`. No server needed.
  - `stroem-api` — Remote server client: `trigger`, `status`, `logs`, `tasks`, `jobs`, `cancel`, `workspaces`.

## Conventions

- **Error handling**: `anyhow::Result` everywhere. Use `.context("msg")` for error chain.
- **Async runtime**: tokio
- **Logging**: `tracing` crate. Use `#[tracing::instrument]` on public functions.
- **Secrets in logs**: wrap any field that can carry a rendered secret (action specs with `env`, resolved connection inputs, agent prompts/state, MCP server defs, step output, log lines) in `stroem_common::secret::Secret<T>` (re-export of the `redact` crate). `Debug` prints `[REDACTED T]`, reads go through `.expose_secret()`, `Serialize` needs `#[serde(serialize_with = "stroem_common::secret::serialize_opt_secret")]` so the wire format is unchanged. Never put a whole request/step struct into an `#[instrument]` span — use `skip(state, req)` / `skip_all` + explicit id fields. Regression tests: `test_claimed_step_debug_redacts_sensitive_fields` (worker), `test_worker_api_structs_debug_redacts_sensitive_fields` (server). `Secret<T>` only guards `Debug`; a secret **interpolated into a string** needs value scrubbing instead — `workspace_set::redact_secrets_in_str` (shared with `redact_json`) does **span-union masking**: it finds every occurrence of every secret value in the original text (overlapping occurrences included), merges intersecting or adjacent spans, and replaces each merged span with `••••••` — safe against one secret being a prefix/substring of another, or two secrets' occurrences crossing, which naive sequential replacement is not. Tera quotes the offending value in filter/type errors, so every claim-time render failure goes through `fail_claimed_step`, which scrubs before the message is logged to the job, persisted to `job_step.error_message`/`retry_history`, or returned in the 422 body. Any NEW path that persists or returns a rendered-template error must scrub the same way. Scrubbing guards SAME-workspace and server-log text always, and cross-workspace `type: task` text that carries no owner value (the caller's own input, or a structural error like a missing task). An owner-side render error — one that actually renders the OWNER workspace's own config — is withheld entirely (a fixed, value-free message) instead, once the step crosses that ownership boundary, because a filter chain can wrap a value in unboundedly many representations no finite scrub matches. Origin, not just which workspace is involved, decides withholding — see § Cross-Workspace References, `settlement/dispatch.rs::handle_task_steps_pass`.
- **YAML parsing**: `serde_yaml` (direct parsing in tests/models), `config` crate (loading with env var overrides)
- **Database**: sqlx with runtime queries (`sqlx::query()` / `sqlx::query_as()`), NOT compile-time macros.
- **Tests**: Unit tests in-module (`#[cfg(test)] mod tests`). Integration tests in `tests/` dirs using `testcontainers` for Postgres.
- **Frontend**: React 19 + TypeScript + Vite + Tailwind v4 + shadcn/ui in `ui/` directory. Package manager: `bun`.
- **Static serving**: UI built to `crates/stroem-server/static/`, embedded via `rust-embed` with SPA fallback.

## Development Rules

### Mandatory Test Coverage
Every new feature or functionality **must** be accompanied by tests:
- **Unit tests**: Cover the happy path, edge cases, and error conditions. Place in-module under `#[cfg(test)] mod tests`.
- **Edge cases**: Think about empty inputs, missing fields, invalid data, boundary conditions, and concurrent access.
- **Integration tests**: When the feature touches the database or cross-crate boundaries, add integration tests.
- **E2E tests**: When the feature affects the workflow execution pipeline (server ↔ worker ↔ runner), update `tests/e2e.sh` to verify it end-to-end.
- **Regression tests**: When fixing a bug, add a test that would have caught it.

### Mandatory Documentation Updates
Every new feature or significant change **must** include documentation updates:
- Update this `CLAUDE.md` if architecture, conventions, or key patterns change.
- Update `docs/internal/stroem-v2-plan.md` if the plan status changes.
- Add/update user-facing docs in `docs/src/content/docs/` (Starlight site), README, or CLI help text for anything users interact with.
- Keep code comments minimal — only where logic isn't self-evident.

### TODO Tracking
Maintain `docs/internal/TODO.md` as the consolidated task tracker:
- When discovering a new issue, improvement, or missing feature during work, add it to the appropriate section in `TODO.md`.
- When completing a task that has a corresponding entry, mark it `[x]` in `TODO.md`.
- Keep sections organized: Security, Architecture, Code Quality, Performance, Frontend, Test Coverage, Roadmap, Bugs.

### Work Execution
- **Use subagents** as much as possible — delegate research, code review, exploration, and specialized tasks to appropriate Agent types (Explore, code-reviewer, rust-engineer, typescript-pro, etc.).
- **Use agent teams** for complex multi-step tasks that benefit from parallel work (e.g., full-stack features, large refactors, multi-file changes with independent subtasks).

### Tera Templating
- Step names with hyphens (e.g., `say-hello`) are sanitized to underscores (`say_hello`) in the template context because Tera interprets hyphens as subtraction.
- Workflow YAML must use underscored names in template references: `{{ say_hello.output.greeting }}`, not `{{ say-hello.output.greeting }}`.
- **Job metadata**: `{{ job.revision }}` (workspace revision pinned at job creation) is available in all template contexts — step inputs, `when` conditions, action bodies (script/cmd/env/args/image/manifest), agent prompts, approval messages. Always inserted (null → renders as `""` for pre-migration jobs). Hooks get the same value as `hook.revision` (both `HookContext` and `SuspendedHookContext`).
- **Single owner**: `crates/stroem-server/src/render_context.rs::build` builds the context for EVERY template field (S1 step input, S2/S3 action body + image, S4 agent prompts, S5 when/for_each, S6 task-step input + approval message). `Scope` is the only thing it matches on (which `input`, whose `secret`); everything else — insertion order `input, secret, state, global_state, job, steps, each`, four statuses with masked output, loop-instance rows skipped — is one rule. `RenderContext` is opaque: never patch a variable into `as_value()`'s map after the fact; add it to `build`. Reserved step names are `FRAMEWORK_KEYS`; collisions are recorded on the context and logged. Snapshots for `state`/`global_state` are resolved once per entry by `render_context::latest_snapshots` (pool-only, migration 047's `state_json` column) in `Settlement::advance`, `dispatch::init` and `claim_job`, and threaded down as `&Snapshots` — `cascade::run` stays pure. Spec: `docs/superpowers/specs/2026-09-11-render-context-owner-design.md`.

## Build & Test

```bash
# Build everything (all features enabled by default: docker, kubernetes, s3)
cargo build --workspace

# Run all tests (needs Docker for integration tests)
cargo test --workspace

# Run specific crate tests
cargo test -p stroem-common
cargo test -p stroem-db
cargo test -p stroem-runner
cargo test -p stroem-server

# Check formatting
cargo fmt --check --all

# Lint
cargo clippy --workspace -- -D warnings

# E2E tests (needs Docker)
./tests/e2e.sh
```

### Frontend (ui/)

```bash
cd ui && bun install
bun run dev          # Dev server (proxy to backend on :8080)
bun run build        # Build (outputs to crates/stroem-server/static/)
bunx playwright test # Playwright E2E (needs backend running)

# Playwright E2E in Docker
docker compose -f docker-compose.yml -f docker-compose.test.yml \
  up --build --abort-on-container-exit playwright
```

### Documentation (docs/)

```bash
cd docs && bun install
bun run dev            # Dev server
bun run build          # Build static site (also regenerates llms.txt)
bun run generate-llms  # Regenerate llms.txt only
bun run preview        # Preview built site
```

### LLM Reference (llms.txt)

- `docs/public/llms.txt` is auto-generated from doc sources by `docs/scripts/generate-llms-txt.ts`
- Contains workflow authoring reference; served at `/llms.txt` on the docs site
- To add/remove sections, edit the `sections` array in the generator script

## Key Patterns

### Workflow YAML structure
See `docs/internal/stroem-v2-plan.md` Section 2 for the full YAML format.

### Action Types and Runners
- **`agent`**: LLM call as a workflow step, worker-side dispatch. Supports structured output via `output` (converted to JSON Schema).
- **`docker` / `pod`** (container actions): Runs user's prepared image as-is, no workspace mounting. Uses `cmd` field for entrypoint/command override.
- **`script`**: `type: script` + `runner: local|docker|pod` — scripts with workspace files. Languages: `shell` (default), `python`, `javascript`, `typescript`, `go`. Uses `script` (inline) or `source` (file path) fields. Optional `dependencies`, `interpreter`, and `args` (CLI arguments, Tera-templated) fields.
- **`task`**: References another task, server creates a child job (see Task Actions below)
- **`approval`**: Pauses execution for human approve/reject (see Approval Gates below)
- `type: script` + `image` is **rejected** by validation. Use `type: docker` or `type: script` + `runner: docker` instead.
- **Toolchain preferences**: `uv > python3 > python`, `bun > node` (JS), `bun > deno` (TS), `bash > sh`
- **Pod manifest overrides**: `type: pod` and `runner: pod` support a `manifest` field deep-merged into the generated pod spec. See `docs/src/content/docs/guides/action-types.md`.

### Runner Architecture
- `RunnerMode` enum: `WithWorkspace` (script actions) or `NoWorkspace` (docker/pod actions)
- `StepExecutor::select_runner()` dispatches on `(action_type, runner_field)`:
  - `("script", "local")` → ShellRunner, `("script", "docker")` or `("docker", _)` → DockerRunner, `("script", "pod")` or `("pod", _)` → KubeRunner
- **DockerRunner**: `WithWorkspace` bind-mounts at `/workspace:ro`; `NoWorkspace` runs standalone
- **KubeRunner**: `WithWorkspace` uses init container + workspace volume; `NoWorkspace` runs directly
- **Startup scripts**: `docker/entrypoint.sh` sources `*.sh` from `/etc/stroem/startup.d/`. DockerRunner bind-mounts this; KubeRunner uses ConfigMap via `runner_startup_configmap`.

### Capabilities, Tags, and Step Claiming
Post-042 (`042_worker_exclusive.sql`): two routing axes with affinity semantics on `tags`, plus an optional exclusive flag for reserved workers.
- **`capabilities`** on worker — runners supported. Any of `"script"`, `"docker"`, `"kubernetes"`, `"agent"`. Required. Matched against step's `required_ability` (derived from action type + runner).
- **`tags`** on worker — free-form **affinity labels**. A step's `required_tags` MUST be a subset of these for the worker to claim it. Empty = no affinity axis.
- **`exclusive`** on worker — bool (default `false`). When `true`, the worker also refuses any step whose `required_tags` don't cover its own `tags` — the "reserved worker" pattern.
- Claim SQL: `worker.capabilities @> to_jsonb(required_ability) AND required_tags::jsonb <@ worker.tags AND (NOT worker.exclusive OR worker.tags <@ required_tags::jsonb)`, plus `action_type NOT IN ('task','approval','event_source')` — the server-dispatched kinds. **`agent` must not be in that list** (worker-side since a8aa1c6; ed67c76 regressed it until 044). The unmatched-step sweep and the partial index `idx_job_step_ready_claim` (migration `044_claim_index_agent.sql`) must carry the identical predicate or the planner drops the index.
- User pattern "reserve worker X for step Y": worker `tags: ["some-label"], exclusive: true` + action `tags: ["some-label"]`. Untagged steps never leak onto worker X.
- Behaviour flip from 041: an untagged step (`required_tags: []`) NO LONGER reaches a tagged non-exclusive worker's *tagged* peers — a step needs `required_tags: ["claude"]` to be routed to a `tags: ["claude"]` worker. Under 041 a permissive (empty-tag) worker could still snap up any step; that leak is gone.
- Recovery's unmatched-step sweep mirrors the claim SQL (`get_unmatched_ready_steps`).

### Multi-Workspace
- Server config: `workspaces:` map with named entries (folder or git source)
- **Per-server trigger suppression**: `workspaces.<name>.triggers: false` (both `folder` and `git`, default `true`, env `STROEM__WORKSPACES__<NAME>__TRIGGERS=false`) loads the workspace but never fires its triggers on that server. Single source of truth is `WorkspaceManager::triggers_enabled(name)` (backed by a `triggers_disabled: HashSet` captured from the defs before source construction, so it also covers `load_errors` placeholders); the three trigger consumers — `scheduler::load_triggers`, `event_source::collect_desired`, `hooks::find_webhook_trigger` — each `continue` past a disabled workspace. Any NEW consumer that iterates `config.triggers` must add the same check. Triggers stay in the loaded config (UI/API still list them); `WorkspaceInfo.triggers_enabled` surfaces the flag. The `triggers` field uses `lenient_bool` because `WorkspaceSourceDef` is a `#[serde(tag)]` enum — serde's tagged-enum buffering bypasses the `config` crate's string→bool coercion, so env overrides arrive as the string `"false"` (the same limitation applies to `poll_interval_secs` via env; pre-existing, see TODO.md). Tests: `test_with_triggers_disabled_builder` + `*_skips_workspace_with_triggers_disabled` in each consumer; router-level `test_workspace_triggers_disabled_reported_and_webhook_hidden` (integration_test.rs, via `setup_multi_workspace_with(&["default"])`). `WorkspaceManager::with_triggers_disabled(name)` is a pub builder because integration tests build managers through `from_entries`, never `new`.
- `WorkspaceSource` trait with `FolderSource` and `GitSource` impls
- `GitSource` tests use local bare repos (`file://` URL) via `git2`. They are plain `#[test]`s: `GitSource` is synchronous (loads run on `spawn_blocking`, no `block_in_place`), so they need no runtime.
- **Error recovery**: Failed workspaces get placeholder entries; watchers retry on each poll cycle
- **Unavailable ≠ removed (scheduler only)**: a workspace whose reload failed serves NOTHING (`get_config`/`get_path`/`get_revision` → `None`, unchanged). Prod 2026-09-16: GitHub rejected the deploy key for ~60 s and all 27 cron triggers silently vanished from the scheduler. `scheduler::load_triggers` now carries the previous `TriggerState`s of a workspace whose config is unavailable over to the next map (nothing is fabricated for a workspace that was never loaded; `triggers: false` still wins; a trigger removed from a HEALTHY workspace still disappears). `fire_trigger` checks availability — and then that the CURRENT config still defines the trigger IDENTICALLY (the whole serialized `TriggerDef` — schedule, task, input, concurrency, `force_refresh`; a workspace can recover with the trigger removed between `load_triggers` and the fire) — BEFORE the concurrency policy — a fire that cannot create its job logs `Trigger '…' MISSED: workspace '…' is unavailable` and must not cancel the previous run (`cancel_previous`) or record a skipped row (`skip`); it is never replayed. The EVENT-SOURCE manager deliberately still cancels the consumer of an unavailable workspace: `/worker/event-source/emit` 404s while the workspace is down and the worker drops the payload, so a consumer kept alive would drain its queue into the void — cancelling pauses consumption, step 3 recreates it on recovery. Tests: `test_load_triggers_keeps_schedule_of_unavailable_workspace`, `test_load_triggers_drops_trigger_removed_from_healthy_workspace`, `test_trigger_fire_on_unavailable_workspace_has_no_side_effects` and `test_trigger_fire_revalidates_against_current_config` (integration, via `scheduler::fire_trigger_once`). Serving the last good CONFIG/FILES through an outage was tried and reverted after Codex review — see TODO.md "Last-good serving".
- **Workspace refresh (spec `docs/superpowers/specs/2026-09-17-workspace-scale-design.md` § 4).** `WorkspaceSource` is SYNCHRONOUS (`load(&LoadBudget) -> LoadOutcome`, `peek_revision(&LoadBudget) -> Peek`) and always runs on `spawn_blocking`; `block_in_place` is gone from `GitSource`. A `Peek::Failed` (network/auth/timeout) SKIPS the tick and keeps serving the last loaded config — a new policy, not last-good serving after a failed reload (still reverted); K=5 consecutive failures force one load. Per-entry state is split three ways: the execution mutex (`WorkspaceEntry::exec`, held by a load's finalizer until the worker REALLY returns; every acquirer uses `try_lock`), `Availability` (µs std mutex, written only by the pure `availability::transition`), and the published snapshot (`WorkspaceEntry::published()`, what every getter reads). `WorkspaceEntry::apply_load_result` is the ONLY writer of the snapshot and of load-completion state — every load path (watcher, `reload`, `reload_for_api`, and startup) goes through it. Startup builds a `WorkspaceEntry::pending` entry per workspace and applies its result with `Caller::Startup` and that workspace's OWN completion instant (spec § 4.5 (9)); a startup failure lands in `Errored` with `next_attempt = completed_at`. An external failure lands in `Errored` and the watcher retries it on a doubling backoff (cap `workspace_reload.max_backoff_secs`). `workspace_reload.{peek,load}_timeout_secs` and `max_backoff_secs` are capped at 86 400 (24 h) by `ServerConfig::validate`; `availability::instant_after` saturates the remaining `Instant + Duration` sums (git `poll_interval_secs` is uncapped). Loads run worker (`spawn_blocking`) → finalizer (detached; owns guard + permit; applies result; releases; THEN spawns the best-effort peer notification) → observer (may time out; never cancels). Watcher loads take one of `MAX_CONCURRENT_WORKSPACE_LOADS` permits via non-blocking admission; `Busy`/`Saturated` is never a transition. External callers get `ReloadBusy` (downcast it) instead of queueing; a busy `force_refresh` fires from a healthy snapshot (errored ⇒ still MISSED). Deadlines: `LoadBudget` (stroem-common) reaches `sops`/`vals` (killed at the deadline via `budget::run_with_deadline`), the YAML scan (expiry aborts the whole load, never a per-file warning), git `transfer_progress` and checkout-planning `notify`; libgit2 connect/read timeouts are process-wide (`workspace::git::configure_global_timeouts`, set in `main`). DNS, a slow-drip remote, one blocked read and the checkout write phase stay uninterruptible — that is what the watchdog (`stroem_workspace_load_overdue`, derived at scrape time) is for. Tuning: `workspace_reload:` server config. NOT covered (spec § 4.10): peer-reload listener stall, notification origin, startup saturation, signal-to-exit bound. `git2` is pinned at `0.21` with features `["ssh", "https"]` explicitly enabled — 0.21 made both opt-in (`default = []`); `git::tests::libgit2_is_built_with_ssh_and_https_transports` guards it. `budget::run_with_deadline` (stroem-common) runs `sops`/`vals` in their own process group and kills the whole group at the deadline, so helper grandchildren cannot keep pipes (and reader threads) alive. Folder hashing treats a dangling symlink (`NotFound` target) or a symlink loop as stable content (hashes the link path + target) instead of erroring; every other I/O error — including a permission error reached THROUGH a symlink — makes the peek `Failed` and fails the load. `run_with_deadline` enforces the deadline until the child has exited AND its pipe threads have finished (a descendant holding stdout open past the parent's exit is killed with the group; the threads are detached, never joined unbounded). A git clone dir that is not a usable repository (e.g. left empty by a deadline-aborted first clone) is removed and re-cloned; a failed clone removes its dir. A workspace that is `Errored` when its watcher starts (a startup failure) gets NO jitter — it retries on the first tick; healthy watchers start at a deterministic per-name offset in `[0, poll)`. Only EXTERNAL reloads (API refresh / `reload()`) stamp the API refresh cooldown (`reload_state.last_completed`); watcher loads do not.
- **Tarball retention keep-set** (`JobRepo::tarball_keep_revisions`): active jobs' revisions, cross-workspace `job_step.action_revision`s of active jobs, and failed top-level jobs still owed a task retry (1-hour window).
- **Runtime**: `main` builds tokio explicitly with `worker_threads = max(4, available_parallelism)` (`runtime::worker_threads`) — a sub-core CPU quota used to yield ONE worker.
- API routes workspace-scoped: `/api/workspaces/{ws}/tasks/{name}/execute`
- Worker: `WorkspaceCache` with immutable revision-based dirs, `WorkspaceGuard` (RAII ref-counted), ETag caching
- **Concurrent startup load**: `WorkspaceManager::new_with_reload` loads all workspaces concurrently — one `JoinSet` task per workspace, each running its `load` on `spawn_blocking` (no runtime worker thread is occupied by git or YAML work), bounded by the shared `MAX_CONCURRENT_WORKSPACE_LOADS` (8) semaphore that the watchers' load admission also uses. Each result is finalized through `apply_load_result(Caller::Startup, …)` (see the refresh bullet above). Logs `Loaded {n} workspace(s) in {elapsed}` after all joins. A panicked load task is caught and recorded as a load error for that workspace, not a crash.
- **Git credential fail-fast**: `GitSource::build_remote_callbacks`'s libgit2 credentials callback grants the configured SSH key/token only on the first request per URL (via the pure `credential_decision()` function in `git.rs`); a second request means the remote rejected it, so it errors immediately ("`<auth type> credential rejected by remote for <url>`") instead of letting libgit2 retry the same credential for over a minute. A username-only probe (`ssh://` URLs with no embedded user) is answered without consuming an attempt.

### Libraries (Actions, Tasks, Connection Types)
- Import shared actions, tasks, and connection types from Git repos or local folders
- Defined in `server-config.yaml` (`libraries:` + `git_auth:`), shared across all workspaces
- Namespace separator: `.` (dot) — e.g. `common.slack-notify`
- During import: actions, tasks, connection types are prefixed; triggers, secrets, connections are ignored
- Internal reference rewriting: action refs in flow steps, task refs, hook actions, connection-type input fields
- CLI `stroem validate` skips `.`-containing names with a warning; server validates fully after resolution

### Cross-Workspace References
- A flow step's `action:` may be `owner_ws.action` — resolved live against the OWNER workspace's config at job creation. No library registration needed (unlike Libraries above).
- **Precedence**: a dotted name resolves as a library item first (libraries are flattened into the local config at load time), then as a `workspace.item` cross-workspace reference on local miss (split on first `.`). Unqualified names stay local. Fully backward-compatible.
- **Owner-context execution**: the action runs with the OWNER workspace's tarball (files), secrets, and connections; the caller's flow-step `input:` map still renders in the caller's context (previous outputs, caller job input). Connection-typed inputs of a cross-workspace action resolve by bare name against the owner — the caller needs no local connection/type/secret.
- **Data model** (migration `043_job_step_action_workspace.sql`, additive/nullable): `job_step.action_workspace` (owner workspace, NULL ⇒ job's own workspace) + `job_step.action_revision` (owner's pinned revision, NULL ⇒ job's revision). Stamped at job creation from the resolved action; `action_spec`/`required_ability`/`required_tags`/`runner`/retry derive from the owner action definition.
- **Claim-time**: `claim_job` derives the owner ws/config, threads it through `RenderContext.action_workspace` so `prepare_step_action_input` resolves the action + its connections against the owner (bare-name lookup), and returns `ClaimResponse.workspace`/`revision` pointing at the owner — the worker (unchanged) fetches the owner's tarball at the pinned revision. A step downloads exactly one workspace (its owner), never two.
- **Actions**: open, no ACL. **Connections**: gated per connection by `shared: true` (`ConnectionDef.shared`); unshared foreign references are 400 at creation / failed step at claim.
- **Connections & types**: `ws.conn` and `type: ws.type` are independently addressable; matched by canonical `(workspace, type)` pair (`template::canonical_type_ref`). Resolver: `resolve_connection_inputs_scoped` + `ResolveScope`; server lookup `workspace_set::WorkspaceSet` (all loaded configs, `WorkspaceSet.known` backed by `WorkspaceManager::configured_names()` — the union of loaded/placeholder entries AND source-construction failures in `load_errors`, so a workspace whose source itself failed to construct still classifies as `Unavailable` rather than `Unknown`); CLI `SingleWorkspace`. Provenance-aware two-pass for cross-workspace actions: `prepare_action_input_cross` (caller-supplied names → caller, then owner-if-shared; owner defaults → ungated). Literal flow-step values pre-checked at creation (`job_creator::precheck_literal_connection_inputs`); templated ones fail the step at claim. A `when`-guarded flow step skips the literal pre-check entirely (the step may never run), regardless of whether its connection inputs are literal or templated — it always falls back to failing at claim time if reached.
- **Redaction**: job detail masks all workspaces' secrets + `secret: true` connection properties (`workspace_set::collect_redaction_values`).
- **Errors**: unresolvable action reference — unknown workspace, or a known workspace with no such action (`web/api/tasks.rs::classify_execute_error` matches `"has no action"`) → `BadRequest` (400), never 500 — same fix applied independently to missing/misnamed local connection references. Owner workspace being transiently unavailable (load/health condition, not a caller mistake) still surfaces as 500. The classifier is two-tier: precise cross-workspace phrases (`"is not shared"`, `"unknown workspace"`, `"has no connection"`) are matched anywhere in the FULL error context chain (`{:#}`, since a resolution error is nested several `.context()` layers deep); legacy broad phrases (`"not found"`, `"does not exist"`, `"resolve connection"`, `"has no action"`, `"required"`, `"invalid"`, `"validation"`) are matched on the outermost message only, to avoid misreading a coincidental substring in an inner infra-layer error (e.g. a Postgres error's own "does not exist") as a user mistake. `"is not available"` (configured-but-unloaded workspace) is checked first, anywhere in the chain, and always forces 500.
- **Cross-workspace `type: task`**: `job_creator::resolve_task_ref` resolves `action_spec.task` relative to the ACTION owner `O` (`step.action_workspace ?? job.workspace`): `O`'s `tasks` key first (library-flattened names land here), then `ws.task` on a miss — `T` is whichever workspace wins. The child is created in the TASK owner `T`'s workspace (`settlement/dispatch.rs::handle_task_steps_pass`) from one config snapshot taken when `T` is resolved, with `T`'s **current** revision when `T` is foreign (a same-workspace child still inherits the parent's revision, unlike a cross-workspace action's revision, which is pinned at parent creation). Provenance buckets: the caller's rendered step `input:` (`C`), the action's persisted `action_spec.input` defaults rendered with `O`'s live secrets (`D`), and `T`'s own task defaults (ungated, `T` reading itself) — connection-typed fields of the **task's** schema resolve once from `C`/`D` by provenance (`template::resolve_task_input_by_provenance`); a value crossing a workspace boundary into one of those fields must be a connection name, never an object/array/scalar (same-workspace object-trust is unchanged and still a known gap, TODO.md). The full merged input (`C` + `D` + task defaults) is always what the CHILD job gets, but `JobStepRepo::update_input` on the PARENT step only persists that merged value when `O == A`; when `O != A` it persists bucket `C` alone, so the owner's resolved defaults — which can include the owner's unshared connections — never leak onto a step visible to anyone with only View on the caller's task. `job_creator.rs::precheck_task_step_literals` pre-checks caller literals against the TASK schema (not the action's), skipping `when`-guarded steps; `web/api/tasks.rs::classify_execute_error` gained `"has no task"` beside `"has no action"`. `fail_task_step` scrubs and persists the full chain (`workspace_set::redact_secrets_in_str`, span-union masking, see § Secrets in logs) for the caller-side step-input render (bucket `C`, never withheld — it's the caller's own template in the caller's own context) and for a Caller-bucket `template::ProvenanceError` (a bad literal the caller itself supplied) or a structural `create_job_for_task_inner` error (task not found, DB error, missing required field) regardless of `O`/`T`. An owner-side render error is withheld instead — a fixed, value-free sentence naming the workspace being rendered, persisted to `job_step.error_message`, with the full scrubbed chain still going to the server log via `tracing::error!` — decided by ORIGIN, not phase: the action-defaults merge (`merge_action_defaults`, bucket `D`) is unconditionally owner-side when `O != A` (every value it renders is one of `O`'s own templates); connection resolution is withheld only for an `ActionDefault`-bucket `template::ProvenanceError` when `O != A`; child-job creation is withheld only when `job_creator::OwnerSideRender` tags the error (rendering `T`'s own task schema) and `T != A`. Both markers are found with `err.downcast_ref::<Marker>()` on the `anyhow::Error` directly (not a `.chain()` walk, which doesn't reliably find a `.context(...)` value). A filter chain (`{{ secret.X | json_encode | round }}`) can wrap a value in unboundedly many encodings, one more than any scrub added last time — matching representations does not converge, so the origin-typed boundary withholds instead. Hook actions still cannot resolve a `type: task` reference cross-workspace: today this is rejected only at runtime, by `settlement/hooks.rs::fire_single_hook`'s bail *before* any hook job is created — no hook job exists to fail, and the diagnostic is logged to the SOURCE job (the job whose completion fired the hook) — `stroem-common::validation::validate_workflow_config_with_cross_workspace_resolver` (`CrossWorkspaceResolver::has_task`) could reject it earlier, at workspace-load time, but is not wired into any server load/reload path yet (pre-existing gap, TODO.md). Cross-workspace `agent` steps still render prompt/system_prompt/MCP/task-tools against the CALLER's config, not the owner's — both deferred. `WorkspaceManager::mark_unavailable_for_test` / `replace_config_for_test` back the resolver's error-path tests (`replace_config_for_test` does not bump `get_revision`, TODO.md). The lifecycle gaps this feature inherits (double dispatch, stale-dispatcher failure, non-atomic terminal delivery, missed cancellation, timed-out-parent-doesn't-cancel-child, task-removed-mid-run stranding, silent suspended-child hooks) are carried, not fixed — see `docs/superpowers/specs/2026-09-16-task-step-lifecycle-hardening-design.md` and TODO.md. Workspace-config validation (`validate_workflow_config_with_libraries`) is still not wired into any server load/reload path for ANY action ref (pre-existing gap, not introduced by this feature) — job-creation-time resolution is the real safety net today.

### Scheduler (Cron Triggers)
- `scheduler.rs` — background task, smart sleep (wakes at next fire time), config hot-reload, `CancellationToken`
- `job_creator.rs` — shared job+step creation for API handler and scheduler
- `croner` crate for cron parsing (5/6-field with `with_seconds_optional()`)
- **Timezone**: Optional `timezone` field (IANA name), defaults to UTC. Uses `chrono-tz`.
- **Concurrency policy**: `Allow` (default) / `Skip` / `CancelPrevious` on scheduler triggers

### Timeouts
- **Step**: `FlowStep.timeout: Option<HumanDuration>` (max 24h) — **Task/job**: `TaskDef.timeout` (max 7d)
- `HumanDuration` parses `"30s"`, `"5m"`, `"1h30m"`, or plain integer (seconds)
- Server-side: recovery sweep Phase 2 (steps) + Phase 3 (jobs). Worker-side: `tokio::time::timeout`
- **Server-level defaults**: `default_step_timeout` / `default_job_timeout` on `ServerConfig`. Applied at job-creation time: when `flow_step.timeout` or `task.timeout` is absent, the default fills in. Visible via the API and enforced by the same recovery sweep. Same caps as explicit timeouts (24h/7d). `None` = no default (existing behaviour, unbounded). Resolved into a `JobDefaults` Copy struct threaded through `create_job_for_task` / `create_child_job_for_task` / `handle_task_steps`. To opt a single task out of the global default, set the task's `timeout` to the maximum (`86400s` / `7d`).

### Step Cascade
- `crates/stroem-server/src/cascade.rs` — one pure fixpoint replaces the old promote→skip→expand loops and the keyed loop rollup. `run(task, job, steps, workspace_config) -> Plan` never touches the DB; `apply(conn, job_id, plan)` composes stroem-db `_tx` primitives with affected-row-count checks; `execute(pool, ...)` reads, runs, applies in ONE transaction and re-runs on a guard miss (max 3).
- **Phase order per pass is part of the interface**: P0 rollup/advance (R5/R6) → context A → P1 cascade-skip + promote with `when` (R1/R2) → P2 skip-unreachable (R3) → context B → P3 adopt + retire/expand (R0/R4). A `when` in P1 does not see a skip from the same phase until the next pass; expansion in P3 does. Do not reorder.
- Callers: `settlement::cascade_and_settle` (worker completion, approval, recovery, propagation — everything that reaches `Settlement::advance`) and `settlement::dispatch::init` (creation-time). Both call `execute` then continue as before. `check_loop_completion`, `expand_for_each_steps`, `promote_ready_steps`, `skip_unreachable_steps` no longer exist.
- `execute` commits before returning; settlement, task/approval dispatch and propagation run after it, outside any transaction. Never wrap `execute` in a larger transaction.
- Guards: `Promote`/`Skip`/`Fail` require `pending`; `Expand`/`Adopt` require the placeholder `pending`; `Rollup` requires `running` (so a cancelled or timed-out placeholder is never overwritten). The R7 job-row `UPDATE` may match zero rows. A `Fail` for a placeholder is guarded where the old code was not.
- `Change::Skip { step, reason: SkipReason }` — the reason (`condition` | `empty` | `cascade` | `unreachable`) is applied to the in-memory snapshot row so later passes see it, and `apply` batches a run of skips per reason (one `skip_steps_tx` per bucket, each guarded). Spec `docs/superpowers/specs/2026-09-09-continue-when-skipped-design.md`.
- `run` renders templates (which may call the `vals` subprocess); that is why it runs before the apply transaction opens.
- Known, unchanged from before the cascade: two cascades on one job can race (lost work, `job_step.rs` TODO history); a same-status `output`/`error_message` rewrite between snapshot and apply is not detected; `try_retry_job`'s transaction (job row then steps) inverts the cascade's order (steps then job row). All three are closed by `docs/superpowers/specs/2026-09-08-cascade-concurrency-hardening-design.md`.
- New context variables in `build_step_render_context` must be inserted BEFORE completed-step outputs (a step named `job` shadows `job`).
- Deployment: the fail-or-retry change (CLAUDE.md § Retry Mechanism) must be running on every server replica before the release containing the cascade activation is rolled out.

### Settlement
Everything a job owes after one of its steps moves (or a job was created, or cancelled) — from the step cascade through the last terminal side effect. One module, `crates/stroem-server/src/settlement/`, replacing the three copies that used to live in `job_recovery.rs`, `orchestrator.rs` and `hooks.rs` (all deleted; `cancellation.rs` now keeps only the cancelled-jobs set). Spec: `docs/superpowers/specs/2026-09-08-job-settlement-design.md`. Glossary: `CONTEXT.md` (Settlement, Terminal handling, Claim, Drain gate, Reconcile, Advance).

- **Module layout** — a pool tier of free functions (no `AppState`, what the creator and the pool-only tests call) and a state tier, the `Settlement` struct (built on demand from `&AppState` via `AppState::settlement()`, cheap: `Arc` clones + a pool handle):
  | File | Owns |
  |---|---|
  | `mod.rs` | `Settlement` struct, `CreatedJob`, `BornTerminal`, `CancelResult`, the seven entries, `advance`, `server_log` |
  | `settle.rs` | `Settled`, `decide` (pure, unit-tested), `settle_if_all_terminal`, `cascade_and_settle` |
  | `dispatch.rs` | `handle_task_steps`, `handle_task_steps_pass`, `fail_task_step`, `handle_approval_steps`, `fire_initial_suspended_hooks`, `init` (the creator's post-commit block) |
  | `terminal.rs` | `claim`, `drained`, `TerminalPlan` + `plan` (pure, unit-tested), `run_terminal_actions`, `build_minimal_task_def`, log-upload helpers |
  | `propagate.rs` | `Settlement::propagate` (agent registration barrier + parent-step write + `advance(parent)`) |
  | `retry.rs` | `create_retry_job`, `compute_retry_delay`, the retry log-line helpers (`step_retry_message`, `step_retries_exhausted_message`, `task_retry_message`) |
  | `hooks.rs` | moved from `src/hooks.rs`; hook selection, `fire_hooks`/`fire_suspended_hooks`, and the hook-job creation path via `build_step` |

- **The seven entries** (`impl Settlement`) — nothing outside the module reaches a terminal side effect without the claim:
  | Entry | Replaces | Typical callers |
  |---|---|---|
  | `step_settled` | old `orchestrate_after_step` | worker `complete_step` success, approval approve, recovery phases that don't fail a step |
  | `step_failed` | old `fail_step` + `orchestrate_after_step` | worker `complete_step` failure, claim-time render failure, four recovery phases — **six** production call sites; the approval REJECT handler is the seventh, calling `JobStepRepo::fail_or_retry` inline (so its `[approval] … rejected` log line precedes any `[retry]` line and the 409-conflict branch survives) and then `step_settled` directly |
  | `job_created` | old `finalize_created_job` | every job-creation entry point (HTTP execute/re-run/restart, hooks, webhooks, MCP, scheduler, event source) and `advance` itself for a newly created retry job |
  | `agent_child_created` | old `reconcile_settled_children` + inline check | `web/worker_api/jobs.rs::agent_task_tool` |
  | `agent_children_registered` | old inline loop over `propagate_to_parent` | `agent_save_state`, `agent_suspend_step` |
  | `cancel` | old `cancellation::cancel_job` | `web/api/jobs.rs`, `mcp/tools.rs`, `recovery.rs`, `scheduler.rs`, `event_source.rs` |
  | `worker_completed_job` | old `mark_completed` + `handle_job_terminal` | `web/worker_api/jobs.rs::complete_job` (local-mode) |

- **`advance`** — the one body (`pub`, so tests can drive a job from an arbitrary row state; production code goes through the entries above) — moves a job as far as its rows allow, once per call (not a loop; the next step completion, approval or recovery tick re-enters):
  1. If the job row is non-terminal: `cascade_and_settle` → dispatch newly-promoted `type: task` steps → `reconcile` (descendants that settled at creation under a still-`running` parent step) → dispatch newly-promoted `type: approval` steps and fire their `on_suspended` hooks.
  2. If the job row is terminal: **drain** (`JobStepRepo::has_live_steps`; a live step → stop here) → clear the cancel signal → **claim** (the exactly-once CAS) → build the `TerminalPlan` and run it in order: **propagate** to the parent step → **retry-or-hooks** (task-level retry job if the plan allows it, else `fire_hooks`) → notify sync waiters → close and archive the log.
  An unresolvable workspace or task does not skip drain/claim/propagation — only retry, hooks, notify and archive are skipped (matches the old `handle_job_terminal`; pinned by `metrics_test::cascading_cancel_counts_parent_exactly_once`). The workspace-missing log line has two wordings, both inherited: a terminal job gets the old terminal path's `… not found for terminal job … — skipping hooks and S3 upload` warning, a non-terminal one the old `orchestrate_after_step` error.

- **A dispatch error inside step 1 is logged, not propagated** (D1). Only an error that ESCAPES `handle_task_steps` reaches this — a `type: task` step row missing its `action_spec`, or a DB error; an unknown task name is already caught inside `handle_task_steps_pass` by `fail_task_step`. Before the settlement module the parent leg of propagation returned it with `?`, which skipped the REST of that advance — reconcile, approval dispatch, `on_suspended` hooks. Terminal handling was never reachable there: the job cannot be terminal while the erroring step is still non-terminal. Regression tests: `test_parent_dispatch_failure_after_child_settles_still_runs_terminal_actions` and `test_parent_dispatch_error_escapes_but_approvals_still_dispatch`.

- **Retry fall-through fires hooks.** `TerminalPlan.hooks` is `HookKind::None` whenever `plan.retry` is true, so `advance` re-derives the kind with `terminal::hook_kind(&job.status)` when `retry::create_retry_job` returns `Ok(None)` or `Err` — the failure is final after all and `on_error` must fire. `hooks::fire_hooks` (the 4-arg wrapper, for callers holding only a row) uses `hook_kind` for the same reason.

- **Drain before claim, and why**: a job row can be terminal while its workers still run (`JobRepo::cancel` stamps `cancelled` immediately). Without the gate the first worker to report would win the one-shot claim and `close_log` + archive while a sibling is still emitting; the later completion loses the claim and the archive is never refreshed. `clear_cancelled` sits behind the same gate so the cancellation signal stays visible until the workers acknowledge.

- **Reconcile's CTE** (`JobRepo::get_settled_descendants_with_running_parent_step`) — three predicates: descendant is terminal, its `parent_job_id` chain leads back to the root, and its own parent step is still `running`. Walks the WHOLE descendant chain (not just direct children — a grandchild that settles at creation can leave an intermediate job `running` forever with nothing to complete it), rows deepest-first, capped at `MAX_TASK_DEPTH` (10).

- **`CreatedJob`** — not `Copy`, its `terminal_at_creation` flag private; only `job_created` and `agent_child_created` (both take it by value) can act on it. Obligation: every creation call site must end in one of those two, with one deliberate exception — `dispatch::handle_task_steps_pass` reads only `created.job_id` and drops the struct, because `advance` runs `reconcile` immediately after task dispatch and that is what picks up a child which settled at creation (the old creator relied on `reconcile_settled_children` in exactly the same way). Residual hole: `create_job_for_task_detailed(..).await?.job_id` moves the id out and drops the struct — `#[must_use]` does not catch field access; reviewer discipline is the only guard (tracked in `docs/internal/TODO.md`).

- **`JobRepo::settle`** — the predicated settlement write: `UPDATE job SET status=$2, output=COALESCE($3, output), completed_at=NOW() WHERE status IN ('pending','running')`. Returns whether the row was written; a `false` means the row was already terminal (e.g. an explicit cancellation) and the caller re-reads the status. Replaces the "never overwrite an explicit cancellation" re-read that used to live in the decider.

- **Task retry is now functional** — see § Retry Mechanism for the persistence rule and the regression tests.

- **Hook jobs** go through the same step builder as ordinary job creation, `job_creator::build_step` — one construction site, one lifecycle (`CreatedJob` → `job_created`). The hook payload is inserted as the step's literal, already-rendered input; a `type: task` hook still goes through the full creator. See § Hooks.

- **The `None` workspace-config mode is gone**: `cascade_and_settle` always takes `&WorkspaceConfig`. Tests that used to pass `None` now build one with the `workspace_with(&task) -> WorkspaceConfig` helper (duplicated per test file — `orchestrator_test.rs`, `integration_test.rs`, `mcp_test.rs` — there is no shared test module in `crates/stroem-server/tests/`).

### Conditional Flow Steps (`when`)
- `FlowStep.when: Option<String>` — Tera expression evaluated at step promotion time
- Truthy if non-empty and, after trim and lowercase, not "false", "0", "null" or "none".
- All-deps-skipped rule: if ALL deps are skipped the step is cascade-skipped, unless EVERY skipped dependency has `continue_when_skipped: true` on ITS OWN flow definition AND no dep is `tainted` (skipped `unreachable` or with a NULL `skip_reason`); a tainted step still runs when the dependent also has `continue_on_failure`. Formula: `bypass = all_deps_cws && (!tainted || S.continue_on_failure)` (`cascade.rs::all_skipped_decision`, takes `task: &TaskDef` to look up each dependency's flag). `continue_on_failure` alone is failure-only since 0.16.2 (it used to bypass this rule). **(0.16.3)** `continue_when_skipped` moved from the dependent to the dependency — a step opts its own skip into being tolerated by whatever depends on it, mirroring how `continue_on_failure` tolerates a dependency's failure; the flag on the dependent itself is no longer read. Mixed deps (≥1 completed): a `condition`/`empty`/`cascade` skip counts as satisfied, but **(0.16.5)** a `tainted` skipped dep (skipped `unreachable`, or NULL reason) counts like a FAILED dependency — the dependent is skipped `unreachable` unless it has `continue_on_failure` (`cascade.rs::dep_tainted`, read by `deps_satisfied` and `any_dep_carries_failure`, so R2, R3 and the R4 placeholder branch agree). Before 0.16.5 a completed sibling laundered the failure: a merge step ran, and everything after it, although one of its branches had failed upstream (prod 2026-09-17, `jobs/recalc-pipeline`). Regression tests: `mixed_completed_and_unreachable_skipped_deps_skip_unreachable`, `failure_on_one_branch_stops_the_merge_and_everything_after_it`.
- Skip reasons: `job_step.skip_reason` (migration 046) — `condition` (own `when` false), `empty` (zero `for_each` items), `cascade` (all deps skipped by choice), `unreachable` (R3/R4/R5 failure skips, and any step with a tainted dep — all-skipped or mixed — that lacks `continue_on_failure`; an ALL-skipped tainted step is `unreachable` even WITH `continue_on_failure` unless every dep also has `continue_when_skipped`). Every writer of `status='skipped'` (`skip_steps_tx`, `mark_skipped`, `seed_steps_tx`) takes the reason. The CLI local runner mirrors the flag but never produces `unreachable` (it aborts on failure).
- Skipped steps have `{ "output": null }` in render context for downstream `when` expressions
- Condition evaluation errors → step fails (not silently skipped)
- Evaluated in cascade phase P1 (see Step Cascade).

### For-Each Loops (`for_each`)
- `FlowStep.for_each: Option<serde_json::Value>` — Tera template string or literal JSON array
- `FlowStep.sequential: bool` — instances run one at a time when true (default: parallel)
- Creates N instance steps (`step[0]`, `step[1]`, ...) from placeholder. `each.item` + `each.index` injected at claim time.
- Sequential: `[i+1]` promoted after `[i]` completes. Output aggregated as ordered array on placeholder.
- `when` + `for_each`: `when` evaluated first; if falsy, step skipped without expansion
- **Placeholder lifecycle has one owner**: `cascade.rs` rules R0 (adopt), R4 (retire/expand), R5 (sequential advance), R6 (rollup). A placeholder whose dependency failed or was cancelled (without `continue_on_failure`) is retired `skipped` by R4; rollup and sequential advance are global rules re-evaluated on every cascade of the job (self-healing), not keyed on the completing instance.
- **Sequential failure stops the loop immediately**: any `failed`/`cancelled` instance without `continue_on_failure` skips every pending instance, even a successor of a later completed instance (`[failed, completed, pending]` → `[2]` skipped).
- Only `failed` instances fail a loop; `cancelled` instances count as terminal but do not. Rollup output has one element per existing instance, `null` where the instance produced none.
- Empty array → skipped; non-array → fails; instance failure → placeholder fails (unless `continue_on_failure`)

### Task State Snapshots
- Immutable state snapshots persisted across job runs per workspace+task
- `STATE:` stdout protocol: `STATE: {"key": "value"}` — structured state saved as `state.json` in snapshot tarball
- `/state` directory: previous snapshot mounted read-only, `/state-out`: writable directory for new state
- `StateArchive` trait with S3 and Local implementations, separate from `LogArchive`
- DB: `task_state` table tracks snapshots (id, workspace, task_name, job_id, storage_key, size_bytes, has_json, created_at)
- Snapshot rows resolved pool-only once per entry (`render_context::latest_snapshots`); the parsed `state.json` is persisted at upload (migration 047, `json` column bound as text with `::json` — never `jsonb`, never bound as `Value`: the NUL escape). Pre-047 rows: claim falls back to the archive for rendering only; no backfill (see `docs/superpowers/specs/2026-09-13-task-state-storage-hardening-design.md`). Each claim re-resolves, so intra-job state propagation for sequential steps still holds (state written by step N is visible to step N+1's templates and mount).
- Worker API: `GET /worker/state/{ws}/{task}` (download), `POST /worker/state/{ws}/{task}/{job_id}` (upload)
- Tera templates: `{{ state.key }}` and `when: "{{ not state or state.days_remaining < 30 }}"` — a `when` expression MUST be wrapped in `{{ }}`; `evaluate_condition` (`template.rs:338`) renders the string and tests truthiness, so a bare expression renders to itself and is ALWAYS true (a silent no-op)
- Config: optional `state_storage` section (prefix, max_snapshots, optional archive override). Defaults to log archive backend.
- Retention: `max_snapshots` per task (default 5), pruned on upload
- Runners: Shell (env vars), Docker (bind mounts `/state:ro` + `/state-out:rw`), Kube (emptyDir volumes)
- Always available, no opt-in. Upload only triggered if `/state-out` has content or `STATE:` lines emitted.
- Snapshots can also be uploaded out-of-band via `POST /api/workspaces/{ws}/tasks/{task}/state` and `POST /api/workspaces/{ws}/state` (see `docs/src/content/docs/guides/task-state.md` §"Uploading state manually"). Creates a synthetic `source_type="upload"` job for audit.

### Global Workspace State
- Workspace-scoped state shared across all tasks (any task can read and write)
- `GLOBAL_STATE:` stdout protocol: `GLOBAL_STATE: {"key": "value"}` — structured state in `state.json`
- `/global-state` directory: previous snapshot read-only, `/global-state-out`: writable for new state
- DB: `workspace_state` table — same structure as `task_state` but scoped by `workspace` only (task_name for provenance)
- Worker API: `GET /worker/global-state/{ws}`, `POST /worker/global-state/{ws}/{job_id}`
- Tera templates: `{{ global_state.key }}` — separate namespace from task state `{{ state.key }}`
- Storage key format: `{prefix}__global__/{workspace}/{job_id}.tar.gz`
- Same `StateArchive` backend and retention model as task state
- Concurrent writes: last writer wins (immutable snapshots, latest by `created_at`)

### Artifacts
- Per-job opaque files produced by successful steps. Convention dir `/artifacts/` (or `$ARTIFACTS_DIR`); recursive scan, dotfiles included, symlinks skipped + warned.
- Per-file 100 MiB cap, per-job 1 GiB cap, both configurable under `artifact_storage:`.
- Success-only upload: failed/cancelled steps discard `/artifacts/`. Upload retried 3× with backoff; terminal failure fails the step AND cleans up already-uploaded blobs for that step.
- Per-job namespace, `UNIQUE(job_id, name)`, last-writer-wins on collision (for_each authors must template filenames).
- Worker sniffs Content-Type via `infer` crate; server stores verbatim, applies `X-Content-Type-Options: nosniff` and inline-when-safe `Content-Disposition` (images, PDF, text/plain, text/markdown). HTML/SVG/XML/JSON forced to attachment.
- Storage via `BlobArchive` trait (unified backend for logs, state, artifacts). S3 + Local impls; `put_stream`/`get_stream` overrides for memory-flat artifact transfers.
- Retention: cascades with `job` row. FK `RESTRICT` + explicit two-phase delete (blob → row).
- Runner support: shell, `script:docker`, `type:docker`. Kube modes deferred (same gap as state file-mount).
- Hooks: `hook.artifacts` is a list of `{name, content_type, size_bytes, url, step_name, created_at}`.
- ACL: `View` on the task. No new permission level.

### Retry Mechanism
- **Two layers**: step/action retry (in-place) and task retry (new job).
- **Step retry**: `FlowStep.retry: Option<RetryConfig>` — retries the individual step on failure. In-place: same `job_step` row reset to `ready` with `retry_at` backoff timestamp. Previous attempt errors stored in `retry_history` JSONB array.
- **Action retry**: `ActionDef.retry: Option<RetryConfig>` — default retry for all steps using this action, overridden by step-level.
- **Task retry**: `TaskDef.retry: Option<RetryConfig>` — retries the entire task as a new job on failure. Creates new job with `source_type = "retry"`, linked via `retry_of_job_id`/`retry_job_id`.
- **Resolution**: step.retry > action.retry (most specific wins). Task retry is independent.
- **RetryConfig**: `max_attempts` (1-10) counts TOTAL executions, including the initial run — `max_attempts: 3` ⇒ at most 3 executions (2 retries); `max_attempts: 1` disables retry. This is a user-facing/model-level meaning only: the DB columns `job_step.max_retries` / `job.max_retries` keep their original meaning of "retries after the initial run" (retry loop runs while `retry_attempt < max_retries`), so every writer that persists a `RetryConfig` into one of those columns stores `max_attempts - 1` (subtraction is safe — validation guarantees `max_attempts >= 1`). `delay` (HumanDuration, max 1h), `backoff` (fixed/exponential), `jitter` (bool).
- **Attempt counters in messages**: the UI timeline and the `_server` log lines (`settlement::retry::{step_retry_message, step_retries_exhausted_message, task_retry_message}`) both count EXECUTIONS on both sides of the slash: `attempt {retry_attempt + 1}/{max_retries + 1}`. Keep them in sync (unit test `retry_messages_count_executions_consistently`).
- **BackoffStrategy**: `Fixed` (constant delay) or `Exponential` (base * 2^attempt, capped at 2^6).
- **Server-side**: the retry decision is made atomically at the failure write by `JobStepRepo::fail_or_retry` (called through `Settlement::step_failed` at six of its seven sites — see § Settlement's entries table — that fail a step and then advance the job: worker `complete_step`, claim-time render failure, the four recovery phases; the approval REJECT handler calls `fail_or_retry` inline for log-ordering reasons and then `step_settled`). A retried step goes straight from `running`/`suspended` to `ready` with `retry_at`; it is **never observable as `failed`**, so no concurrent cascade or loop rollup can act on it. Retry no longer depends on the workspace being loaded at orchestration time. Callers advance only on `FailOutcome::Failed`. `claim_ready_step` respects `retry_at`. Failure paths that never retry (child-job propagation, task dispatch failure, approval dispatch failure, `when`/`for_each` errors) still call plain `mark_failed`. Retry is decided before `Settlement::advance`'s job/workspace/task lookups, so it also fires when the job's task is missing from the workspace (previously an early return left the step failed with unused budget). Deployment: this change must be running on every server replica before the step-cascade activation release ships (a mixed fleet still produces failed-then-reset rows).
- **Hooks**: `on_error`/`on_cancel` hooks fire only after all retries exhausted (step and task). `source_type = "retry"` is top-level for hook fallback.
- **Interactions**: retry runs before `continue_on_failure`; for-each instances inherit retry config; each retry attempt gets full timeout; agent_state cleared on retry.
- **`job.max_retries` is written at creation from `task.retry.max_attempts - 1` for every creation mode** (child jobs carry it too but never retry: `terminal::plan` gates on top-level). The retry decision is part of the terminal plan, so it applies on every path into terminal handling, including jobs that fail at creation.
- DB: `retry_attempt`, `max_retries`, `retry_backoff_secs`, `retry_strategy`, `retry_jitter`, `retry_history`, `retry_at` on `job_step`. `retry_of_job_id`, `retry_job_id`, `retry_attempt`, `max_retries` on `job`.

### MCP Server (Model Context Protocol)
- Feature-gated: `mcp` cargo feature (enabled by default). Config: `mcp: { enabled: true }` (disabled by default)
- Endpoint: `/mcp` via Streamable HTTP. Crate: `rmcp` with `#[tool_router]` / `#[tool]` macros
- 8 tools: `list_workspaces`, `list_tasks`, `get_task`, `execute_task`, `get_job_status`, `get_job_logs`, `list_jobs`, `cancel_job`
- Auth: Bearer token (API key or JWT) via `tokio::task_local!`. Per-tool ACL checks.
- **Admin mirrors the user (re-derived per request)**: MCP grants admins the same ACL bypass they have in the UI/REST API, but admin is NEVER carried in the OAuth access token. The token is minted with `is_admin: false` (`oauth/token.rs`, defense in depth); `mcp/auth.rs::authenticate` re-reads the user's live `is_admin` from the DB on every request (OAuth path) — matching the API-key path, which also reads it fresh. This means revoking admin takes effect **immediately** (no access-token-TTL lag). Do NOT reintroduce `is_admin` into the minted token; `load_user_acl_context` trusts `claims.is_admin`, and the MCP auth layer is the single place that sets it from the DB.
- **Host allow-list**: rmcp's Streamable-HTTP transport enforces a DNS-rebinding `Host` allow-list (loopback-only by default — would reject all proxy traffic *after* auth passes). `build_mcp_routes` calls `resolve_allowed_hosts()` to allow loopback + the `auth.base_url` host + optional `mcp.allowed_hosts`. `mcp.allowed_origins` (default empty = Origin check off) for browser clients. Middleware order: auth runs outer, rmcp transport (host check) inner — so tokenless probes get 401 while a *valid-token* request to a non-allowed host hits the transport-level Host rejection.

### Prometheus Metrics
- `crates/stroem-server/src/metrics.rs` — recorder install + `gather_gauges` + metric name constants (`STROEM_*`) + RED tower middleware (`track_http_metrics`)
- `crates/stroem-server/src/web/metrics.rs` — `GET /metrics` handler
- Always enabled. Optional config: `metrics: { public: bool }` (default `false` → requires `worker_token` Bearer, same auth posture as `/healthz/detail`).
- Hybrid recording: counters/histograms inline at event sites via `metrics::counter!` / `metrics::histogram!`; gauges sampled at scrape time in `gather_gauges` (2s timeout per DB query via `tokio::time::timeout`, errors logged + skipped, NOT zeroed — Prometheus treats absence as stale).
- DB queries inside `gather_gauges` run concurrently via `tokio::join!` so worst-case scrape latency is bounded at 2s, not 6s.
- RED middleware applied to `/api/*` only. `/worker`, `/hooks`, `/mcp` deliberately excluded (worker traffic would swamp user-facing signal).
- Job-completion counter is incremented inside `settlement::terminal::claim` — see § Settlement.
- Global `replica_id` label added at recorder install — keeps multi-replica scrapes from collapsing into one series.
- Helm: `serviceMonitor.enabled: true` renders `templates/servicemonitor.yaml`; uses `bearerTokenSecret` when `metrics.public: false`.
- New metrics: add a `pub const` in `metrics.rs`, add the recording site, add an integration test in `crates/stroem-server/tests/metrics_test.rs`, document in `docs/src/content/docs/operations/metrics.md`.

### Webhook Triggers
- `TriggerDef` tagged enum: `Scheduler`, `Webhook`, and `EventSource` variants
- Handler at `/hooks/{name}` (not under `/api/`). Auth: optional `secret` field (query param or Bearer header)
- Input mapping: `body`, `headers`, `method`, `query` + YAML `input` defaults
- **Sync/async mode**: `mode: "sync"` waits for completion (default: async). `timeout_secs` max wait (default 30, max 300).

### Event Source Triggers
- `TriggerDef::EventSource` variant: long-running queue consumer processes that emit jobs via stdout `OUTPUT: ` protocol.
- **Consumer task**: `task:` field references a regular task whose flow runs the long-lived consumer process.
- **Target task**: `target_task:` field specifies which task to create jobs for (receives emitted JSON as input).
- **Stdout protocol**: Lines starting with `OUTPUT: ` followed by valid JSON become job input for target task, merged with trigger `input` defaults. All other stdout/stderr lines are captured in consumer task's log view.
- **Environment & input**: `env:` provides Tera-templated environment overrides for consumer execution. `input:` provides defaults merged into each emitted job.
- **RestartPolicy enum**: `Always` (default), `OnFailure`, `Never` — controls behavior when consumer process exits.
- **Exponential backoff**: `backoff_secs` field (default 5) — doubled on consecutive failures, capped at 5 minutes. Resets on clean exit.
- **Backpressure**: `max_in_flight` field limits concurrent pending/running jobs for target task. Server tracks count; stdout reading pauses when limit reached, creating natural Unix pipe backoff.
- **EventSourceManager**: Server-side background task creating/monitoring consumer task jobs via normal task dispatch. Handles lifecycle (start, failure, restart per policy).
- **Job tracking**: Emitted jobs have `source_type: "event_source"`, `source_id: "{workspace}/{trigger_name}"` for audit trail.

### Hooks (on_success / on_error / on_cancel)
- `HookDef`: `action` + `input` map. Task-level and workspace-level (fallback when task has none).
- `on_success` fires on completed, `on_error` fires on failed, `on_cancel` fires on cancelled. Each is independent.
- Workspace-level hooks only fire for top-level jobs. Top-level source types (workspace-hook fallback): `hooks::is_top_level_source` — api, user, trigger, webhook, mcp, retry, rerun, restart.
- Recursion guard: `source_type = "hook"` → no further hooks
- **Hook chains are bounded at `hooks::MAX_HOOK_CHAIN_DEPTH` (3)**: the recursion guard alone does NOT stop an indirect cycle. A `type: task` hook creates a hook job whose own `type: task` step creates an ordinary `source_type = "task"` child — no longer hook-sourced, so its task-level hooks fire again, forever, with fresh job ids no CAS can stop (validation rejects only *direct* self-reference). `hook_chain_depth` walks the ancestry (hook → UUID prefix of `source_id`; otherwise → `parent_job_id`, max 20 hops) and both `fire_hooks` and `fire_suspended_hooks` return early at the limit, logging `[hooks] hook chain depth limit (3) reached` to the job. Regression test: `test_indirect_hook_cycle_is_bounded`.
- Hook actions can be `type: task` — creates full child job instead of single-step hook job

### Connections
- Named, typed objects storing external system configs (DB creds, API endpoints)
- `ConnectionTypeDef` — property schema. `ConnectionDef` — optional `connection_type` + flattened values
- When `InputFieldDef.field_type` is not a primitive, it's a connection type reference — resolved to full values object
- Untyped connections skip type validation but still work as task inputs
- `shared: bool` (default false) — opt-in for cross-workspace use.

### Agent Actions (type: agent — LLM Calls)
- Worker-side dispatch. Workers need `"agent"` tag and `agents:` config with LLM provider API keys.
- 19 providers via `rig-core`. `prompt` and `system_prompt` are Tera templates.
- **Structured output**: `OutputDef::to_json_schema()` → JSON Schema injected into system prompt
- **Multi-turn** (Phase 7B+C): `tools: [{task: "..."}, {mcp: "..."}]` — task tools create async child jobs, MCP tools call external servers sync
- **Task-tool children must not be born terminal**: a tool result reaches the agent step only through normal step completion (`Settlement::propagate`'s `agent_tool` branch), so a child that is already terminal at creation (every root step skipped by `when`, or a server-dispatched root step that failed) can never deliver one. `agent_task_tool` calls `Settlement::agent_child_created`, which returns `Err(BornTerminal)` in that case (500 to the worker) instead of finalizing — the worker has not yet recorded the child id in `agent_state`, so propagation would mark the agent step ready or terminal out from under the still-running worker. `agent_child_created` first runs `reconcile` on the CHILD's own subtree, so a child that settles because a nested grandchild settled is caught by the same check. See § Settlement.
- **Agent registration barrier**: `Settlement::propagate`'s `agent_tool` branch returns `Ok(())` without touching the step when the parent step's `agent_state` is `None`, or when it parses but does not list this child in `pending_tool_calls`. The worker records pending child ids only AFTER the creation response, so anything earlier is unregistered — falling through to ordinary `mark_completed` would settle an agent step whose worker is still running. `agent_save_state` and `agent_suspend_step` (worker API) call `Settlement::agent_children_registered`, which replays `propagate` for every pending child that is already terminal right after persisting the state — that is what lifts the barrier. The child's terminal-handling claim was consumed by its own settlement, so the replay calls `propagate` directly, not `advance`.
- **ask_user**: `interactive: true` enables suspension → user approves → worker resumes
- **MCP client**: Servers spawn on worker (stdio/SSE). Config: `mcp_servers:` in workspace YAML
- **Max turns**: safety limit (default 25, max 100)

### Approval Gates (type: approval)
- Server-side dispatch. `ActionDef.message` is Tera template shown to approver.
- `POST /api/jobs/{id}/steps/{step}/approve` — approve or reject with `rejection_reason`
- `on_suspended` hooks fire when step enters `suspended`. Recovery sweep fails timed-out suspended steps.
- Workers never claim approval steps.

### Task Actions (type: task)
- `ActionDef.task` references another task by name. Cannot have `cmd`, `script`, `source`, `image`, `runner`, or `language`.
- Server-side dispatch. `create_job_for_task_inner()` uses `Box::pin` for recursive async.
- `compute_depth()` max 10 levels. Child propagation via `Settlement::propagate` (see § Settlement).
- Self-referencing task actions rejected by validation.
- **Dispatch failures re-orchestrate**: `handle_task_steps` runs in passes; every failure branch (depth exceeded, input render/prepare error, child-job creation error) goes through `fail_task_step` → `mark_failed` + `cascade_and_settle` (shared with approval steps), and the pass loop repeats while failures keep promoting further task steps. A server-side `mark_failed` that does NOT re-cascade leaves dependents `pending` and the job stuck `running` (prod 2026-09-07). Regression test: `test_task_step_dispatch_failure_cascades_and_fails_job`.
- **Settlement, the claim, the drain gate and reconcile** are all owned by the `settlement` module now — see § Settlement and the glossary entries in `CONTEXT.md`.
- Post-commit initialisation errors fail the live steps with `[creation] initialisation failed: …` and mark the job failed, both in ONE transaction (a half-applied compensation leaves failed steps under a non-terminal job that no sweep revisits). See `settlement::dispatch::init` (§ Settlement) for what the coordinated init block covers.
- Action-level `input` defaults for a `type: task` step come from the persisted `action_spec.input` (serialized onto the step at job creation), never a live lookup of the action definition — so editing the action after creation does not change an in-flight job's child. The creation-time literal pre-check for `type: task` actions runs against the **TASK's** input schema (`job_creator::precheck_task_step_literals`), not the wrapping action's, skipping `when`-guarded steps (unchecked at creation, may fail at dispatch if reached). See § Cross-Workspace References above for the full `A`/`O`/`T` resolution and provenance rules.

### Config Loading
- `config` crate loads YAML + env var overrides. Prefix: `STROEM__`, separator: `__`
- Example: `STROEM__DB__URL` overrides `db.url`
- Helm: ConfigMap for YAML, secrets via `extraSecretEnv` as `STROEM__` env vars

### Database
- Runtime sqlx queries, NOT compile-time checked. Migrations in `crates/stroem-db/migrations/`
- Job claiming: `SELECT ... FOR UPDATE SKIP LOCKED`
- `job.revision` stores workspace revision at creation. Sub-jobs and hook jobs inherit parent's revision.

### Job Lineage (retry / rerun / restart)
- **`retry_of_job_id`** — server-initiated retry of a failed job. Same logical run, attempt N+1.
- **`source_job_id`** + **`source_type = 'rerun'`** — user clicked **Re-run** in the UI. New job uses `source.raw_input` to prefill the form; UI sends `••••••` for fields the user didn't touch and the server replaces it with the source value before merging defaults / resolving connections (see `crates/stroem-common/src/template.rs::resolve_rerun_sentinels`).
- **`source_job_id`** + **`source_type = 'restart'`** + **`restart_from_step`** — Restart From Step (spec `docs/superpowers/specs/2026-09-07-restart-from-step-design.md`). `restart::compute_restart_set` → `RestartPlan`; `job_creator::create_restart_job` uses `CreationMode::Restart` to seed carried rows (`job_step.carried_over = true`, `JobStepRepo::seed_steps_tx`) inside the creation transaction; the normal post-commit cascade + `settle_if_all_terminal` close the job when the restart set is empty/skipped. Input = source `raw_input` replayed (never the resolved `input`); revision = current. Restart jobs are top-level for hooks and EXCLUDED from duration stats; carried failures are flagged in `hook.failed_steps[].carried_over`. Endpoint `POST /api/jobs/{id}/restart` (`dry_run` for the UI preview). Known limits: state snapshots = latest at claim time; artifacts not carried.
- `rerun` and `restart` are top-level source types for workspace-level hook fallback — see § Hooks.
- **Top-level jobs only.** Both Restart and Re-run reject a source job with a `parent_job_id` or a `source_type` of `hook` / `task` / `agent_tool` / `upload` (400). Both always create a parentless job, so a child restart would strand the original parent's step, and a `hook` restart would relabel the job `restart` — a source type `is_top_level_source` accepts — escaping the hook recursion guard. Single rule: `web/api/jobs.rs::is_top_level_job`, mirrored in the UI by `ui/src/lib/job-status.ts::isTopLevelJob` (needs `parent_job_id`, which `GET /api/jobs/{id}` returns).
- **Restart validates required input.** `CreationMode::Restart` is the ONLY creation mode that checks required-with-no-default fields after `merge_defaults` (400 if any are missing). Normal/rerun/webhook/trigger creation deliberately skips that check — their input shapes do not match the task schema. The bail message must keep the word "required" in its OUTERMOST text; `classify_execute_error` keys off that to answer 400 rather than 500.
- **`raw_input`** — verbatim user submission stored on every job, before `merge_defaults` and `resolve_connection_inputs`. Returned by `GET /api/jobs/{id}` with workspace-defined secret values redacted to `••••••`. NULL for jobs created before migration `032_job_raw_input_and_lineage.sql`. **Redaction limitation:** the existing `redact_response` only matches values listed in `workspace.secrets`; user-typed secret values not present in the workspace config are stored and returned as plain text (same exposure as the existing `job.input` column — pre-existing limitation, not introduced by Re-run prefill).

### Health Check
- Three endpoints, split by question. `GET /livez` (unauthenticated, Helm LIVENESS): should this process be restarted? — background loops only, NO DB check (a restart does not fix a DB outage); body `{"status": "ok" | "stalled" | "stopped"}`, no task name. `GET /healthz` (unauthenticated, Helm READINESS): can this replica serve traffic? — DB only, unchanged. `GET /healthz/detail` (worker token): leader flag + per-task `ok | stalled | stopped | follower`. A hung scheduler must restart the pod, not pull a replica that still serves API/worker traffic out of the Service — never merge the two probes.
- Two signals per loop in `BackgroundTasks`: `AliveGuard` flags (`*_alive` — catches an EXITED task) and `Heartbeat`s (`*_beat` — catches a HUNG one; prod 2026-09-16 the scheduler sat silent for 9 h with its guard held). `Heartbeat` uses a process-local MONOTONIC clock (wall-clock corrections must neither fake nor hide a stall). Pure policy: `web/health.rs::task_health(alive, beat_age, stall_after)` → `Ok | Stopped | Stalled`.
- Failure rules: `Stalled` fails `/livez` and `/healthz/detail` on ANY replica (followers run the loops too). `Stopped` fails `/healthz/detail` on the leader (followers report `"follower"`), and fails `/livez` only on the leader AND only if the loop ever beat — a never-started task (router-only tests, the window before `main` spawns it) must not fail the probe.
- **Beat = progress, not iteration.** Each loop beats at the top of its iteration, once per unit of work inside it, AND once more after the work before it sleeps (so the sleep is never charged to a unit that finished just inside its budget) (per fired trigger, per recovered step, per retention job, per collected/reconciled event source and per cancelled consumer job), so a long batch of bounded units does not read as a stall. Thresholds bound ONE unit: scheduler 300 s, event source 600 s, recovery `max(1800, 5 × sweep_interval_secs)`. The scheduler never sleeps longer than `scheduler::MAX_SLEEP` (30 s). A NEW background loop needs a guard, beats at both levels, and a row in `health::background_report`. Known gap: a single unit with an unbounded external call (`force_refresh` git reload, `vals`) can still outlast its threshold — see TODO.md. The workspace watcher is not one of these three loops and has its own deadline/backoff/observability path instead (`LoadBudget`, `stroem_workspace_load_overdue`, see § Multi-Workspace's workspace-refresh bullet); external reloads (API/peer/`force_refresh`) still take no load permit and have no watchdog (spec § 4.10, TODO.md).
- Gauge `stroem_background_task_last_tick_age_seconds{task}` (absent before the first beat). Tests: `health::tests::test_task_health_*`, `ha_test::livez_*`, `metrics_test::background_task_last_tick_age_reflects_heartbeat`.

### Error Handling (AppError)
- `AppError` enum in `web/error.rs`: `BadRequest`, `Unauthorized`, `Forbidden`, `NotFound`, `Conflict`, `Internal`
- `Internal` logs full error, returns generic message. `From<anyhow::Error>` and `From<sqlx::Error>` impls.

### Authentication
- **User auth**: Optional JWT (access 15min, refresh 30d with rotation). Enabled via `auth` section in config.
- **Worker auth**: Bearer token from config (`worker_token`)
- **API keys**: `strm_` prefix + 32 hex chars. SHA256 stored in DB. Optional expiry. Frontend: Settings page.
- **OIDC SSO**: Authorization Code + PKCE via `openidconnect`. JIT user provisioning (auth_link → email → create). State in signed HttpOnly cookie.

### ACL (Access Control)
- Config-driven: optional `acl` section. No config = everything open.
- Admin flag bypasses all checks. Groups managed by admins via API.
- Rule evaluation: all matching rules checked, **highest permission wins** (Run > View > Deny).
- Glob matching with `*` wildcard. Task path: `"{folder}/{task_name}"` or `"{task_name}"`.

### Log Storage
- `LogStorage` — local JSONL for live buffering + optional `LogArchive` backend (S3 or local)
- Archive keys: `{prefix}{workspace}/{task}/YYYY/MM/DD/YYYY-MM-DDTHH-MM-SS_{job_id}.jsonl.gz` (gzipped)
- Upload spawned after hooks fire (includes server events).
- **Reads are bounded** (spec `docs/superpowers/specs/2026-09-22-log-tail-streaming-design.md`): `LogStorage::read_tail` (default 256 KiB, whole lines, `truncated`/`total_bytes`) and `LogStorage::stream_full` (NDJSON stream) are the ONLY readers; the String-returning `get_log`/`get_step_log` were removed on purpose — never add a reader that materialises a whole log. Primitives live in `crate::log_read` (`matcher`, `splitter`, `local`, `archive`). Finished jobs: tail = union of local + archive tails; full = in-memory union under `log_storage.read.merge_max_{bytes,lines}`, else local-first single source. `X-Stroem-Log-Source` names the source. Bounds are enforced by `tests/log_peak_alloc_test.rs` (counting allocator, `harness = false`) — a new read path needs a case there. Read fallback: `.jsonl` → legacy `.log` → (terminal) archive; `NotFound` is never a 500.
- Config: `archive` (preferred) or `s3` (legacy) in `log_storage` section.
- **Server events**: `append_server_log()` writes `step: "_server"` entries for hook failures, orchestration errors, recovery timeouts.

### WebSocket Log Streaming
- `GET /api/jobs/{id}/logs/stream` — backfill = the default tail (`read_tail`), then live via `tokio::sync::broadcast`

### Task Duration Stats
- `GET /api/workspaces/{ws}/tasks/{name}/stats?limit=50` — p50/p95/avg/min/max + recent durations + per-step breakdown over last N **completed** runs (View permission sufficient). Failed/cancelled runs excluded; `for_each` instance rows excluded from per-step breakdown.
- Queries: `JobRepo::get_task_duration_stats` / `get_recent_durations` and `JobStepRepo::get_step_duration_stats_for_task` use Postgres `percentile_cont(...)` directly.
- UI: `<DurationInsightsCard>` on Task Detail page; ETA / overrun pill on Job Detail page driven by `lib/eta.ts::computeEta`; per-step `p50: Xs` badges in `<StepTimeline>`.

### Worker Recovery
- `recovery.rs` — sweeper with 4 phases: (1) stale workers → fail steps, (2) timed-out steps, (3) timed-out jobs, (4) unmatched ready steps
- Config: `heartbeat_timeout_secs` (120), `sweep_interval_secs` (60), `unmatched_step_timeout_secs` (30)
- Data retention: optional `retention` section with `worker_hours`, `job_days`
- Strategy: fail, don't retry — avoids non-idempotent side effects
- A `fail_or_retry` DB error for one step is logged to that job and the sweep continues with the next step; before the settlement module it aborted the tick.
- HA: gated on `state.leader.is_leader()`. Followers run the loop but skip sweeps.

### High Availability (multi-replica server)
- **Leader election**: `leader.rs` — one server replica holds a Postgres advisory lock (`pg_try_advisory_lock(0x5354524D4C445201)` = "STRMLDR" + version byte). Lock is held on a dedicated `PgConnection`, released automatically when the connection drops (pod restart, network partition, DB restart). `AppState.leader.is_leader()` is checked at the top of `scheduler.rs`, `event_source.rs`, `recovery.rs` ticks. Default for tests/single-replica: `LeaderElection::always()`.
- **Cross-replica event bus**: `events.rs` — Postgres `LISTEN/NOTIFY` over `sqlx::postgres::PgListener`. Three channels: `stroem_job_cancelled` (job cancel → all replicas' `cancelled_jobs` cache), `stroem_workspace_reloaded` (revision change → peers re-read workspace cache), `stroem_job_log_chunk` (worker log push → peer WS subscribers). Payloads carry the originating replica's UUID; listeners drop self-emitted messages to avoid duplicate broadcasts.
- **Publishers** (one-line each, all best-effort, DB is source of truth):
  - `cancellation.rs` cancel_job — publishes `stroem_job_cancelled` after local insert.
  - `web/worker_api/jobs.rs` append_log — publishes `stroem_job_log_chunk` after local broadcast. Lines > `NOTIFY_MAX_BYTES` (3500) degrade to signal-only.
  - `workspace/mod.rs` watcher — publishes `stroem_workspace_reloaded` when source revision changes. `start_watchers(cancel, Some(event_bus))` opts in.
- **`/healthz`**: `web/health.rs` — leader-aware. Process + DB checks always required. Scheduler/recovery/event_source liveness only failure-eligible on the leader; followers report `"follower"` and return 200. Adds `checks.leader: bool`.
- **Replica id**: generated per-process (UUID v4) in `main.rs`, passed into `EventBus::new` for self-filtering.
- **Config**: `config::log_ha_diagnostics()` logs SHA-256 fingerprints of `auth.jwt_secret` + `auth.refresh_secret` at startup so operators can verify both pods loaded the same value.
- **Helm**: defaults at `server.replicas: 2`, `RollingUpdate` with `maxUnavailable: 0`, `terminationGracePeriodSeconds: 60`, `preStop sleep 10`, PodDisruptionBudget `minAvailable: 1`, topologySpreadConstraints by hostname. See `docs/src/content/docs/operations/high-availability.md`.
- Integration tests: `crates/stroem-server/tests/ha_test.rs` (leader uniqueness, failover, NOTIFY roundtrip per channel, oversize fallback, self-filter).

### React UI
- Pages: Login, Dashboard, Workspaces, Workspace Detail (`/workspaces/:workspace`), Tasks, Task Detail, Jobs, Job Detail, Workers, Users, Settings, plus a `*` Not Found page.
- Auth-aware, SPA with react-router, embedded via rust-embed
- **Workspace is a navigation level**: every workspace name in the UI links to `/workspaces/<name>` (Workspaces list, Tasks page badges/group rows, job header). Task Detail's back arrow goes to the workspace page. Breadcrumbs come from the pure `lib/breadcrumbs.ts::buildBreadcrumbs`, which drops the `tasks` segment of `/workspaces/:ws/tasks/:name` so every crumb links to a real route — keep it in sync when adding nested routes.
- **Task tree**: `components/task-tree.tsx` (`<TaskTree>`) renders the collapsible folder tree used by both the Tasks page and the workspace page; row building lives in the React-free `lib/task-tree.ts::buildRows` (unit-tested). Folder expansion keys are workspace-qualified (`ws::path`) only in grouped mode; workspace groups are open by default (inverted `collapsed` set). localStorage keys: `stroem_tasks_view` (`merged`|`workspace`), `stroem_tasks_expanded_folders`, `stroem_tasks_collapsed_workspaces`. The Merged/By-workspace toggle only renders when tasks span >1 workspace.
- Vitest: `vitest.setup.ts` installs an in-memory `localStorage` because Node 22+'s experimental global shadows jsdom's and is `undefined` without `--localstorage-file`.
- `ui/src/lib/api.ts` — token management. `ui/src/hooks/use-step-log.ts` — step log polling, full-log load and stitching (`lib/log-tail.ts::appendTail`).

### Release Pipeline
- Current state of `.github/workflows/release.yml` (arm64/darwin/windows jobs are commented out, not yet re-enabled): one `build-binaries` job builds all four binaries (`stroem-server`, `stroem-worker`, `stroem`, `stroem-api`) for linux-amd64 only and uploads four `*-x86_64-unknown-linux-gnu.tar.gz` tarballs as GitHub release assets.
- Three single-platform (`linux/amd64` only — multi-arch is not currently built) Docker images published to `ghcr.io/{owner}/stroem-{server,worker,runner}`, tagged `{version}`/`{major.minor}`/`{major}` via `docker/metadata-action`.
- Helm chart (`helm/stroem`) version/appVersion stamped from the tag and pushed to `oci://ghcr.io/{owner}/charts`.
- Final `release` job collects all binary artifacts and creates the GitHub Release with auto-generated notes.
- Release Dockerfiles (`docker/Dockerfile.server.release`, `docker/Dockerfile.worker.release`, `Dockerfile.runner`) COPY the pre-built linux-amd64 binary in.
