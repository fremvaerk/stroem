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
# Install dependencies
cd ui && bun install

# Dev server (proxy to backend on :8080)
bun run dev

# Build (outputs to crates/stroem-server/static/)
bun run build

# Playwright E2E tests (needs backend running)
bunx playwright test

# Playwright E2E in Docker
docker compose -f docker-compose.yml -f docker-compose.test.yml \
  up --build --abort-on-container-exit playwright
```

### Documentation (docs/)

```bash
# Install dependencies
cd docs && bun install

# Dev server
bun run dev

# Build static site (also regenerates llms.txt)
bun run build

# Regenerate llms.txt only
bun run generate-llms

# Preview built site
bun run preview
```

### LLM Reference (llms.txt)

- `docs/public/llms.txt` is auto-generated from doc sources by `docs/scripts/generate-llms-txt.ts`
- Runs automatically before every `bun run build` in `docs/`
- Contains workflow authoring reference (actions, tasks, triggers, hooks, templating, secrets, examples)
- Served at `/llms.txt` on the docs site — paste into any LLM to generate valid Strøm workflow YAML
- To add/remove sections, edit the `sections` array in the generator script

## Key Patterns

### Workflow YAML structure
See `docs/internal/stroem-v2-plan.md` Section 2 for the full YAML format.

### Action Types and Runners
- **`agent`** (AI agent actions): Calls an LLM provider (Anthropic, OpenAI) with a rendered prompt. Server-side dispatch, no worker involved. Supports structured output via `output` (converted to JSON Schema at dispatch time).
- **`docker` / `pod`** (container actions): Runs user's prepared image as-is, no workspace mounting. Uses `cmd` field for entrypoint/command override.
- **`script`** (script actions): `type: script` + `runner: local|docker|pod` — scripts in a runner environment with workspace files. Supports multiple languages via the `language` field: `shell` (default), `python`, `javascript`, `typescript`, `go`. Inline scripts use `script` field; file paths use `source` field. Optional `dependencies` (package list) and `interpreter` (override auto-detected binary) fields.
- **`task`** (sub-job): References another task, server creates a child job (see Task Actions below)
- `type: script` + `image` is **rejected** by validation (breaking change). Use `type: docker` or `type: script` + `runner: docker` instead.
- **Toolchain preferences**: `uv > python3 > python`, `bun > node` (JS), `bun > deno` (TS), `bash > sh`
- **DB migration**: `014_script_type.sql` renames existing `'shell'` action_type rows to `'script'`
- **Pod manifest overrides**: `type: pod` and `type: script` + `runner: pod` support a `manifest` field — a raw JSON/YAML object deep-merged into the generated pod spec (service accounts, node selectors, tolerations, resource limits, annotations, sidecars, etc.). See `docs/src/content/docs/guides/action-types.md` for details.

### Runner Architecture
- `RunConfig` carries `action_type`, `image`, `runner_mode`, `runner_image`, `entrypoint`, `command`. For script actions, command is derived from `script` or `source` fields; for docker/pod, `cmd` field overrides defaults.
- `RunnerMode` enum: `WithWorkspace` (script actions) or `NoWorkspace` (docker/pod actions)
- `StepExecutor::select_runner()` dispatches on `(action_type, runner_field)`:
  - `("script", "local")` → ShellRunner
  - `("script", "docker")` or `("docker", _)` → DockerRunner
  - `("script", "pod")` or `("pod", _)` → KubeRunner
- **DockerRunner** (`crates/stroem-runner/src/docker.rs`): Uses `bollard` crate. Dual mode: `WithWorkspace` bind-mounts workspace at `/workspace:ro`; `NoWorkspace` runs image standalone with optional entrypoint/command.
- **KubeRunner** (`crates/stroem-runner/src/kubernetes.rs`): Uses `kube` + `k8s-openapi`. Dual mode: `WithWorkspace` has init container + workspace volume; `NoWorkspace` runs image directly.
- All runners enabled by default via cargo features (`docker`, `kubernetes` on stroem-runner/stroem-worker)
- Worker config: optional `docker` and `kubernetes` sections, plus `tags` and `runner_image` in `worker-config.yaml`
- **Startup scripts**: Worker and runner images use `docker/entrypoint.sh` which sources `*.sh` from `/etc/stroem/startup.d/` before the main process. DockerRunner always bind-mounts this path (WithWorkspace mode). KubeRunner injects a ConfigMap volume when `runner_startup_configmap` is set in worker config. Helm chart provides `startupScript`, `worker.startupScript`, and `runner.startupScript` values.

### Tags and Step Claiming
- Workers declare `tags` — e.g. `["script", "docker", "gpu", "agent"]`
- Steps compute `required_tags` from action type/runner + explicit tags
- Agent steps require `"agent"` tag (computed by `compute_required_tags("agent")`)
- Claim SQL: `required_tags <@ worker_tags::jsonb` (all required tags must be in worker's tag set)
- GIN index on `job_step.required_tags` for efficient containment queries

### Multi-Workspace
- Server config uses `workspaces:` map with named entries (folder or git source)
- `WorkspaceSource` trait in `crates/stroem-server/src/workspace/mod.rs` with `FolderSource` and `GitSource` impls
- `GitSource` tests use local bare repos (`file://` URL) via `git2` — see `create_bare_repo` / `add_commit` helpers. Tests require `#[tokio::test(flavor = "multi_thread")]` due to `block_in_place` in `load()`.
- `WorkspaceManager` holds all workspace entries, provides `get_config(name)`, `get_path(name)`, `get_revision(name)`
- **Error recovery**: Workspaces that fail initial load get placeholder entries with `load_error` field (`std::sync::RwLock<Option<String>>`). `get_config()`/`get_path()` return `None` for errored workspaces. Watchers retry `load()` on each poll cycle; on success the error is cleared and config becomes available. Source construction failures (`GitSource::new()`) still go to `load_errors` (no source object to retry with).
- Smart polling: `start_watchers()` uses each source's `poll_interval_secs()` and `peek_revision()` to detect changes cheaply before doing full reloads. Errored entries skip the peek optimization and always attempt `load()`.
- `FolderSource`: polls every 30s, `peek_revision()` hashes file metadata+content
- `GitSource`: polls every `poll_interval_secs` (default 60), `peek_revision()` uses ls-remote (via `git2::Remote::connect_auth` + `list`) to check remote HEAD without fetching objects. `block_in_place` wraps the blocking network call.
- API routes are workspace-scoped: `/api/workspaces/{ws}/tasks/{name}/execute`
- Workers download workspace tarballs via `GET /worker/workspace/{ws}.tar.gz` with ETag caching
- `WorkspaceCache` in worker uses immutable revision-based directories: `{base_dir}/{workspace}/{revision}/` with a `.current` file tracking the active revision. Multiple steps share the same revision dir read-only. `WorkspaceGuard` (RAII ref-counted) keeps revision directories alive during step execution, preventing cleanup from deleting in-use directories. Old revisions cleaned up lazily via `cleanup_old_revisions()`. Config: `max_retained_revisions` (default 2).

