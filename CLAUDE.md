# Strøm v2 -- Development Guide

## Project Overview

Strøm is a workflow/task orchestration platform. Backend in Rust, frontend in React.
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
Two independent routing axes (split from the pre-041 single-`tags` model in migration `041_worker_capabilities.sql`):
- **`capabilities`** on worker — runners supported. Any of `"script"`, `"docker"`, `"kubernetes"`, `"agent"`. Required. Matched against step's `required_ability` (derived from action type + runner).
- **`tags`** on worker — free-form **reservation labels** (taints). Empty = permissive. Non-empty = worker ONLY claims steps whose `required_tags` include ALL labels here. Matched against step's `required_tags` (user-declared action tags only, no longer prefixed with an ability token).
- Claim SQL: `worker.capabilities @> to_jsonb(required_ability) AND worker.tags <@ required_tags::jsonb`.
- User pattern "reserve worker X for step Y": worker `tags: ["some-label"]` + action `tags: ["some-label"]`. Other steps never leak onto worker X.
- Recovery's unmatched-step sweep applies both checks (`get_unmatched_ready_steps`).

### Multi-Workspace
- Server config: `workspaces:` map with named entries (folder or git source)
- `WorkspaceSource` trait with `FolderSource` and `GitSource` impls
- `GitSource` tests use local bare repos (`file://` URL) via `git2`. Tests require `#[tokio::test(flavor = "multi_thread")]` due to `block_in_place`.
- **Error recovery**: Failed workspaces get placeholder entries; watchers retry on each poll cycle
- Smart polling via `peek_revision()`: FolderSource hashes metadata (30s), GitSource uses ls-remote (60s default)
- API routes workspace-scoped: `/api/workspaces/{ws}/tasks/{name}/execute`
- Worker: `WorkspaceCache` with immutable revision-based dirs, `WorkspaceGuard` (RAII ref-counted), ETag caching

### Libraries (Actions, Tasks, Connection Types)
- Import shared actions, tasks, and connection types from Git repos or local folders
- Defined in `server-config.yaml` (`libraries:` + `git_auth:`), shared across all workspaces
- Namespace separator: `.` (dot) — e.g. `common.slack-notify`
- During import: actions, tasks, connection types are prefixed; triggers, secrets, connections are ignored
- Internal reference rewriting: action refs in flow steps, task refs, hook actions, connection-type input fields
- CLI `stroem validate` skips `.`-containing names with a warning; server validates fully after resolution

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

### Conditional Flow Steps (`when`)
- `FlowStep.when: Option<String>` — Tera expression evaluated at step promotion time
- Truthy if non-empty and not `"false"` or `"0"`. Condition-false → `skipped`
- All-deps-skipped rule: if ALL deps are skipped, step is cascade-skipped
- Skipped steps have `{ "output": null }` in render context for downstream `when` expressions
- Condition evaluation errors → step fails (not silently skipped)

### For-Each Loops (`for_each`)
- `FlowStep.for_each: Option<serde_json::Value>` — Tera template string or literal JSON array
- `FlowStep.sequential: bool` — instances run one at a time when true (default: parallel)
- Creates N instance steps (`step[0]`, `step[1]`, ...) from placeholder. `each.item` + `each.index` injected at claim time.
- Sequential: `[i+1]` promoted after `[i]` completes. Output aggregated as ordered array on placeholder.
- `when` + `for_each`: `when` evaluated first; if falsy, step skipped without expansion
- Empty array → skipped; non-array → fails; instance failure → placeholder fails (unless `continue_on_failure`)

### Task State Snapshots
- Immutable state snapshots persisted across job runs per workspace+task
- `STATE:` stdout protocol: `STATE: {"key": "value"}` — structured state saved as `state.json` in snapshot tarball
- `/state` directory: previous snapshot mounted read-only, `/state-out`: writable directory for new state
- `StateArchive` trait with S3 and Local implementations, separate from `LogArchive`
- DB: `task_state` table tracks snapshots (id, workspace, task_name, job_id, storage_key, size_bytes, has_json, created_at)
- State resolved at **claim time** (not job creation) — enables intra-job state propagation for sequential steps
- Worker API: `GET /worker/state/{ws}/{task}` (download), `POST /worker/state/{ws}/{task}/{job_id}` (upload)
- Tera templates: `{{ state.key }}` and `when: "not state or state.days_remaining < 30"`
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
- **RetryConfig**: `max_attempts` (1-10), `delay` (HumanDuration, max 1h), `backoff` (fixed/exponential), `jitter` (bool).
- **BackoffStrategy**: `Fixed` (constant delay) or `Exponential` (base * 2^attempt, capped at 2^6).
- **Server-side**: retry check in `orchestrate_after_step()` before failure cascade. `claim_ready_step` respects `retry_at`.
- **Hooks**: `on_error`/`on_cancel` hooks fire only after all retries exhausted (step and task). `source_type = "retry"` is top-level for hook fallback.
- **Interactions**: retry runs before `continue_on_failure`; for-each instances inherit retry config; each retry attempt gets full timeout; agent_state cleared on retry.
- DB: `retry_attempt`, `max_retries`, `retry_backoff_secs`, `retry_strategy`, `retry_jitter`, `retry_history`, `retry_at` on `job_step`. `retry_of_job_id`, `retry_job_id`, `retry_attempt`, `max_retries` on `job`.

