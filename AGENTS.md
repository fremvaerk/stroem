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
Phase 6d complete: Git refs on references (`ref:` on actions / `type: task` / triggers; PinStore; pinned jobs).
Phase 6f complete (v0.18.5): File inputs (`type: file`: upload, then the step gets a read-only path; § File Inputs).
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
  - `stroem-api` — Remote server client: `trigger`, `status`, `logs`, `tasks`, `jobs`, `cancel`, `restart`, `rerun`, `approve`, `reject`, `workspaces`.

## Conventions

- **Error handling**: `anyhow::Result` everywhere. Use `.context("msg")` for error chain.
- **Async runtime**: tokio
- **Logging**: `tracing` crate. Use `#[tracing::instrument]` on public functions.
- **Secrets in logs**: a field that can carry a rendered secret is a `stroem_common::secret::Secret<T>`; a secret inside a string is scrubbed (`workspace_set::redact_secrets_in_str`); template errors are value-free (`TemplateError`). `docs/internal/architecture/templating-and-secrets.md` § Secrets in logs.
- **YAML parsing**: `serde_yaml` (direct parsing in tests/models), `config` crate (loading with env var overrides)
- **Database**: sqlx runtime queries (`sqlx::query()` / `sqlx::query_as()`), NOT compile-time macros. `docs/internal/architecture/operations.md` § Database queries (sqlx).
- **Tests**: unit tests in-module; integration tests in ONE `tests/main.rs` binary per crate; Postgres from `stroem-test-support`. `docs/internal/architecture/testing-and-ci.md` § Tests.
- **CodeQL**: advanced setup; test code is `paths-ignore`d. `docs/internal/architecture/testing-and-ci.md` § CodeQL.
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
- Put architecture and pattern detail in the matching `docs/internal/architecture/*.md` file (a `###` section; a new topic may get a new file). `AGENTS.md` gets only an index row or a cross-cutting tripwire. `CLAUDE.md` only imports `AGENTS.md` (`@AGENTS.md`): add nothing else to it.
- Size budget: `AGENTS.md` stays at or below 25 600 bytes (`wc -c AGENTS.md`); Codex reads at most 32 KiB of it by default. Move detail out before you add to it.
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
bun run test           # Unit tests for docs/plugins/
```

- **Internal links never carry the site base** (`base: "/stroem"` in `docs/astro.config.mjs`). Markdown links are root-relative without it (`[Hooks](/guides/hooks/)`): the Sätteri mdast plugin `docs/plugins/base-links.mjs` adds the base at build time (Astro 7's default Markdown processor is Sätteri, so it is a Sätteri plugin, not a remark plugin — `markdown.remarkPlugins` would need `@astrojs/markdown-remark` and switch every page to unified). Sidebar `link:` values are base-less too, because Starlight adds the base itself (`/stroem/llms.txt` rendered as `/stroem/stroem/llms.txt`). The llms generator turns the same links into absolute `https://fremvaerk.github.io/stroem/…` URLs.

### LLM Reference (llms.txt)

- `docs/public/llms.txt` is auto-generated from doc sources by `docs/scripts/generate-llms-txt.ts`
- Contains workflow authoring reference; served at `/llms.txt` on the docs site
- To add/remove sections, edit the `sections` array in the generator script

## Key Patterns

The detail is in `docs/internal/architecture/`, one file per area. Do not load it at session start: before you edit a path in the last column, or work on a section in the middle column, read that file. A `§ <Title>` in code, docs or a topic file names one of these `###` sections. In the last column, `stroem-x/…` means `crates/stroem-x/…`, and a path with no crate name is under `crates/stroem-server/src/`.