### Libraries (Actions, Tasks, Connection Types)
- Libraries import shared actions, tasks, and connection types from Git repos or local folders
- Defined centrally in `server-config.yaml` (`libraries:` + `git_auth:` sections), shared across all workspaces
- Namespace separator: `.` (dot) — e.g. `common.slack-notify`. Avoids URL routing conflicts.
- `LibraryDef` enum: `Folder { path }` or `Git { url, git_ref, auth }` in `config.rs`
- `LibraryResolver` in `crates/stroem-server/src/workspace/library.rs` — clones/loads, prefixes items, rewrites internal references
- During import: actions, tasks, connection types are prefixed; triggers, secrets, connections are ignored
- Internal reference rewriting: action refs in flow steps, `type: task` refs, hook actions, connection-type input field types
- `WorkspaceManager` resolves libraries once at startup, merges into every workspace config after loading
- Tarballs include library source files under `_libraries/{lib_name}/`
- Worker resolves library action scripts against `_libraries/{lib_name}/` (in `executor.rs`)
- CLI `stroem validate` skips `.`-containing names with a warning; server validates fully after resolution
- `validate_workflow_config_with_libraries()` for server-side validation (all refs must exist)

### Scheduler (Cron Triggers)
- `crates/stroem-server/src/scheduler.rs` — background task that fires cron triggers
- `crates/stroem-server/src/job_creator.rs` — shared job+step creation used by both API handler and scheduler
- Uses `croner` crate for cron parsing (supports 5-field and 6-field with seconds via `with_seconds_optional()`)
- Smart sleep: wakes only at next fire time, no fixed polling interval
- Config hot-reload: picks up workspace changes on each cycle, preserves `last_run`/`next_run` for unchanged triggers
- Clean shutdown via `tokio_util::sync::CancellationToken` (SIGINT/SIGTERM)
- Jobs created by triggers have `source_type = "trigger"`, `source_id = "{workspace}/{trigger_name}"`
- Cron validation at YAML parse time in `validation.rs` (CLI `validate` catches bad expressions)
- **Timezone support**: Optional `timezone` field (IANA name, e.g., `"Europe/Copenhagen"`) on `TriggerDef::Scheduler`. Defaults to UTC. Uses `chrono-tz` crate. Scheduler converts to local time for cron matching, then back to UTC. DST handled by `croner`. Parsed `Tz` stored in `TriggerState` to avoid re-parsing. Hot-reload resets state when timezone changes.
- **Concurrency policy**: `ConcurrencyPolicy` enum (`Allow`/`Skip`/`CancelPrevious`) on `TriggerDef::Scheduler`
  - `skip`: `count_active_by_source() > 0` → create a `skipped` job record (for visibility) and skip execution
  - `cancel_previous`: `get_active_by_source()` → cancel all active → create new job
  - `allow`: (default) no check

