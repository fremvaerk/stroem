# Strøm v2 -- Development Guide

## Project Overview

Strøm is a workflow/task orchestration platform. Backend in Rust, frontend in React (Phase 2).
This is Phase 1 (MVP): end-to-end workflow execution via API.

## Architecture

- **stroem-common**: Shared types, models, DAG walker, Tera templating, validation
- **stroem-db**: PostgreSQL layer via sqlx (runtime queries), migrations, repositories
- **stroem-runner**: Execution backends (ShellRunner for MVP)
- **stroem-server**: Axum API server, orchestrator, workspace loader, log storage
- **stroem-worker**: Worker process: polls server, executes steps, streams logs
- **stroem-cli**: CLI tool (stub in MVP)

## Conventions

- **Error handling**: `anyhow::Result` everywhere. Use `.context("msg")` for error chain.
- **Async runtime**: tokio
- **Logging**: `tracing` crate. Use `#[tracing::instrument]` on public functions.
- **YAML parsing**: `serde_yml`
- **Database**: sqlx with runtime queries (`sqlx::query()` / `sqlx::query_as()`), NOT compile-time macros.
- **Tests**: Unit tests in-module (`#[cfg(test)] mod tests`). Integration tests in `tests/` dirs using `testcontainers` for Postgres.

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

### API auth (MVP)
- No user auth in MVP
- Worker auth: bearer token from config (`worker_token`)