### MCP Server (Model Context Protocol)
- Feature-gated: `mcp` cargo feature (enabled by default). Config: `mcp: { enabled: true }` (disabled by default)
- Endpoint: `/mcp` via Streamable HTTP. Crate: `rmcp` with `#[tool_router]` / `#[tool]` macros
- 8 tools: `list_workspaces`, `list_tasks`, `get_task`, `execute_task`, `get_job_status`, `get_job_logs`, `list_jobs`, `cancel_job`
- Auth: Bearer token (API key or JWT) via `tokio::task_local!`. Per-tool ACL checks.

### Prometheus Metrics
- `crates/stroem-server/src/metrics.rs` — recorder install + `gather_gauges` + metric name constants (`STROEM_*`) + RED tower middleware (`track_http_metrics`)
- `crates/stroem-server/src/web/metrics.rs` — `GET /metrics` handler
- Always enabled. Optional config: `metrics: { public: bool }` (default `false` → requires `worker_token` Bearer, same auth posture as `/healthz/detail`).
- Hybrid recording: counters/histograms inline at event sites via `metrics::counter!` / `metrics::histogram!`; gauges sampled at scrape time in `gather_gauges` (2s timeout per DB query via `tokio::time::timeout`, errors logged + skipped, NOT zeroed — Prometheus treats absence as stale).
- DB queries inside `gather_gauges` run concurrently via `tokio::join!` so worst-case scrape latency is bounded at 2s, not 6s.
- RED middleware applied to `/api/*` only. `/worker`, `/hooks`, `/mcp` deliberately excluded (worker traffic would swamp user-facing signal).
- Job-completion counter lives in `run_terminal_job_actions` (not `handle_job_terminal`) — that's the single funnel called by `orchestrate_after_step`, `propagate_to_parent`, AND `handle_job_terminal`.
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
- Workspace-level hooks only fire for top-level jobs (`source_type`: api, user, trigger, webhook, retry)
- Recursion guard: `source_type = "hook"` → no further hooks
- Hook actions can be `type: task` — creates full child job instead of single-step hook job

### Connections
- Named, typed objects storing external system configs (DB creds, API endpoints)
- `ConnectionTypeDef` — property schema. `ConnectionDef` — optional `connection_type` + flattened values
- When `InputFieldDef.field_type` is not a primitive, it's a connection type reference — resolved to full values object
- Untyped connections skip type validation but still work as task inputs

### Agent Actions (type: agent — LLM Calls)
- Worker-side dispatch. Workers need `"agent"` tag and `agents:` config with LLM provider API keys.
- 19 providers via `rig-core`. `prompt` and `system_prompt` are Tera templates.
- **Structured output**: `OutputDef::to_json_schema()` → JSON Schema injected into system prompt
- **Multi-turn** (Phase 7B+C): `tools: [{task: "..."}, {mcp: "..."}]` — task tools create async child jobs, MCP tools call external servers sync
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
- `compute_depth()` max 10 levels. Child propagation via `propagate_to_parent()`.
- Self-referencing task actions rejected by validation.

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
- **`source_job_id`** + **`source_type = 'restart'`** + **`restart_from_step`** — *reserved* for the Restart-from-failed-step feature.
- **`raw_input`** — verbatim user submission stored on every job, before `merge_defaults` and `resolve_connection_inputs`. Returned by `GET /api/jobs/{id}` with workspace-defined secret values redacted to `••••••`. NULL for jobs created before migration `032_job_raw_input_and_lineage.sql`. **Redaction limitation:** the existing `redact_response` only matches values listed in `workspace.secrets`; user-typed secret values not present in the workspace config are stored and returned as plain text (same exposure as the existing `job.input` column — pre-existing limitation, not introduced by Re-run prefill).

### Health Check
- `GET /healthz` — unauthenticated. Checks DB, scheduler liveness, recovery sweeper liveness.
- `AliveGuard` drop guards set flags true on creation, false on drop.
- Returns 200 (ok), 503 (degraded/unhealthy). Helm probes use `/healthz`.

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
- Upload spawned after hooks fire (includes server events). Read fallback: local → legacy .log → archive.
- Config: `archive` (preferred) or `s3` (legacy) in `log_storage` section.
- **Server events**: `append_server_log()` writes `step: "_server"` entries for hook failures, orchestration errors, recovery timeouts.