### Timeouts
- **Step timeout**: `FlowStep.timeout: Option<HumanDuration>` — kills step after duration (max 24h)
- **Task/job timeout**: `TaskDef.timeout: Option<HumanDuration>` — cancels entire job after duration (max 7d)
- `HumanDuration` in `crates/stroem-common/src/duration.rs` — parses `"30s"`, `"5m"`, `"1h30m"`, or plain integer (seconds)
- DB: `timeout_secs INTEGER` nullable columns on `job` and `job_step` tables (migration `012_timeouts.sql`)
- **Server-side enforcement**: `recovery.rs` sweep Phase 2 (`get_timed_out_steps`) and Phase 3 (`get_timed_out_jobs`)
- **Worker-side enforcement**: `poller.rs` wraps step execution in `tokio::time::timeout` with abort handle
- `ClaimResponse` includes `timeout_secs` so workers know the step's timeout

### Conditional Flow Steps (`when`)
- `FlowStep.when: Option<String>` — Tera template expression evaluated at step promotion time
- `evaluate_condition()` in `template.rs`: renders template, result is truthy if non-empty and not `"false"` or `"0"`
- Condition-false steps marked `skipped`; cascade-skip handled in `promote_ready_steps()` (all-deps-skipped rule)
- Skipped deps count as satisfied by default — convergence works without `continue_on_failure`
- All-deps-skipped rule: if ALL deps are skipped (none completed), step is cascade-skipped
- Root steps with `when` (no deps) start as `pending`; evaluated in post-creation promote loop in `job_creator.rs`
- Condition evaluation errors → step fails with error message (not silently skipped)
- `build_step_render_context()` includes skipped steps with `{ "output": null }` so downstream `when` expressions get falsy values
- DB: `when_condition TEXT` column on `job_step` (migration `018_when_conditions.sql`)
- Validation: Tera syntax checked at YAML parse time in `validation.rs`
- `orchestrator::on_step_completed()` takes optional `&WorkspaceConfig` for building render context with secrets
- UI: "condition" badge on skipped conditional steps, "when" badge on active ones, `when` expression shown in task detail

### For-Each Loops (`for_each`)
- `FlowStep.for_each: Option<serde_json::Value>` — Tera template string or literal JSON array
- `FlowStep.sequential: bool` — when true, instances run one at a time (default: parallel)
- **Expansion**: `expand_for_each_steps()` in `job_creator.rs` — evaluates expression, creates N instance steps (`step[0]`, `step[1]`, ...)
- **Placeholder lifecycle**: pending → (deps met) → `expand_for_each_steps()` creates instances → placeholder marked `running` → all instances complete → placeholder marked `completed`/`failed`
- **Instance steps**: `loop_source` = placeholder name, `loop_index` = 0..N, `loop_total` = N, `loop_item` = array element
- **`each` variable**: injected at claim time (`rendering.rs`) and in `handle_task_steps()` — `each.item` + `each.index`
- **Sequential mode**: instance `[0]` starts ready, others pending; `check_loop_completion()` promotes `[i+1]` after `[i]` completes
- **Output aggregation**: `check_loop_completion()` collects outputs ordered by index into an array on the placeholder
- **Fan-in**: downstream steps depend on placeholder name, see `{{ step.output }}` as aggregated array
- **`when` + `for_each`**: `when` evaluated first; if falsy, step skipped without expansion
- **`type: task` + `for_each`**: each instance creates a child sub-job via `handle_task_steps()`
- **Flow step lookup fallback**: `loop_source` used when instance name not in `task.flow` (rendering.rs, jobs.rs)
- DB: migration `022_for_each.sql` adds `for_each_expr`, `loop_source`, `loop_index`, `loop_total`, `loop_item` to `job_step`
- Validation: Tera syntax check, bracket-free step names, max 10000 items, sequential-without-for_each warning
- `promote_ready_steps()` skips for_each placeholders (handled by expansion logic)
- Orchestrator: `check_loop_completion()` called before `on_step_completed()`, `expand_for_each_steps()` called after
- Empty array → step skipped; non-array → step fails; instance failure → placeholder fails (unless `continue_on_failure`)

