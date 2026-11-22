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
Phase 6f implemented, not released: File inputs (`type: file`: upload, then the step gets a read-only path; branch `feat/file-inputs-impl`, § File Inputs).
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
- **YAML parsing**: `serde_yaml` (direct parsing in tests/models), `config` crate (loading with env var overrides)
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