### WebSocket Log Streaming
- `GET /api/jobs/{id}/logs/stream` — backfill on connect, then live via `tokio::sync::broadcast`

### Task Duration Stats
- `GET /api/workspaces/{ws}/tasks/{name}/stats?limit=50` — p50/p95/avg/min/max + recent durations + per-step breakdown over last N **completed** runs (View permission sufficient). Failed/cancelled runs excluded; `for_each` instance rows excluded from per-step breakdown.
- Queries: `JobRepo::get_task_duration_stats` / `get_recent_durations` and `JobStepRepo::get_step_duration_stats_for_task` use Postgres `percentile_cont(...)` directly.
- UI: `<DurationInsightsCard>` on Task Detail page; ETA / overrun pill on Job Detail page driven by `lib/eta.ts::computeEta`; per-step `p50: Xs` badges in `<StepTimeline>`.

### Worker Recovery
- `recovery.rs` — sweeper with 4 phases: (1) stale workers → fail steps, (2) timed-out steps, (3) timed-out jobs, (4) unmatched ready steps
- Config: `heartbeat_timeout_secs` (120), `sweep_interval_secs` (60), `unmatched_step_timeout_secs` (30)
- Data retention: optional `retention` section with `worker_hours`, `job_days`
- Strategy: fail, don't retry — avoids non-idempotent side effects
- HA: gated on `state.leader.is_leader()`. Followers run the loop but skip sweeps.

### High Availability (multi-replica server)
- **Leader election**: `leader.rs` — one server replica holds a Postgres advisory lock (`pg_try_advisory_lock(0x5354524D4C445201)` = "STRMLDR" + version byte). Lock is held on a dedicated `PgConnection`, released automatically when the connection drops (pod restart, network partition, DB restart). `AppState.leader.is_leader()` is checked at the top of `scheduler.rs`, `event_source.rs`, `recovery.rs` ticks. Default for tests/single-replica: `LeaderElection::always()`.
- **Cross-replica event bus**: `events.rs` — Postgres `LISTEN/NOTIFY` over `sqlx::postgres::PgListener`. Three channels: `stroem_job_cancelled` (job cancel → all replicas' `cancelled_jobs` cache), `stroem_workspace_reloaded` (revision change → peers re-read workspace cache), `stroem_job_log_chunk` (worker log push → peer WS subscribers). Payloads carry the originating replica's UUID; listeners drop self-emitted messages to avoid duplicate broadcasts.
- **Publishers** (one-line each, all best-effort, DB is source of truth):
  - `cancellation.rs` cancel_job — publishes `stroem_job_cancelled` after local insert.
  - `web/worker_api/jobs.rs` append_log — publishes `stroem_job_log_chunk` after local broadcast. Lines > `NOTIFY_MAX_BYTES` (7000) degrade to signal-only.
  - `workspace/mod.rs` watcher — publishes `stroem_workspace_reloaded` when source revision changes. `start_watchers(cancel, Some(event_bus))` opts in.
- **`/healthz`**: `web/health.rs` — leader-aware. Process + DB checks always required. Scheduler/recovery/event_source liveness only failure-eligible on the leader; followers report `"follower"` and return 200. Adds `checks.leader: bool`.
- **Replica id**: generated per-process (UUID v4) in `main.rs`, passed into `EventBus::new` for self-filtering.
- **Config**: `config::log_ha_diagnostics()` logs SHA-256 fingerprints of `auth.jwt_secret` + `auth.refresh_secret` at startup so operators can verify both pods loaded the same value.
- **Helm**: defaults at `server.replicas: 2`, `RollingUpdate` with `maxUnavailable: 0`, `terminationGracePeriodSeconds: 60`, `preStop sleep 10`, PodDisruptionBudget `minAvailable: 1`, topologySpreadConstraints by hostname. See `docs/src/content/docs/operations/high-availability.md`.
- Integration tests: `crates/stroem-server/tests/ha_test.rs` (leader uniqueness, failover, NOTIFY roundtrip per channel, oversize fallback, self-filter).

### React UI
- Pages: Login, Dashboard, Tasks, Task Detail, Jobs, Job Detail, Settings
- Auth-aware, SPA with react-router, embedded via rust-embed
- `ui/src/lib/api.ts` — token management. `ui/src/hooks/use-job-logs.ts` — WebSocket logs.

### Release Pipeline
- 5 platforms (linux-amd64/arm64, darwin-amd64/arm64, windows-amd64), 3 binaries = 15 assets
- Cross-compilation for linux-arm64 uses `cross`; others use native runners
- Multi-arch Docker images (amd64 + arm64) for server, worker, runner
- Release Dockerfiles COPY pre-built binaries using `TARGETARCH` arg