### MCP Server (Model Context Protocol)
- Feature-gated: `mcp` cargo feature on `stroem-server` (enabled by default alongside `s3`)
- Config: `mcp: { enabled: true }` in `server-config.yaml` (disabled by default)
- Endpoint: `/mcp` via Streamable HTTP transport (stateless, JSON response mode)
- Crate: `rmcp` v1.2 (official Rust MCP SDK) with `macros` + `transport-streamable-http-server` features
- `build_mcp_routes()` in `mcp/mod.rs` returns an Axum `Router` with auth middleware wrapping `StreamableHttpService`
- `StromMcpHandler` in `mcp/handler.rs`: `ServerHandler` impl with `#[tool_handler]` macro, holds `Arc<AppState>` + `Option<McpAuthContext>`
- 8 tools defined via `#[tool_router]` / `#[tool]` macros in `mcp/tools.rs`
- Tools: `list_workspaces`, `list_tasks`, `get_task`, `execute_task`, `get_job_status`, `get_job_logs`, `list_jobs`, `cancel_job`
- **Auth middleware**: `tokio::task_local!` passes `Option<McpAuthContext>` from Axum middleware to handler factory. Validates Bearer token (API key `strm_` or JWT). Returns 401 when auth is configured and token is missing/invalid.
- **Per-tool ACL**: Each tool checks permissions via `check_task_acl()` or `resolve_acl_scope()` from `mcp/auth.rs`. List tools filter by ACL scope; mutation tools (execute, cancel) require `Run` permission; read tools (get_task, get_job_status, get_job_logs) require `View` or higher; `Deny` returns "not found".
- `Parameters<T>` wrapper from `rmcp::handler::server::wrapper::Parameters` for tool input deserialization
- Jobs created by MCP tools have `source_type = "mcp"`, `source_id` = user email (audit trail)
- `CancellationToken` passed from `main.rs` through `build_router()` for graceful shutdown

### Webhook Triggers
- `TriggerDef` is a tagged enum (`#[serde(tag = "type")]`) with `Scheduler` and `Webhook` variants
- Accessor methods: `task()`, `input()`, `enabled()`, `trigger_type_str()`
- `crates/stroem-server/src/web/hooks.rs` — webhook HTTP handler (GET+POST `/hooks/{name}`)
- Not under `/api/` — avoids user auth middleware. Auth: optional `secret` field on trigger
- Secret validation: `?secret=xxx` query param or `Authorization: Bearer xxx` header
- Input mapping: `body` (parsed JSON or raw string), `headers`, `method`, `query` + YAML `input` defaults
- Jobs created by webhooks have `source_type = "webhook"`, `source_id = "{workspace}/{trigger_key}"`
- Webhook name validation: must match `^[a-zA-Z0-9_-]+$` (URL-safe)
- Duplicate webhook names across workspaces: first match wins at dispatch time (no startup validation)
- **Sync/async mode**: `mode: "sync"` waits for job completion before responding (default: `"async"`, fire-and-forget)
- `timeout_secs`: max wait in sync mode (default 30, max 300). On timeout, returns 202 with `"status": "running"`
- `JobCompletionNotifier` (`crates/stroem-server/src/job_completion.rs`): per-job broadcast channels, notified from `handle_job_terminal()` and `orchestrate_after_step()`

### Hooks (on_success / on_error)
- `HookDef` struct in `crates/stroem-common/src/models/workflow.rs` — `action` + `input` map
- `TaskDef` has `on_success: Vec<HookDef>` and `on_error: Vec<HookDef>` (default empty)
- **Workspace-level hooks**: `WorkflowConfig` and `WorkspaceConfig` also have `on_success`/`on_error` fields — act as fallback defaults when a task has no hooks defined for that event type. Only fire for top-level jobs (`source_type` is `api`, `user`, `trigger`, or `webhook`). Evaluated independently per event type.
- `crates/stroem-server/src/hooks.rs` — `fire_hooks()` builds `HookContext`, renders input through Tera, creates single-step hook jobs
- Recursion guard: jobs with `source_type = "hook"` never trigger further hooks
- Hook jobs: `task_name = "_hook:{action}"`, `source_type = "hook"`, `source_id = "{ws}/{task}/{job_id}/{hook_type}[idx]"`
- Validation in `validation.rs` — hook action references must exist (or be library actions with `/`), both task-level and workspace-level
- Migration `005_hooks.sql` adds `'hook'` to `source_type` CHECK constraint
- Hook actions can be `type: task` — creates a full child job instead of a single-step hook job