| File | Sections | Read it before you edit |
|---|---|---|
| `templating-and-secrets.md` | Secrets in logs · Tera Templating | `stroem-common/src/{template,template_error,tera_engine,tera_compat,json_field,secret}.rs`, `render_context.rs`, `redaction.rs`, `workspace_set.rs`, `web/worker_api/rendering.rs`, `stroem-cli/src/local/error_report.rs`; any error, log line or span that can hold a rendered value |
| `runners.md` | Workflow YAML structure · Action Types and Runners · Runner Architecture · Capabilities, Tags, and Step Claiming | `stroem-runner/`, `stroem-worker/src/{executor,poller,toolchain,workspace_cache}.rs`, `stroem-common/src/language.rs`, the claim in `web/worker_api/jobs.rs` and `stroem-db/src/repos/job_step.rs`, `stroem-db/src/repos/worker.rs` |
| `workspaces.md` | Multi-Workspace · Libraries (Actions, Tasks, Connection Types) · Cross-Workspace References · Connections · Config Loading | `workspace/` (not `pins.rs`), `workspace_set.rs`, `config.rs`, `job_creator.rs`, `web/api/{tasks,workspaces}.rs`, `web/worker_api/workspace.rs`, `stroem-common/src/{workspace_loader,budget,sops,validation}.rs` |
| `git-refs.md` | Git refs (pinned references) | `refs.rs`, `templated_ref.rs`, `trigger_target.rs`, `tarball_cache.rs`, `workspace/pins.rs`, `settlement/deferred.rs`, `web/api/{pinned_source,jobs,workers}.rs`, `web/worker_api/workspace.rs`, `redaction.rs`, `acl.rs`, `stroem-common/src/git_ref.rs`, `stroem-db/src/repos/job_ref_pin.rs`; code that reads `git_ref`, `action_ref` or `task_ref` |
| `job-lifecycle.md` | Timeouts · Step Cascade · Settlement · Conditional Flow Steps (`when`) · For-Each Loops (`for_each`) · Retry Mechanism · Hooks (on_success / on_error / on_cancel) · Approval Gates (type: approval) · Task Actions (type: task) · Job Lineage (retry / rerun / restart) | `cascade.rs`, `settlement/`, `job_creator.rs`, `job_completion.rs`, `restart.rs`, `cancellation.rs`, `web/api/{jobs,restart,rerun,approve}.rs`, `stroem-common/src/{gate,depends_on,dag,duration}.rs`, `stroem-db/src/repos/{job,job_step}.rs` |
| `file-inputs.md` | File Inputs | `input_files/`, `state.rs` (`job_defaults`), `web/api/input_files.rs`, `web/worker_api/input_files.rs`, `stroem-common/src/file_field.rs`, `stroem-db/src/repos/input_file.rs`, `stroem-worker/src/poller.rs`, `stroem-runner/src/kubernetes.rs` (inputs init), `stroem-cli/src/{local/run,remote/trigger}.rs` |
| `state-and-artifacts.md` | Task State Snapshots · Global Workspace State · Artifacts | `state_storage.rs`, `blob_storage.rs`, `web/worker_api/{state,artifacts}.rs`, `web/api/{artifacts,state_upload}.rs`, `stroem-worker/src/artifacts.rs`, `stroem-db/src/repos/{task_state,workspace_state,job_artifact}.rs` |
| `triggers.md` | Scheduler (Cron Triggers) · Webhook Triggers · Event Source Triggers | `scheduler.rs`, `event_source.rs`, `trigger_status.rs`, `trigger_target.rs`, `web/hooks.rs`, `web/api/triggers.rs`, `web/worker_api/event_source.rs`, `stroem-common/src/cron.rs`, `stroem-db/src/repos/trigger_run.rs` |
| `agents-and-mcp.md` | MCP Server (Model Context Protocol) · Agent Actions (type: agent — LLM Calls) | `mcp/`, `oauth/`, `web/api/caller.rs`, `stroem-agent/`, `stroem-worker/src/agent_executor.rs`, the agent routes in `web/worker_api/jobs.rs` |
| `operations.md` | Prometheus Metrics · Database queries (sqlx) · Database · Health Check · Log Storage · WebSocket Log Streaming · Worker Recovery · High Availability (multi-replica server) | `metrics.rs`, `recovery.rs`, `leader.rs`, `events.rs`, `log_storage.rs`, `log_read/`, `log_broadcast.rs`, `runtime.rs`, `web/{health,metrics}.rs`, `web/api/{logs,ws}.rs`, any SQL in `stroem-db/`, `stroem-db/migrations/`, `helm/` |
| `api-and-auth.md` | Error Handling (AppError) · Authentication · ACL (Access Control) · Task Duration Stats | `acl.rs`, `auth.rs`, `oidc.rs`, `web/error.rs`, `web/api/{auth,api_keys,users,middleware,oidc,oauth_consent}.rs`, the stats routes in `web/api/tasks.rs` |
| `ui.md` | React UI | `ui/` |
| `testing-and-ci.md` | Tests · CodeQL · Release Pipeline | a new test file, `stroem-*/tests/main.rs`, `stroem-test-support/`, `.github/workflows/`, `.github/codeql/`, `docker/*.release` |

