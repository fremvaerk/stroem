# Strøm TODO

Consolidated from [codebase-review-2026-03-01.md](codebase-review-2026-03-01.md) and development memory.
Last updated: 2026-03-04.

---

## Security

- [x] Worker token exposed in K8s pod spec — moved to env var
- [x] WebSocket log streaming has no auth — added AuthUser extractor
- [x] CORS allows Any origin — restricted to configured base_url
- [x] OIDC state cookie missing Secure flag — conditional on HTTPS
- [x] Refresh token in localStorage — moved to HttpOnly cookie
- [x] Pod manifest overrides can escalate privileges — blocklist added
- [x] `subtle::ct_eq` short-circuits on length — SHA-256 hash before compare
- [x] No rate limiting on auth endpoints — per-IP via tower-governor
- [ ] Secrets passed as env vars to containers (visible in pod spec, /proc/environ)
- [ ] Webhook secret accepted in query string (logged by proxies) — deprecate in favor of Authorization header
- [ ] No per-worker authorization: any worker can complete any step, access any workspace
- [ ] Refresh tokens not invalidated on password change (30-day window)
- [ ] Error messages leak internal details (SQL errors, file paths) to API clients — sanitize for clients
- [ ] `vals` CLI executed via PATH — susceptible to binary replacement

## Architecture