### Connections
- Named, typed objects storing external system configs (DB creds, API endpoints)
- `ConnectionTypeDef` — property schema: `property_type`, `required`, `default`, `secret`
- `ConnectionDef` — `connection_type: Option<String>` + `#[serde(flatten)] values: HashMap<String, Value>`
- `WorkflowConfig` and `WorkspaceConfig` have `connection_types` and `connections` fields
- `render_connections()` on `WorkspaceConfig`: Phase 1 renders template values, Phase 2 applies type defaults
- Called in `folder.rs` after `render_secrets()` during workspace loading
- **Connection input resolution**: When `InputFieldDef.field_type` is not a primitive (`string`/`integer`/`number`/`boolean`), it's a connection type reference
- `resolve_connection_inputs()` in `template.rs`: looks up connection name string → replaces with full values object
- Called in `job_creator.rs` after `merge_defaults()` before job creation
- Validation: property types, type references, required fields, unknown field warnings, connection input references
- Untyped connections (no `type` field) skip type validation but still work as task inputs

### Agent Actions (type: agent — LLM Calls)
- `type: agent` — LLM call as a workflow step, worker-side dispatch. Workers with `"agent"` tag claim and execute agent steps.
- **Worker requirements**: Workers need `tags: ["script", "agent"]` (or at least `"agent"`) and an `agents:` section in `worker-config.yaml` with LLM provider API keys
- **Server config** (validation-only): Optional `agents.providers` in `server-config.yaml` for provider name validation at YAML load time. Server does NOT store API keys.
- Supports 19 providers: anthropic, azure, cohere, deepseek, galadriel, gemini, groq, huggingface, hyperbolic, llamafile, mira, mistral, moonshot, ollama, openai, openrouter, perplexity, together, xai
- `ActionDef` fields: `provider`, `model`, `system_prompt`, `prompt` (Tera templates), `output` (OutputDef, converted to JSON Schema at dispatch), `temperature`, `max_tokens`
- `prompt` and `system_prompt` are Tera templates rendered at claim time with standard context (input, step outputs, secrets)
- **Structured output**: when `output` is set, `OutputDef::to_json_schema()` produces a JSON Schema injected into the system prompt; response is parsed as JSON; output includes `_meta` with model, provider, tokens, latency
- Worker-side dispatch: `crates/stroem-agent/src/dispatch.rs` in `stroem-agent` crate, executed on worker after step claim
- At claim time, server sends `provider` name only; worker looks up API key from its local `agents:` config
- Uses `rig-core` (v0.33) for LLM provider abstraction (19 providers, custom endpoints)
- Feature-gated: `agent` cargo feature on stroem-worker (enabled by default)
- DB: migration `023_agent_type.sql` adds `'agent'` to action_type CHECK + `agent_state` JSONB column
- **Multi-turn dispatch** (Phase 7B+C): When `tools` is non-empty or `interactive: true`, uses custom dispatch loop in `stroem-agent` with rig's `CompletionModel::completion()` for raw LLM calls
- **Tool types**: `tools: [{task: "task-name"}, {mcp: "server-name"}]` — task tools create child jobs (async via server endpoint), MCP tools call external servers (sync)
- **ask_user**: `interactive: true` enables the `ask_user` tool — worker suspends step, user approves via server endpoint, worker re-claims step to resume
- **Conversation state**: `AgentConversationState` persisted as JSONB in `agent_state` column, restored on worker re-claim
- **MCP client**: MCP servers (stdio, SSE) spawn on worker, not server. `stroem-agent` connects to configured MCP servers via stdio or SSE (Streamable HTTP) transport, discovers tools, calls them synchronously
- **MCP server config**: `mcp_servers:` in workspace YAML — `McpServerDef` with `type: stdio|sse`, `command`, `args`, `env`, `url`
- **Task tool flow**: Worker's LLM calls task tool → worker creates child job via server endpoint, saves conversation state, releases step → child completes → server marks step ready → worker re-claims and resumes dispatch loop
- **ask_user flow**: Worker's LLM calls `ask_user` → step suspended → user approves via `POST /api/jobs/{id}/steps/{step}/approve` → server marks ready → worker re-claims and resumes dispatch
- **Max turns**: `max_turns` safety limit (default 25, max 100) prevents unbounded loops
- **Server role**: Server handles orchestration (child job creation for task tools, suspension/approval, hook firing). Workers handle LLM dispatch and tool invocation.
- DB: migration `025_agent_tool_source.sql` adds `'agent_tool'` to source_type CHECK

