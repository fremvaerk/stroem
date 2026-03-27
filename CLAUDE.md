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
Phase 6: Shared storage & worker affinity.
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

### Tags and Step Claiming
- Workers declare `tags` — e.g. `["script", "docker", "gpu", "agent"]`
- Steps compute `required_tags` from action type/runner + explicit tags
- Claim SQL: `required_tags <@ worker_tags::jsonb` (GIN-indexed containment)

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

### MCP Server (Model Context Protocol)
- Feature-gated: `mcp` cargo feature (enabled by default). Config: `mcp: { enabled: true }` (disabled by default)
- Endpoint: `/mcp` via Streamable HTTP. Crate: `rmcp` with `#[tool_router]` / `#[tool]` macros
- 8 tools: `list_workspaces`, `list_tasks`, `get_task`, `execute_task`, `get_job_status`, `get_job_logs`, `list_jobs`, `cancel_job`
- Auth: Bearer token (API key or JWT) via `tokio::task_local!`. Per-tool ACL checks.

### Webhook Triggers
- `TriggerDef` tagged enum: `Scheduler`, `Webhook`, and `EventSource` variants
- Handler at `/hooks/{name}` (not under `/api/`). Auth: optional `secret` field (query param or Bearer header)
- Input mapping: `body`, `headers`, `method`, `query` + YAML `input` defaults
- **Sync/async mode**: `mode: "sync"` waits for completion (default: async). `timeout_secs` max wait (default 30, max 300).

### Event Source Triggers
- `TriggerDef::EventSource` variant: long-running queue consumer processes. Workers claim and run indefinitely per `restart_policy`.
- **Stdout protocol**: Each JSON line emitted to stdout (parsed as valid JSON) becomes a job input, merged with trigger `input` defaults. Non-JSON lines silently skipped.
- **Runner support**: `script` (local/docker/pod) or `image` + `runner` (docker/pod). Language, dependencies, interpreter, manifest fields supported like script actions.
- **RestartPolicy enum**: `Always` (default), `OnFailure`, `Never` — controls behavior when process exits.
- **Exponential backoff**: `backoff_secs` field (default 5) — doubled on consecutive failures, capped at 5 minutes. Resets on clean exit.
- **Backpressure**: `max_in_flight` field limits concurrent pending/running jobs. Worker pauses stdout reading when limit reached, creating natural Unix pipe backoff.
- **Worker requirements**: Workers claiming event sources must declare `event_source` tag and set `max_event_sources` config (default 1, max concurrent event source triggers per worker).
- **EventSourceManager**: Server-side background task creating/monitoring event source controller jobs. `POST /worker/event-source/emit` endpoint for workers to emit events.
- **Job tracking**: Created jobs have `source_type: "event_source"`, `source_id: "{workspace}/{trigger_name}"` for audit trail.

### Hooks (on_success / on_error)
- `HookDef`: `action` + `input` map. Task-level and workspace-level (fallback when task has none).
- Workspace-level hooks only fire for top-level jobs (`source_type`: api, user, trigger, webhook)
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

### Worker Recovery
- `recovery.rs` — sweeper with 4 phases: (1) stale workers → fail steps, (2) timed-out steps, (3) timed-out jobs, (4) unmatched ready steps
- Config: `heartbeat_timeout_secs` (120), `sweep_interval_secs` (60), `unmatched_step_timeout_secs` (30)
- Data retention: optional `retention` section with `worker_hours`, `job_days`
- Strategy: fail, don't retry — avoids non-idempotent side effects

### React UI
- Pages: Login, Dashboard, Tasks, Task Detail, Jobs, Job Detail, Settings
- Auth-aware, SPA with react-router, embedded via rust-embed
- `ui/src/lib/api.ts` — token management. `ui/src/hooks/use-job-logs.ts` — WebSocket logs.

### Release Pipeline
- 5 platforms (linux-amd64/arm64, darwin-amd64/arm64, windows-amd64), 3 binaries = 15 assets
- Cross-compilation for linux-arm64 uses `cross`; others use native runners
- Multi-arch Docker images (amd64 + arm64) for server, worker, runner
- Release Dockerfiles COPY pre-built binaries using `TARGETARCH` arg
