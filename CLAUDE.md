# Strøm v2 -- Development Guide

## Project Overview

Strøm is a workflow/task orchestration platform. Backend in Rust, frontend in React.
Phase 1 (MVP) complete: end-to-end workflow execution via API and CLI.
Phase 2a complete: JWT authentication backend + WebSocket log streaming.
Phase 2b complete: React UI with shadcn/ui, embedded in Rust binary via rust-embed.
Phase 3: Multi-workspace support, tarball distribution, Docker and Kubernetes runners.

## Architecture

- **stroem-common**: Shared types, models, DAG walker, Tera templating, validation
- **stroem-db**: PostgreSQL layer via sqlx (runtime queries), migrations, repositories
- **stroem-runner**: Execution backends (ShellRunner, DockerRunner via bollard, KubeRunner via kube). All runners enabled by default.
- **stroem-server**: Axum API server, orchestrator, multi-workspace manager (folder + git sources), log storage, embedded UI via rust-embed
- **stroem-worker**: Worker process: polls server, downloads workspace tarballs, executes steps, streams logs
- **stroem-cli**: CLI tool (validate, trigger, status, logs, tasks, jobs, workspaces)

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

# Build static site
bun run build

# Preview built site
bun run preview
```

## Key Patterns

### Workflow YAML structure
See `docs/internal/stroem-v2-plan.md` Section 2 for the full YAML format.

### Action Types and Runners (Type 1 / Type 2 Split)
- **Type 1 (Container)**: `type: docker` or `type: pod` — runs user's prepared image as-is, no workspace mounting
- **Type 2 (Shell)**: `type: shell` + `runner: local|docker|pod` — shell commands in a runner environment with workspace files
- **Type 3 (Sub-job)**: `type: task` — references another task, server creates a child job (see Task Actions below)
- `type: shell` + `image` is **rejected** by validation (breaking change). Use `type: docker` (Type 1) or `type: shell` + `runner: docker` (Type 2) instead.
- **Pod manifest overrides**: `type: pod` and `type: shell` + `runner: pod` support a `manifest` field — a raw JSON/YAML object deep-merged into the generated pod spec (service accounts, node selectors, tolerations, resource limits, annotations, sidecars, etc.). See `docs/src/content/docs/guides/action-types.md` for details.

### Runner Architecture
- `RunConfig` carries `action_type`, `image`, `runner_mode`, `runner_image`, `entrypoint`, `command`
- `RunnerMode` enum: `WithWorkspace` (Type 2) or `NoWorkspace` (Type 1)
- `StepExecutor::select_runner()` dispatches on `(action_type, runner_field)`:
  - `("shell", "local")` → ShellRunner
  - `("shell", "docker")` or `("docker", _)` → DockerRunner
  - `("shell", "pod")` or `("pod", _)` → KubeRunner
- **DockerRunner** (`crates/stroem-runner/src/docker.rs`): Uses `bollard` crate. Dual mode: `WithWorkspace` bind-mounts workspace at `/workspace:ro`; `NoWorkspace` runs image standalone with optional entrypoint/command.
- **KubeRunner** (`crates/stroem-runner/src/kubernetes.rs`): Uses `kube` + `k8s-openapi`. Dual mode: `WithWorkspace` has init container + workspace volume; `NoWorkspace` runs image directly.
- All runners enabled by default via cargo features (`docker`, `kubernetes` on stroem-runner/stroem-worker)
- Worker config: optional `docker` and `kubernetes` sections, plus `tags` and `runner_image` in `worker-config.yaml`
- **Startup scripts**: Worker and runner images use `docker/entrypoint.sh` which sources `*.sh` from `/etc/stroem/startup.d/` before the main process. DockerRunner always bind-mounts this path (WithWorkspace mode). KubeRunner injects a ConfigMap volume when `runner_startup_configmap` is set in worker config. Helm chart provides `startupScript`, `worker.startupScript`, and `runner.startupScript` values.

### Tags and Step Claiming
- Workers declare `tags` (replaces `capabilities`) — e.g. `["shell", "docker", "gpu"]`
- Steps compute `required_tags` from action type/runner + explicit tags
- Claim SQL: `required_tags <@ worker_tags::jsonb` (all required tags must be in worker's tag set)
- GIN index on `job_step.required_tags` for efficient containment queries
- Backward compatible: `capabilities` still works if `tags` not set

### Multi-Workspace
- Server config uses `workspaces:` map with named entries (folder or git source)
- `WorkspaceSource` trait in `crates/stroem-server/src/workspace/mod.rs` with `FolderSource` and `GitSource` impls
- `GitSource` tests use local bare repos (`file://` URL) via `git2` — see `create_bare_repo` / `add_commit` helpers. Tests require `#[tokio::test(flavor = "multi_thread")]` due to `block_in_place` in `load()`.
- `WorkspaceManager` holds all workspace entries, provides `get_config(name)`, `get_path(name)`, `get_revision(name)`
- Smart polling: `start_watchers()` uses each source's `poll_interval_secs()` and `peek_revision()` to detect changes cheaply before doing full reloads
- `FolderSource`: polls every 30s, `peek_revision()` hashes file metadata+content
- `GitSource`: polls every `poll_interval_secs` (default 60), `peek_revision()` uses ls-remote (via `git2::Remote::connect_auth` + `list`) to check remote HEAD without fetching objects. `block_in_place` wraps the blocking network call.
- API routes are workspace-scoped: `/api/workspaces/{ws}/tasks/{name}/execute`
- Workers download workspace tarballs via `GET /worker/workspace/{ws}.tar.gz` with ETag caching
- `WorkspaceCache` in worker manages per-workspace tarball extraction and revision tracking