### Approval Gates (type: approval — Human-in-the-Loop)
- `type: approval` — pauses execution for human approve/reject, server-side dispatch (like `type: task`)
- `ActionDef.message: Option<String>` — Tera template rendered at suspension time, shown to approver
- `StepStatus::Suspended` — new step status for steps waiting for human input
- `handle_approval_steps()` in `job_creator.rs` — renders message, marks step `suspended`, stores rendered message in output
- `POST /api/jobs/{id}/steps/{step}/approve` — approve (`approved: true`) or reject with `rejection_reason`
- Approve output: `{ "approved": true, "approved_by": "email", "approved_at": "ISO8601", "input": {...} }`
- Reject: step fails, downstream steps cascade-skip, `on_error` hooks fire
- `on_suspended` hooks: `TaskDef.on_suspended` and workspace-level fallback, fire when step enters `suspended`
- `SuspendedHookContext`: `workspace`, `task_name`, `job_id`, `step_name`, `message`
- Recovery: `get_timed_out_suspended_steps()` in sweep Phase 2.5 — fails steps past their `timeout`
- Cancellation: `cancel_pending_steps()` includes `'suspended'` status
- Workers never claim approval steps (filtered in claim SQL alongside `task`/`agent`)
- DB: migration `024_approval_gates.sql` adds `'suspended'` to status CHECK, `'approval'` to action_type CHECK, `suspended_at` column
- Frontend: `ApprovalCard` component with message display, form fields, approve/reject buttons

### Task Actions (type: task — Sub-Job Execution)
- `ActionDef.task: Option<String>` — references another task by name
- `type: task` actions cannot have `cmd`, `script`, `source`, `image`, `runner`, or `language`
- Server-side dispatch: workers never claim task-type steps (filtered in claim SQL `action_type != 'task'`)
- `job_creator.rs` — `handle_task_steps()` finds ready task steps, renders input, creates child jobs
- `create_job_for_task_inner()` uses `Box::pin` for recursive async (task → child task → grandchild task)
- `compute_depth()` walks parent chain — max 10 levels (`MAX_TASK_DEPTH`)
- DB: `parent_job_id` + `parent_step_name` columns on `job` table (migration `006_task_actions.sql`)
- `propagate_to_parent()` in `complete_step` handler: child completes → parent step updated → parent orchestrated → recurse up chain
- Child jobs: `source_type = "task"`, `source_id = "{parent_job_id}/{step_name}"`
- Validation: self-referencing task actions rejected (direct and via hooks)

### Config Loading (env var overrides)
- Both server and worker use the `config` crate to load YAML + env var overrides
- `config::load_config()` in `stroem-server/src/config.rs`, `load_config()` in `stroem-worker/src/config.rs`
- Env vars prefixed with `STROEM__` override YAML values; `__` is the separator for nested keys
- Example: `STROEM__DB__URL` overrides `db.url`, `STROEM__WORKER_TOKEN` overrides `worker_token`
- Helm chart uses this pattern: ConfigMap has full YAML config, secrets injected via `extraSecretEnv` as `STROEM__` env vars

### Database
- Runtime sqlx queries, NOT compile-time checked
- Migrations in `crates/stroem-db/migrations/`
- Job claiming uses `SELECT ... FOR UPDATE SKIP LOCKED`
- **Job revision tracking**: `job.revision` stores the workspace revision (git SHA or folder content hash) at job creation time. Sub-jobs inherit their parent's revision. Hook jobs inherit the originating job's revision. Looked up via `WorkspaceManager::get_revision()`.

### Health Check
- `GET /healthz` — unauthenticated endpoint, not under `/api/`
- Checks: DB connectivity (`SELECT 1`), scheduler liveness, recovery sweeper liveness
- `BackgroundTasks` struct in `state.rs` with `Arc<AtomicBool>` flags for scheduler/recovery
- `AliveGuard` drop guard in `scheduler.rs` and `recovery.rs` — sets flag true on creation, false on drop (handles panics)
- Response: `{"status": "ok"|"degraded"|"unhealthy", "checks": {"db": "ok"|"error", "scheduler": "ok"|"stopped", "recovery": "ok"|"stopped"}}`
- Returns 200 when all healthy, 503 otherwise. "degraded" = DB ok but background task stopped
- Helm chart probes configured to use `/healthz`

### Error Handling (AppError)
- `AppError` enum in `web/error.rs` — centralized API error type implementing `IntoResponse`
- Variants: `BadRequest(String)`, `Unauthorized(String)`, `Forbidden(String)`, `NotFound(String)`, `Conflict(String)`, `Internal(anyhow::Error)`
- `Internal` variant logs full error via tracing, returns generic "Internal server error" to client (sanitization)
- `From<anyhow::Error>` and `From<sqlx::Error>` impls enable `?` operator in handlers
- All API handlers (`web/api/`, `web/worker_api/`, `web/hooks.rs`) migrated to return `Result<impl IntoResponse, AppError>`
- Helper: `AppError::not_found("Entity")` → 404 with "Entity not found"
- Response format: `{"error": "message"}` (same shape as before, frontend compatible)