No row of its own: `stroem-cli/` (read the topic of the feature the command calls), `stroem-common/src/models/` (the topic of the field), `stroem-common/src/{constants,format}.rs`, and worker plumbing in `stroem-worker/src/{client,config,secrets}.rs`.

## Tripwires

Know these rules before you open the topic file. The file in brackets (in `docs/internal/architecture/`) has the detail.

**Rollout**
- No YAML with `ref:` or a templated ref merges before every server replica runs the release [`git-refs.md`].
- `type: file`: every server replica, then every worker, then the YAML [`file-inputs.md`].
- All server replicas run the same Tera: templates render on the server and in the CLI, never on workers [`templating-and-secrets.md`].

**Secrets and templates** [`templating-and-secrets.md`]
- A field that can carry a rendered secret is a `Secret<T>`. Never put a whole request/step struct in an `#[instrument]` span: `skip(..)` / `skip_all` + id fields.
- Errors are value-free: a post-render error names the input field and a type, never the value. No `format!` / `bail!` / `context` in `template.rs`, `validation.rs`, `rendering.rs`, `cascade.rs`, `dispatch.rs`, `job_creator.rs` interpolates a rendered value.
- A NEW path that renders a template and reports the error goes in the render-path table (spec `2026-10-06-tera-2-upgrade-design.md` § 3.4). A NEW path that persists or returns one scrubs it (`redact_secrets_in_str`).
- A NEW claim-failure site passes a `ClaimFailure` and chooses `withheld` by origin. An owner-side error that crosses a workspace boundary is withheld, not scrubbed.
- Only `stroem-cli` calls `TemplateError::raw_detail()` (`stroem-common/tests/raw_detail_guard.rs`).
- Render a value once: no second render (json wrapper errors; action defaults, Tera 2 spec R26).
- Add a template variable in `render_context::build`, never by patching `as_value()`'s map. Framework keys go before step outputs: a step named `job` shadows `job` [also `job-lifecycle.md` § Step Cascade].
- Template references use underscored step names (`say_hello`, not `say-hello`). Wrap a `when` in `{{ }}`: a bare expression is always true [`state-and-artifacts.md`].

**Database** [`operations.md`]
- Values always go through `.bind()`. `AssertSqlSafe` only for SQL whose shape varies, with a `// AssertSqlSafe: …` comment.
- An index or CHECK change on a large table goes in its own migration under NEW names with `IF NOT EXISTS`, and its header documents the manual `CONCURRENTLY` pre-run.
- Bind state JSON as text with `::json`, never `jsonb` or a bound `Value` (the NUL escape) [`state-and-artifacts.md`].

**Tests and CI** [`testing-and-ci.md`]
- A NEW test file is a `mod` line in `tests/main.rs`, never a new target (`stroem-common/tests/test_targets_guard.rs`).
- Get Postgres from `stroem-test-support::{test_pool, test_db}`. Never call `testcontainers` directly.
- Parallel worktrees share one cargo target dir. Build one worktree at a time; `cargo clean -p` every workspace crate before you trust a full run after parallel builds.
- A unit-test module with secret fixtures or validation output lives in a sibling `tests.rs`, never inline. CodeQL categories stay `/language:<language>`.
- A NEW transfer or log read path needs a case in `stroem-server/tests/log_peak_alloc_test.rs` [`operations.md`, `file-inputs.md`].

**Runners and claiming** [`runners.md`]
- `agent` must not be in the claim SQL's server-dispatched `NOT IN` list. The unmatched-step sweep mirrors the claim SQL.
- The Kube init container fetches `?revision=` = `RunConfig.workspace_revision` and downloads to a file with retries, never `curl | tar`.

**Workspaces** [`workspaces.md`]
- A NEW consumer of `config.triggers` checks `WorkspaceManager::triggers_enabled`.
- A trigger fire that cannot create its job has no side effects: no `cancel_previous`, no `skip` row, only a `MISSED` log.
- A load deadline aborts the whole load, never a per-file warning. `WorkspaceEntry::apply_load_result` is the only writer of the published snapshot.
- A value that crosses a workspace boundary into a connection-typed field must be a connection name. When the action owner ≠ the caller, the parent step persists only the caller's bucket `C`.
- Workspace `display_name` / `description` are server config, never workspace YAML.
- Tests build `WorkspaceManager` through `from_entries` / `from_configs` and the `with_*` builders, never `new`.