### Scheduler (Cron Triggers)
- `crates/stroem-server/src/scheduler.rs` — background task that fires cron triggers
- `crates/stroem-server/src/job_creator.rs` — shared job+step creation used by both API handler and scheduler
- Uses `croner` crate for cron parsing (supports 5-field and 6-field with seconds via `with_seconds_optional()`)
- Smart sleep: wakes only at next fire time, no fixed polling interval
- Config hot-reload: picks up workspace changes on each cycle, preserves `last_run`/`next_run` for unchanged triggers
- Clean shutdown via `tokio_util::sync::CancellationToken` (SIGINT/SIGTERM)
- Jobs created by triggers have `source_type = "trigger"`, `source_id = "{workspace}/{trigger_name}"`
- Cron validation at YAML parse time in `validation.rs` (CLI `validate` catches bad expressions)

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

### Hooks (on_success / on_error)
- `HookDef` struct in `crates/stroem-common/src/models/workflow.rs` — `action` + `input` map
- `TaskDef` has `on_success: Vec<HookDef>` and `on_error: Vec<HookDef>` (default empty)
- **Workspace-level hooks**: `WorkflowConfig` and `WorkspaceConfig` also have `on_success`/`on_error` fields — act as fallback defaults when a task has no hooks defined for that event type. Only fire for top-level jobs (`source_type` is `api`, `trigger`, or `webhook`). Evaluated independently per event type.
- `crates/stroem-server/src/hooks.rs` — `fire_hooks()` builds `HookContext`, renders input through Tera, creates single-step hook jobs
- Recursion guard: jobs with `source_type = "hook"` never trigger further hooks
- Hook jobs: `task_name = "_hook:{action}"`, `source_type = "hook"`, `source_id = "{ws}/{task}/{job_id}/{hook_type}[idx]"`
- Validation in `validation.rs` — hook action references must exist (or be library actions with `/`), both task-level and workspace-level
- Migration `005_hooks.sql` adds `'hook'` to `source_type` CHECK constraint
- Hook actions can be `type: task` — creates a full child job instead of a single-step hook job

### Task Actions (type: task — Sub-Job Execution)
- `ActionDef.task: Option<String>` — references another task by name
- `type: task` actions cannot have `cmd`, `script`, `image`, or `runner`
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

### Log Storage
- `LogStorage` in `AppState` — local JSONL files + optional S3 archival
- S3 support enabled by default via `s3` cargo feature on `stroem-server` (`aws-sdk-s3` + `aws-config`)
- S3 upload spawned as background task when a job reaches terminal state — **after** hooks fire, so server events are included
- **Structured S3 keys**: `{prefix}{workspace}/{task}/YYYY/MM/DD/YYYY-MM-DDTHH-MM-SS_{job_id}.jsonl.gz` (all datetimes in UTC)
- **Gzip compression**: uploads compressed with `flate2::GzEncoder`, downloads decompressed with `GzDecoder`
- `JobLogMeta` struct carries workspace, task_name, created_at — used to construct S3 keys (no DB column for the key)
- `upload_to_s3(job_id, meta)`, `get_log(job_id, meta)`, `get_step_log(job_id, step, meta)` all require `&JobLogMeta`
- Read fallback: local file → legacy .log → S3 (if configured)
- Config: optional `s3` section in `log_storage` with `bucket`, `region`, `prefix`, `endpoint`
- Credentials via standard AWS chain (env vars, IAM role, `~/.aws/credentials`)
- **Server events**: `AppState::append_server_log()` writes JSONL entries with `step: "_server"`, `stream: "stderr"` — makes server-side errors (hook failures, orchestration errors, recovery timeouts) visible in the job's log stream via UI and API (`GET /api/jobs/{id}/steps/_server/logs`)

### WebSocket Log Streaming
- `GET /api/jobs/{id}/logs/stream` -- WebSocket upgrade endpoint
- Sends existing log content (backfill) on connect, then streams live chunks
- Per-job broadcast channels via `tokio::sync::broadcast` in `LogBroadcast`

### Worker Recovery
- `crates/stroem-server/src/recovery.rs` — background sweeper that detects stale workers
- `crates/stroem-server/src/job_recovery.rs` — shared orchestration logic (used by both `complete_step` handler and recovery sweeper)
- Config: optional `recovery` section in `server-config.yaml` with `heartbeat_timeout_secs` (default 120) and `sweep_interval_secs` (default 60)
- Always active with defaults when config section is absent (`#[serde(default)]`)
- Sweep cycle: mark stale workers inactive → fail their running steps → orchestrate each job (promote/skip/terminal) → propagate to parent
- Worker heartbeat (`POST /worker/heartbeat`) also sets `status = 'active'`, so returning workers auto-reactivate
- `WorkerRepo::mark_stale_inactive()` uses `make_interval(secs => $1)` for threshold comparison
- `JobStepRepo::get_running_steps_for_workers()` finds stuck steps by worker ID
- Failed steps get error message: `"Worker heartbeat timeout (worker {id} unresponsive)"`
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
