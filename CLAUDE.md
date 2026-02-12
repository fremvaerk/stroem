# Strøm v2 -- Development Guide

## Project Overview

Strøm is a workflow/task orchestration platform. Backend in Rust, frontend in React (Phase 2b).
Phase 1 (MVP) complete: end-to-end workflow execution via API and CLI.
Phase 2a complete: JWT authentication backend + WebSocket log streaming.

## Architecture

- **stroem-common**: Shared types, models, DAG walker, Tera templating, validation
- **stroem-db**: PostgreSQL layer via sqlx (runtime queries), migrations, repositories
- **stroem-runner**: Execution backends (ShellRunner for MVP)
- **stroem-server**: Axum API server, orchestrator, workspace loader, log storage
- **stroem-worker**: Worker process: polls server, executes steps, streams logs
- **stroem-cli**: CLI tool (validate, trigger, status, logs, tasks, jobs)

## Conventions

- **Error handling**: `anyhow::Result` everywhere. Use `.context("msg")` for error chain.
- **Async runtime**: tokio
- **Logging**: `tracing` crate. Use `#[tracing::instrument]` on public functions.
- **YAML parsing**: `serde_yml`
- **Database**: sqlx with runtime queries (`sqlx::query()` / `sqlx::query_as()`), NOT compile-time macros.
- **Tests**: Unit tests in-module (`#[cfg(test)] mod tests`). Integration tests in `tests/` dirs using `testcontainers` for Postgres.

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
- Update `docs/stroem-v2-plan.md` if the plan status changes.
- Add/update user-facing docs (README, CLI help text, workflow authoring guides) for anything users interact with.
- Keep code comments minimal — only where logic isn't self-evident.

### Tera Templating
- Step names with hyphens (e.g., `say-hello`) are sanitized to underscores (`say_hello`) in the template context because Tera interprets hyphens as subtraction.
- Workflow YAML must use underscored names in template references: `{{ say_hello.output.greeting }}`, not `{{ say-hello.output.greeting }}`.

## Build & Test

```bash
# Build everything
cargo build --workspace

# Run all tests (needs Docker for integration tests)
cargo test --workspace

# Run specific crate tests
cargo test -p stroem-common
cargo test -p stroem-db
cargo test -p stroem-runner

# Check formatting
cargo fmt --check --all

# Lint
cargo clippy --workspace -- -D warnings

# E2E tests (needs Docker)
./tests/e2e.sh
```

## Key Patterns

### Workflow YAML structure
See `docs/stroem-v2-plan.md` Section 2 for the full YAML format.

### Action types
- `type: shell` (no image) -> ShellRunner in-worker
- `type: shell` (with image) -> DockerRunner/KubeRunner (Phase 3)
- `type: docker` -> DockerRunner/KubeRunner (Phase 3)
- `type: pod` -> KubeRunner only (Phase 4)

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

### WebSocket Log Streaming
- `GET /api/jobs/{id}/logs/stream` -- WebSocket upgrade endpoint
- Sends existing log content (backfill) on connect, then streams live chunks
- Per-job broadcast channels via `tokio::sync::broadcast` in `LogBroadcast`