### Authentication
- **User auth**: Optional JWT-based auth (access tokens 15min, refresh tokens 30 day with rotation)
- Auth is enabled by adding an `auth` section to `server-config.yaml`
- Handlers use `AuthUser` extractor for protected endpoints; handlers without it remain open
- Password hashing: argon2id via the `argon2` crate
- **Worker auth**: Bearer token from config (`worker_token`)
- **API keys**: Long-lived tokens for programmatic/CI access, tied to a user
  - Format: `strm_` prefix + 32 random hex chars (37 chars total), SHA256 hash stored in DB
  - `require_auth` middleware detects `strm_` prefix to distinguish from JWTs
  - CRUD: `POST/GET /api/auth/api-keys`, `DELETE /api/auth/api-keys/{prefix}` (JWT auth required)
  - Optional expiry (`expires_in_days`), `last_used_at` updated in background on each use
  - DB: `api_key` table (migration `008_api_keys.sql`), `ApiKeyRepo` in `stroem-db`
  - Frontend: Settings page (`/settings`) with create/list/revoke UI
- **OIDC SSO**: Authorization Code with PKCE flow via `openidconnect` crate
  - Config: add `provider_type: "oidc"` providers with `issuer_url`, `client_id`, `client_secret`, `display_name`
  - Requires `base_url` in auth config for redirect URI construction
  - OIDC discovery at startup (`CoreProviderMetadata::discover_async`)
  - State stored in signed HttpOnly cookie (JWT with PKCE verifier, nonce, CSRF state)
  - JIT user provisioning: find by auth_link → find by email → create new user (in `oidc::provision_user`)
  - Routes: `GET /api/auth/oidc/{provider}` (start) and `GET /api/auth/oidc/{provider}/callback`
  - After callback, issues internal JWT tokens and redirects to `/login/callback#access_token=...&refresh_token=...`

### ACL (Access Control)
- **Config-driven**: Optional `acl` section in `server-config.yaml` with `default` action and `rules` list
- **No ACL config = backward compat**: everything open, all authenticated users have full access
- **Admin flag**: `is_admin` boolean on user table; admins bypass all ACL checks
- **Groups**: Named sets of users stored in `user_group` DB table, managed by admins via API
- **Rule evaluation**: All matching rules checked, **highest permission wins** (Run > View > Deny). Order doesn't matter.
- **Glob matching**: Simple `*` wildcard for workspace and task patterns
- **Task path**: `"{folder}/{task_name}"` or `"{task_name}"` when no folder — used for task pattern matching
- **Permission levels**: `Run` (full access), `View` (read-only, can see but not execute/cancel), `Deny` (invisible)
- **`AclEvaluator`** in `crates/stroem-server/src/acl.rs`: `evaluate()`, `allowed_scope()`, `is_configured()`
- **Helper**: `load_user_acl_context()` loads user groups from DB, returns `(is_admin, HashSet<String>)`
- DB: Migration `017_acl.sql` adds `is_admin` column and `user_group` table
- `Claims.is_admin` uses `#[serde(default)]` for backward compat with old JWTs
- Initial user (from config) always promoted to admin; first OIDC user becomes admin
- Admin-only endpoints: `PUT /api/users/{id}/admin`, `GET/PUT /api/users/{id}/groups`, `GET /api/groups`
- Workers and Users pages restricted to admin when ACL is enabled

### Log Storage
- `LogStorage` in `AppState` — local JSONL files for live buffering + optional pluggable archive backend
- **`LogArchive` trait** (`log_storage.rs`): `upload(&str, &[u8])`, `download(&str)`, `delete(&str)` — operates on raw bytes, gzip handled by `LogStorage`
- **`S3Archive`**: feature-gated behind `s3` cargo feature (`aws-sdk-s3` + `aws-config`). `from_config(ArchiveConfig)` for production, `from_client(client, bucket)` for tests.
- **`LocalArchive`**: always available, maps archive keys to files under a base path
- `LogStorage::with_archive(Arc<dyn LogArchive>, prefix)` attaches an archive backend
- `archive_key(prefix, job_id, meta)` free function builds structured keys: `{prefix}{workspace}/{task}/YYYY/MM/DD/YYYY-MM-DDTHH-MM-SS_{job_id}.jsonl.gz`
- **Gzip compression**: `upload_to_archive` compresses with `flate2::GzEncoder`, reads decompress with `GzDecoder`
- `upload_to_archive(job_id, meta)`, `delete_archive_log(job_id, meta)`, `get_log(job_id, meta)`, `get_step_log(job_id, step, meta)` all require `&JobLogMeta`
- Archive upload spawned as background task when a job reaches terminal state — **after** hooks fire, so server events are included
- Read fallback: local file → legacy .log → archive (if configured)
- **Config**: `LogStorageConfig` has `archive: Option<ArchiveConfig>` (new, preferred) and `s3: Option<S3Config>` (legacy, still supported). `effective_archive()` resolves: `archive` wins over `s3`.
- `ArchiveConfig`: flat struct with `type` discriminator (`"s3"` or `"local"`) + optional fields per backend
- S3 credentials via standard AWS chain (env vars, IAM role, `~/.aws/credentials`)
- **Server events**: `AppState::append_server_log()` writes JSONL entries with `step: "_server"`, `stream: "stderr"` — makes server-side errors (hook failures, orchestration errors, recovery timeouts) visible in the job's log stream via UI and API (`GET /api/jobs/{id}/steps/_server/logs`)