- [x] Job+step creation wrapped in DB transactions
- [x] Worker graceful shutdown via CancellationToken
- [x] Workspace watcher cancellation tokens
- [x] Runner cancellation mechanism (shell/docker/k8s active kill)
- [x] Worker HTTP client request timeouts
- [x] Terminal job handling consolidated into `run_terminal_job_actions`
- [x] `blocking_read()` in async fn fixed — uses async `read().await`
- [x] `block_in_place` in spawned tasks — uses `spawn_blocking`
- [ ] Single-server bottleneck: scheduler, recovery, log broadcast in-process — no leader election
- [ ] No metrics/Prometheus endpoint — capacity planning blind
- [ ] No proper health check (`/api/config` probe doesn't verify DB/background tasks)
- [ ] Log retention: local JSONL grows unbounded; no cleanup after S3 upload
- [ ] No API versioning (`/api/` with no version prefix)
- [ ] DB transactions only for job+steps creation; other paths still have TOCTOU races
- [ ] `AppState` is a God Object — no compile-time capability enforcement
- [ ] Error responses manually constructed with `json!({})` — define `AppError` enum implementing `IntoResponse`
- [ ] No step-level timeout
- [ ] No heartbeat failure → worker re-registration logic

## Code Quality / Rust

- [x] `context(format!(...))` → `with_context(|| ...)`
- [x] `unwrap()` in production code → `expect()` with invariant description
- [x] Connection pool sized (max=20, min=5)
- [x] Worker tarball extraction wrapped in `spawn_blocking`
- [x] `notify` uses default features (no macos_fsevent hardcode)
- [x] Log file handles cached in DashMap (no open/close per append)
- [x] JSONL log lines use LogEntry struct (no intermediate Value allocation)
- [ ] `action_type`, `status`, `source_type` are all String — should be enums for exhaustive matching
- [ ] Workspace config deep-cloned on every `get_config_async` — should return `Arc<WorkspaceConfig>`
- [ ] UUID parsing boilerplate repeated 6 times in worker API — create custom Axum extractor
- [ ] Business logic mixed into web handlers (claim_job does template rendering inline)
- [ ] No validation of `worker_token` length, `jwt_secret` strength, or timeout minimums
- [ ] `get_step_log` reads entire file into memory to filter by step name
- [ ] S3 download reads entire object into memory for decompression
- [x] LogBroadcast channels grow unbounded — bounded broadcast + DashMap cleanup already in place
- [ ] K8s pod logs fetched only after termination — no live streaming during execution
- [ ] Missing `#[serde(deny_unknown_fields)]` on config types — typos silently ignored
- [ ] `validate_dag` clones all step names into HashMap keys — use `&str` references
- [x] `Vec::remove(0)` in topological sort — already uses VecDeque
- [ ] Workspace cache race on concurrent extraction — add per-workspace Mutex

## Performance

- [x] Partial GIN index on `job_step(status='ready', action_type!='task')` — migration 009
- [x] N+1 queries in orchestration — batch UPDATE
- [x] Log file handle caching — DashMap + AtomicBool
- [x] 4x `get_workspace` in `claim_job` — consolidated to single call
- [x] Vite code splitting for @xyflow/react
- [ ] Claim query ordering causes hot-row contention (all workers compete for same UUID-ordered step)
- [ ] Worker poll has no exponential backoff on empty queue
- [ ] S3 upload reads entire log into memory + synchronous gzip in async context
- [ ] `LogBroadcast::subscribe` always acquires write lock even when channel exists
- [ ] WorkflowDag recalculates full dagre layout on every selectedStep change
- [ ] Job Detail REST polls every 3s while WebSocket already active — make adaptive

## Frontend

- [x] Error boundaries added (per-page + top-level catch-all)
- [x] `useAsyncData` stale-response guard + error state
- [x] `useWorkerNames` singleton cache with 60s TTL
- [x] Dashboard uses server-side `/api/stats` endpoint
- [x] `useAsyncData` resets data on fetcher change
- [x] `apiFetch` handles 204 No Content
- [x] Removed unused `sonner`/`next-themes` deps
- [x] Extracted `formatTime`/`formatDuration` to `lib/formatting.ts`
- [x] Shared `WorkerStatusBadge` component
- [x] StepTimeline `aria-label` and `aria-expanded`
- [x] LogViewer `role="log"` and `aria-label`
- [x] "Live" indicator sr-only text
- [ ] Loading spinner inlined ~10 times — use LoadingSpinner component consistently
- [ ] `task-detail.tsx` at 877 lines — extract InputFieldRow, ComboboxField
- [ ] Array index as React key for log lines (`log-viewer.tsx:88`)
- [ ] `listAllTasks` is N+1 (one request per workspace) — add cross-workspace endpoint
- [ ] Token refresh deduplication has fragile invariants — encapsulate in class
- [ ] `login-callback.tsx` uses `window.location.href` — full page reload loses in-memory token
- [ ] `title` attributes used instead of accessible Tooltip components
- [ ] `@types/dagre` installed but `@dagrejs/dagre` ships own types — remove
- [ ] Both `tw-animate-css` and `tailwindcss-animate` installed — remove unused one
- [ ] No sourcemap in production builds
- [ ] `vite.config.ts` suppresses proxy errors silently

## Test Coverage

### Done
- [x] Orchestrator unit tests (9 integration tests: DAG promotion, failures, continue_on_failure, diamond joins)
- [x] CLI tests (35 unit tests: arg parsing, URL construction, validation, response checking)
- [x] Frontend Vitest tests (119 tests: StatusBadge, useAsyncData, api.ts, formatting)
- [x] DB-level tests for mark_failed, mark_skipped, mark_cancelled, transaction rollback, parent/child
- [x] E2E: single-step execution, multi-step output propagation
- [x] E2E: failing task, on_error hook fires
- [x] E2E: on_success hook fires
- [x] E2E: worker recovery (stale heartbeat)
- [x] E2E: job cancellation (running + pending)
- [x] E2E: default input values
- [x] E2E: task action sub-jobs (type: task)
- [x] Workspace-level hook fallback for `source_type = "user"` (unit test in hooks.rs)

### Still Missing
- [x] Version reporting feature tests (DB round-trip, API responses, serde backward compat, /api/config version)
- [x] Worker `execute_claimed_step` integration test (3 wiremock-based tests: happy path, workspace failure, command failure)
- [ ] Live DockerRunner execution tests
- [ ] Live KubeRunner execution tests (use testcontainers k3s module; refactor KubeRunner to accept optional kube::Client; NoWorkspace mode first, WithWorkspace needs mock tarball endpoint)
- [x] Runner error path tests (6 tests: shell nonexistent workdir/binary/script, docker container config unit tests)
- [x] `render_connections()` unit test (13 tests in workflow.rs)
- [x] `resolve_connection_inputs()` unit test (17 tests in template.rs)
- [x] `validate_workflow_config_with_libraries()` dedicated tests (8 tests in validation.rs)
- [x] Model deserialization edge cases (TriggerDef accessors, ConnectionDef flatten) — 50+ tests in workflow.rs + validation.rs
- [x] Tag-containment edge cases in `claim_ready_step` (6 tests in stroem-db integration_test.rs)
- [x] Migration idempotency test (2 tests: double-run idempotency, schema completeness check)
- [x] Auth middleware helper function unit tests (14 unit + 30+ integration tests)
- [x] Webhook sync-mode timeout test (integration test in stroem-server: sync returns 202 on timeout, async returns 200)
- [x] Scheduler `fire_trigger` → job creation test (4 integration tests: fires cron, disabled skip, input passthrough, clean shutdown)
- [x] `propagate_to_parent` integration test (5 tests: child completed/failed/cancelled, 3-level nesting, mixed steps)
- [ ] Worker registration retry/backoff test
- [ ] Worker semaphore/max-concurrent limit test
- [ ] E2E: cron scheduler trigger fires job
- [ ] E2E: multi-workspace scenarios
- [ ] Playwright: Settings page (API key management)
- [ ] Playwright: OIDC login flow
- [ ] Playwright: webhook trigger display
- [ ] Playwright: Dashboard content
- [x] Full library loading pipeline integration test (2 tests in library.rs: happy path through WorkspaceManager::new + validation failure for missing ref)
- [ ] Git workspace source auth failure test

## Roadmap Items (from review)

- [ ] Leader election via pg advisory locks for scheduler/recovery
- [ ] Generate OpenAPI spec with `utoipa`
- [ ] S3 as primary log store (eliminate local file dependency)
- [ ] Server push to workers (replace polling)
- [ ] Circuit breaker for worker→server communication
- [ ] Retry for final log flush and S3 upload
- [ ] Extract `stroem-orchestrator` crate (orchestrator, job_creator, job_recovery, hooks, scheduler)

## Bugs Found & Fixed

- [x] Workspace-level hooks not firing for authenticated API jobs — `source_type = "user"` missing from `is_top_level` check (v0.5.9)
- [x] Template render errors passed raw templates instead of failing steps (v0.5.8)
- [x] Worker `report_step_start` sent no JSON body → steps stuck
- [x] Worker `push_logs` sent wrong format → logs never stored
- [x] Tera hyphen bug: step names with hyphens → sanitize to underscores in template context
- [x] Relative script paths don't resolve via `current_dir`