**Git refs, ACL and redaction** [`git-refs.md`]
- `ref` on hooks, event sources, agent task tools and agent actions is rejected, never ignored (the models have no `deny_unknown_fields`).
- Classify typed errors (`PinError`, `RefResolveError`, `FileInputError`, …) by `downcast_ref`, never by message text. A user-visible pin error goes through `config_for_user`.
- Dispatch reads only the step's `task_*` stamp, never a pin inferred from the parent. The PinStore never shares the watcher's clone or semaphore; a pinned tarball's cache key is `pin-{sha}`.
- A job-scoped route or MCP tool authorises with the job row (`check_job_acl` / `acl::job_task_path`), never a live `tasks.get(..).folder`. Job lists filter in SQL (`build_job_acl_scope`) before `LIMIT`.
- Every outlet of job content redacts with `redaction::job_redaction_values` and fails closed (503 / `MaskAll`), never a silent cut. A NEW way of copying content between jobs stays inside the redaction closure or extends it.
- `child_jobs[]` and `derived_jobs[]` hold identifiers only: redaction skips them whole.
- `pin_store.claim_load_budget_secs` stays below the worker's `request_timeout_secs`.

**Job lifecycle** [`job-lifecycle.md`]
- `cascade::run` stays pure (no DB). Do not reorder its phases. Never wrap `cascade::execute` in a larger transaction.
- Nothing outside `settlement/` reaches a terminal side effect without the claim. Every job-creation call site ends in `job_created` or `agent_child_created`; `#[must_use]` does not catch `.job_id` field access.
- Hook lineage is `source_job_id`, never `parent_job_id`.
- A `type: task` step's action defaults come from the persisted `action_spec.input`, never a live action lookup.
- A `RetryConfig` stored in a `max_retries` column is `max_attempts - 1`. Attempt counters in messages count executions on both sides of the slash.
- The Restart missing-input bail keeps the word "required" in its outermost text (`classify_execute_error` → 400).

**File inputs** [`file-inputs.md`]
- A NEW job-creation path takes `JobDefaults` from `AppState::job_defaults`. If it cannot carry an upload, it runs `input_files::preflight` before its own side effects.
- File IDs become paths through a `JobFiles`, never the live task schema, and are never patched into the render context afterwards (`Snapshots.files`).
- `FileInputError` never holds an ID, name or path. An upload URL is never built from request headers (`auth.base_url`).
- The UI's Re-run prefill and download links read only `input_files[].field` / `file_id`, never `raw_input` / `input`.

**Triggers** [`triggers.md`]
- Tell an event-source consumer job from an emitted job by `input ? '_fingerprint'`, never by task name.

**Agents and MCP** [`agents-and-mcp.md`]
- An MCP tool calls the same `pub(crate)` function as its REST route (`Caller`, `app_error`). Never re-implement a job action or hand-write a `json!` of job content in `mcp/tools.rs`; cap a response only after redaction.
- Never put `is_admin` into the minted MCP OAuth token: `mcp/auth.rs` re-reads it from the DB per request.
- A rig upgrade can change persisted agent history: re-check `legacy_history::upgrade` on every rig bump.

**Operations** [`operations.md`]
- A NEW background loop needs an `AliveGuard`, beats at both levels and a row in `health::background_report`. `Heartbeat` uses a monotonic clock.
- Never merge `/livez` (liveness) and `/healthz` (readiness).
- Never add a log reader that materialises a whole log: only `LogStorage::read_tail` / `stream_full`. A missing log is never a 500.
- The leader-lock connection sends only simple-protocol `sqlx::raw_sql`, never `sqlx::query`. Never add another independent leader timing knob.
- A NEW metric: a `pub const` in `metrics.rs`, the recording site, a test in `metrics_test.rs`, a line in `docs/src/content/docs/operations/metrics.md`.

**UI** [`ui.md`]
- Tailwind v4: write `h-(--x)`, never `h-[--x]` (it compiles to invalid CSS, with no error).
- CodeMirror stays out of the initial chunk. Reconfigure the editor through a `Compartment`, never recreate it.
- Change a race in `ui/src/lib/step-log-state.ts` with a model test first. A NEW nested route updates `lib/breadcrumbs.ts::buildBreadcrumbs`.