### WebSocket Log Streaming
- `GET /api/jobs/{id}/logs/stream` -- WebSocket upgrade endpoint
- Sends existing log content (backfill) on connect, then streams live chunks
- Per-job broadcast channels via `tokio::sync::broadcast` in `LogBroadcast`

### Worker Recovery
- `crates/stroem-server/src/recovery.rs` — background sweeper that detects stale workers
- `crates/stroem-server/src/job_recovery.rs` — shared orchestration logic (used by both `complete_step` handler and recovery sweeper)
- Config: optional `recovery` section in `server-config.yaml` with `heartbeat_timeout_secs` (default 120), `sweep_interval_secs` (default 60), and `unmatched_step_timeout_secs` (default 30)
- Data retention: separate `retention` section with `worker_hours`, `job_days`, `interval_secs` (all optional, disabled by default)
- Always active with defaults when config section is absent (`#[serde(default)]`)
- Sweep cycle (4 phases): Phase 1: mark stale workers inactive → fail their running steps → orchestrate. Phase 2: fail steps that exceeded their timeout. Phase 3: cancel jobs that exceeded their timeout. Phase 4: fail ready steps with no matching active worker (after `unmatched_step_timeout_secs`).
- Worker heartbeat (`POST /worker/heartbeat`) also sets `status = 'active'`, so returning workers auto-reactivate
- `WorkerRepo::mark_stale_inactive()` uses `make_interval(secs => $1)` for threshold comparison
- `JobStepRepo::get_running_steps_for_workers()` finds stuck steps by worker ID
- `JobStepRepo::get_unmatched_ready_steps()` finds ready steps with no active worker matching `required_tags`
- `job_step.ready_at` column (migration `016_ready_at.sql`) tracks when a step became claimable — set by `promote_ready_steps()` and `create_steps_tx()` (for initially-ready steps)
- Failed steps get error message: `"Worker heartbeat timeout (worker {id} unresponsive)"` or `"No active worker with required tags to run this step"`
- Strategy: fail, don't retry — avoids data corruption from re-running non-idempotent steps
- Clean shutdown via `CancellationToken` (same pattern as scheduler)

### React UI
- Pages: Login, Dashboard, Tasks, Task Detail (with run dialog), Jobs, Job Detail (with live logs), Settings (API keys)
- Auth-aware: detects if server has auth enabled, shows login page accordingly
- SPA routing with react-router, embedded in Rust binary via rust-embed with SPA fallback
- `ui/src/lib/api.ts` handles token management (access token in memory, refresh in localStorage, auto-refresh on 401)
- `ui/src/hooks/use-job-logs.ts` WebSocket hook for live log streaming
- Playwright E2E tests in `ui/e2e/`, can run locally or in Docker

### Release Pipeline
- **Binary releases**: 5 platforms (linux-amd64, linux-arm64, darwin-amd64, darwin-arm64, windows-amd64) via `build-binaries` matrix job in `.github/workflows/release.yml`
- Cross-compilation for linux-arm64 uses `cross` (cargo cross-compilation tool); all others use native runners
- Asset naming: `stroem-{binary}-{target}.tar.gz` (unix) / `.zip` (windows) — 3 binaries x 5 platforms = 15 assets
- **Multi-arch Docker images**: amd64 + arm64 for server, worker, and runner images
- Release Dockerfiles (`docker/Dockerfile.{server,worker}.release`) skip Rust compilation — they COPY pre-built binaries using Docker Buildx `TARGETARCH` arg
- Runner image uses QEMU directly (no Rust, just apt packages + tool downloads)
- Build workflow (`build.yml`) stays amd64-only; multi-arch is release-only
