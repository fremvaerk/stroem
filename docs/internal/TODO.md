# Strøm TODO

Consolidated from [codebase-review-2026-03-01.md](codebase-review-2026-03-01.md) and development memory.
Last updated: 2026-03-13.

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
- [x] Error messages leak internal details — `AppError::Internal` logs server-side, returns generic "Internal server error" to clients
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
- [x] Multi-language inline scripts: `type: shell` → `type: script` with `language`, `dependencies`, `interpreter` fields (Python, JS, TS, Go)
- [ ] Single-server bottleneck: scheduler, recovery, log broadcast in-process — no leader election
- [ ] No metrics/Prometheus endpoint — capacity planning blind
- [x] No proper health check — `GET /healthz` with DB ping + scheduler/recovery liveness via `BackgroundTasks` atomic flags
- [ ] Log retention: local JSONL grows unbounded; no cleanup after S3 upload
- [ ] No API versioning (`/api/` with no version prefix)
- [ ] DB transactions only for job+steps creation; other paths still have TOCTOU races
- [ ] `AppState` is a God Object — no compile-time capability enforcement
- [x] Error responses — `AppError` enum in `web/error.rs` with `IntoResponse`, all handlers migrated, internal details sanitized
- [x] Step-level timeout, job-level timeout, and cron concurrency policy
- [ ] No heartbeat failure → worker re-registration logic
- [ ] Store workspace revision (git SHA / folder content hash) on job creation — enables linking jobs to the exact config/scripts version, diffing between runs, and detecting stale workers running old code
- [ ] No default timeout for running jobs/steps — a stuck pod or script runs forever if no explicit `timeout` is set. Add server-level `default_step_timeout` / `default_job_timeout` config that applies when tasks/steps don't specify their own.

## Code Quality / Rust

- [x] `context(format!(...))` → `with_context(|| ...)`
- [x] `unwrap()` in production code → `expect()` with invariant description
- [x] Connection pool sized (max=20, min=5)
- [x] Worker tarball extraction wrapped in `spawn_blocking`
- [x] `notify` uses default features (no macos_fsevent hardcode)
- [x] Log file handles cached in DashMap (no open/close per append)
- [x] JSONL log lines use LogEntry struct (no intermediate Value allocation)
- [x] `action_type`, `status`, `source_type` are all String — ActionType/SourceType/JobStatus/StepStatus enums with Display/AsRef/FromStr
- [x] Workspace config deep-cloned on every `get_config_async` — returns `Arc<WorkspaceConfig>`
- [x] UUID parsing boilerplate repeated 6 times in worker API — uses Axum `Path<Uuid>` extractor
- [x] Business logic mixed into web handlers — extracted `rendering.rs` module from `claim_job`
- [x] No validation of `worker_token` length, `jwt_secret` strength, or timeout minimums — `validate()` on ServerConfig/WorkerConfig
- [x] `get_step_log` reads entire file into memory to filter by step name — line-by-line BufReader streaming
- [x] S3 download reads entire object into memory for decompression — streaming GzDecoder + BufReader
- [x] LogBroadcast channels grow unbounded — bounded broadcast + DashMap cleanup already in place
- [x] K8s pod logs fetched only after termination — already streams live via `follow: true` in LogParams
- [x] Missing `#[serde(deny_unknown_fields)]` on config types — added to all config structs
- [x] `validate_dag` clones all step names into HashMap keys — uses `&str` references
- [x] `Vec::remove(0)` in topological sort — already uses VecDeque
- [x] Workspace cache race on concurrent extraction — per-workspace Mutex + atomic rename

## Performance

- [x] Partial GIN index on `job_step(status='ready', action_type!='task')` — migration 009
- [x] N+1 queries in orchestration — batch UPDATE
- [x] Log file handle caching — DashMap + AtomicBool
- [x] 4x `get_workspace` in `claim_job` — consolidated to single call
- [x] Vite code splitting for @xyflow/react
- [x] Claim query ordering causes hot-row contention — `ORDER BY random()` in claim query
- [x] Worker poll has no exponential backoff on empty queue — `compute_poll_backoff` with 4× cap
- [x] S3 upload reads entire log into memory + synchronous gzip in async context — `spawn_blocking`
- [x] `LogBroadcast::subscribe` always acquires write lock even when channel exists — read-then-write pattern
- [x] WorkflowDag recalculates full dagre layout on every selectedStep change — split into layout + selection memos
- [x] Job Detail REST polls every 3s while WebSocket already active — adaptive 8s/3s based on running steps

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
- [x] Loading spinner inlined ~10 times — use LoadingSpinner component consistently
- [x] `task-detail.tsx` at 877 lines — extract InputFieldRow, ComboboxField
- [x] Array index as React key for log lines (`log-viewer.tsx:88`)
- [x] `listAllTasks` is N+1 (one request per workspace) — add cross-workspace endpoint
- [x] Token refresh deduplication has fragile invariants — encapsulate in class
- [x] `login-callback.tsx` uses `window.location.href` — full page reload loses in-memory token
- [x] `title` attributes used instead of accessible Tooltip components
- [x] `@types/dagre` installed but `@dagrejs/dagre` ships own types — remove
- [x] Both `tw-animate-css` and `tailwindcss-animate` installed — remove unused one
- [x] No sourcemap in production builds
- [x] `vite.config.ts` suppresses proxy errors silently
- [ ] Job duration insights: show average/p50/p95 duration for a task and per-step, estimated time remaining on running jobs and individual steps, and a duration history chart on the job detail page (with per-step breakdown)

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

## ACL / Authorization

- [x] Database migration: `is_admin` on user, `user_group` table
- [x] Config-driven ACL rules in `server-config.yaml` (`acl:` section)
- [x] `AclEvaluator` module with highest-wins permission evaluation (Run > View > Deny)
- [x] `is_admin` in JWT Claims (backward compat via `#[serde(default)]`)
- [x] Admin middleware helpers (`is_admin()`, `require_admin()`)
- [x] Token creation includes `is_admin`
- [x] Initial user always promoted to admin; first OIDC user becomes admin
- [x] ACL enforcement on all API endpoints (tasks, jobs, workspaces, workers, users, WS)
- [x] Admin-only endpoints: user admin toggle, group CRUD
- [x] Frontend: auth context exposes `isAdmin`/`aclEnabled`, sidebar conditional, task detail `can_execute`, users page admin/group management
- [x] Config parsing tests for ACL section
- [x] ACL evaluator unit tests (19 tests: glob matching, evaluation logic, admin bypass, scoping)
- [ ] Integration tests for ACL enforcement (handler-level with DB)
- [ ] Playwright E2E tests for admin/ACL flows

## Review: ACL Implementation (2026-03-11)

### Critical
- [x] WebSocket ACL bypass: any ACL check failure silently allows upgrade (deny-by-default violated) — `ws.rs`
- [x] Workspace list ACL bypass: ACL errors return ALL workspaces unfiltered — `workspaces.rs`

### Important
- [x] Triggers endpoint has no ACL enforcement — info disclosure — `triggers.rs`
- [x] Workers `get_worker` returns job metadata without ACL filtering — `workers.rs`
- [x] WebSocket double token parsing (fragile duplication) — `ws.rs`
- [x] No group name validation (arbitrary strings accepted) — `users.rs`
- [x] `set_groups` INSERT lacks `ON CONFLICT DO NOTHING` — duplicate groups cause constraint error — `user_group.rs`
- [x] ACL check boilerplate duplicated 5x in `jobs.rs` with inconsistencies — refactor to helper
- [x] `count_with_acl` missing `param_idx += 1` after status binding — `job.rs` (verified: consistent with existing pattern, no fix needed)
- [x] No admin self-demotion guard — `users.rs`

### Minor
- [x] `glob_match` silently wrong with multiple `*` wildcards — `acl.rs`
- [x] Stale JWT `is_admin` (15-min window after admin revocation) — known limitation, documented below

> **Known limitation**: When an admin revokes another user's admin status, the
> user's existing JWT access tokens remain valid (with the old `is_admin: true`)
> until they expire (15 minutes). API key requests always read `is_admin` from
> the DB, so they reflect changes immediately. The 15-minute window is acceptable
> given the access token TTL. Mitigation: use shorter token TTLs or implement a
> token revocation list (not currently planned).
- [x] Frontend race condition in rapid group operations — `user-detail.tsx`
- [x] `user.groups!` non-null assertion should use `?? []` — `user-detail.tsx`
- [x] `getUserGroups` in `api.ts` is dead code (never called) — removed
- [x] Missing `#[tracing::instrument]` on `load_user_acl_context` — `acl.rs`
- [x] Silent error swallowing in `get_user` groups fetch — `users.rs`

## Phase 5: Advanced Flow Control

- [x] 5a: Conditional flow steps (`when` expressions) — model, template evaluation, orchestrator, DB migration, validation, API, UI, docs, tests
- [ ] 5b: For-each loops (fan-out/fan-in)
- [ ] 5c: While loops (retry-until patterns)
- [ ] 5d: Suspend/Resume + Approval gates

## Review: Phase 5a Conditional Flow Steps (2026-03-12)

### Critical
- [x] Stale render context in cascade loop — context built once before loop; steps skipped/failed in iteration N invisible to condition evaluations in iteration N+1. Rebuild context inside loop after each pass. — `orchestrator.rs:25-62`, `job_creator.rs:155-169`
- [x] Missing `AND status = 'pending'` guard on `mark_skipped` — concurrent orchestrator+recovery can overwrite completed/running steps — `job_step.rs:527`
- [x] Missing `AND status = 'pending'` guard on `mark_failed` in `promote_ready_steps` — guarded inline SQL used instead of shared helper — `job_step.rs:498-512`

### Important
- [x] Failed steps excluded from render context — `continue_on_failure` + `when` referencing a failed step causes undefined variable error. Include failed steps with `{"output": null, "error": "..."}` — `job_creator.rs`
- [x] Case-sensitive truthiness in `evaluate_condition` — `"False"`, `"FALSE"`, `"null"`, `"none"` now falsy (case-insensitive) — `template.rs:94-101`
- [x] Condition evaluated from in-memory `flow_step.when` instead of DB `step.when_condition` — now uses `step.when_condition` from JobStepRow — `job_step.rs:449`
- [x] No infinite loop guard on cascade — `task.flow.len() + 1` safety bound with tracing::warn — `orchestrator.rs:39`, `job_creator.rs:161`
- [x] Batch `to_skip` and `to_fail` in `promote_ready_steps` — `to_skip` batched with `ANY($2)`; `to_fail` uses inline guarded SQL — `job_step.rs:481-512`
- [x] `when_condition` set twice in job detail API — removed flow config override, DB value is source of truth — `jobs.rs`

### Test Coverage
- [x] Integration: `promote_ready_steps` with actual `when` conditions (true→ready, false→skipped, error→failed) — `orchestrator_test.rs`
- [x] Integration: orchestrator `on_step_completed` with `workspace_config = Some(...)` and conditional steps (5 tests)
- [x] Integration: cascade — `when`-skipped step → downstream also skipped
- [x] Integration: root step `when` condition at job creation (immediate skip + cascade) — `integration_test.rs`
- [x] Integration: all steps conditional, all false → job completes as `completed`
- [x] Unit: `build_step_render_context` with skipped step → `{"output": null}`
- [x] Unit: `when` condition referencing skipped step's null output — `orchestrator_test.rs`
- [x] Integration: `when_condition` visible in job detail API response — `integration_test.rs`
- [x] Integration: `continue_on_failure` + skipped dependency + `when` condition interaction
- [x] Integration: `type: task` step with `when` condition (should skip without creating child job) — `integration_test.rs`
- [x] Integration: `when` with `render_context = None` leaves step as `pending` (recovery sweeper path) — `orchestrator_test.rs`
- [x] Unit: empty `when: ""` behavior documented — `orchestrator_test.rs`

## Review: Skipped-Deps-As-Satisfied (2026-03-14)

### Critical
- [x] `continue_on_failure: true` + all-deps-skipped incorrectly cascade-skips the step — guard on `!flow_step.continue_on_failure` — `job_step.rs:465`

### Test Coverage
- [x] Cancelled dep blocks step without `continue_on_failure` — Test 23 `orchestrator_test.rs`
- [x] Cancelled dep + `continue_on_failure: true` → step proceeds — Test 24 `orchestrator_test.rs`
- [x] All-deps-skipped + `continue_on_failure: true` → step proceeds (after bug fix) — Test 25 `orchestrator_test.rs`
- [x] Truthy `when` + all-deps-skipped → cascade-skip takes precedence — Test 26 `orchestrator_test.rs`
- [x] 3+ dep fan-in with heterogeneous statuses (1 completed + 2 skipped → runs) — Test 27 `orchestrator_test.rs`
- [x] `skip_unreachable_steps` does NOT treat skipped dep as blocking (regression guard) — `job_step_status_tests.rs`

### Minor
- [x] Test 12 comment says "skipped as unreachable" but cascade-skip now happens in `promote_ready_steps` — update comment — `orchestrator_test.rs`

## Review: Health Check + AppError Migration (2026-03-15)

### Critical
- [x] `middleware.rs:33` — `user_id()` returns 500 → changed to 401 Unauthorized
- [x] `auth.rs:112,170` — "Auth not configured" returns 404 → changed to 400 BadRequest

### Important
- [x] `health.rs:15` — `SELECT 1` wrapped in `tokio::time::timeout(3s, ...)`
- [x] `tasks.rs:335` — `create_job_for_task` validation errors now mapped to 400 BadRequest
- [x] AliveGuard extracted to `state.rs` as `pub(crate)`, removed from scheduler.rs and recovery.rs
- [x] `health.rs` test `test_alive_guard_drop_clears_flag` now tests real `AliveGuard`

### Minor
- [x] `api_keys.rs` — added `.context()` to all `ApiKeyRepo` calls
- [x] `jobs.rs:169,508` — `.into_response()` is actually required (return type is `Result<Response, _>`) — verified correct

### Minor
- [x] Validation doesn't catch typos in `when` variable references (step names) — documented as known limitation with comment — `validation.rs`
- [x] Two-pass validation silently passes unknown Tera filters — clarifying comment added — `validation.rs`
- [x] 13-element tuple in `create_steps_tx` replaced with `StepInsertRow` named struct — `job_step.rs`
- [x] Double step-list fetch per cascade iteration — TODO comment added for future optimization — `orchestrator.rs`
- [x] `REPEATABLE READ` transaction for `promote_ready_steps` — TODO comment added for future hardening — `job_step.rs`

## Roadmap Items (from review)

- [x] MCP server endpoint (Phase 7a): 8 tools, Streamable HTTP transport, auth support
- [x] MCP per-tool ACL enforcement (Phase 7a follow-up)
- [ ] Leader election via pg advisory locks for scheduler/recovery
- [ ] Generate OpenAPI spec with `utoipa`
- [ ] S3 as primary log store (eliminate local file dependency)
- [ ] Server push to workers (replace polling)
- [ ] Circuit breaker for worker→server communication
- [ ] Retry for final log flush and S3 upload
- [ ] Extract `stroem-orchestrator` crate (orchestrator, job_creator, job_recovery, hooks, scheduler)

## Review: Unmatched Step Recovery (2026-03-05)

### Critical
- [x] `get_unmatched_ready_steps()` missing `AND action_type != 'task'` — task steps wrongly failed when no workers active
- [x] Add config validation: `unmatched_step_timeout_secs >= 5`

### Important
- [x] Rewrite SQL predicate `ready_at + interval < NOW()` → `ready_at < NOW() - interval` for index seekability
- [x] Add partial B-tree index on `ready_at WHERE status = 'ready'` in migration 016
- [x] Add test: inactive worker with matching tags must not protect step from Phase 4
- [x] Add test: zero workers registered — unmatched step should be failed
- [x] Add test: `type: task` step not failed when no workers active
- [x] Add test: empty `required_tags` step not failed when any active worker exists
- [x] Add test: config validation rejects `unmatched_step_timeout_secs < 5`

### Minor
- [x] Existing DB tests (`test_create_steps_and_claim`, `test_promote_ready_steps`) don't assert `ready_at`
- [x] Config parse test `test_parse_config_recovery_defaults` doesn't assert `unmatched_step_timeout_secs == 30`

## Review: Webhook Job Status Endpoint (2026-03-12)

### Critical
- [x] Secret leaked via `#[tracing::instrument]` — `webhook_job_status` only skips `state`; `query` (contains secret) and `headers` (contains Bearer token) logged in traces. Also `webhook_handler` doesn't skip `query`.
- [x] `webhook_handler` race guard missing `Cancelled` status — lines 112-114 only check `Completed | Failed`, inconsistent with new code that correctly includes `Cancelled`

### Important
- [x] `HashMap` reconstruction for secret validation — `StatusQuery.secret` round-tripped through HashMap for `extract_secret` compatibility. Refactor `validate_webhook_secret` to accept `Option<&str>` directly.
- [x] `Lagged` broadcast error silently treated as timeout — wildcard arm catches both timeout and `RecvError::Lagged`; should re-query DB on Lagged
- [x] Missing `Cache-Control: no-store` on webhook job status responses — mutable job status could be cached by intermediaries

### Minor
- [x] Duplicated terminal-status check (lines 293-295 and 307-309) — extract `is_terminal_status()` helper
- [x] Response struct vs `json!()` inconsistency — refactored `webhook_handler` to use typed `WebhookAsyncResponse`/`WebhookSyncResponse` structs — `hooks.rs`
- [x] Response missing timestamps (`created_at`, `completed_at`) — available on JobRow but not returned
- [x] UUID error message phrasing — fixed to `"Invalid job ID"` consistent with API — `hooks.rs`

### Test Coverage
- [x] Integration: malformed UUID returns 400 — `integration_test.rs`
- [x] Integration: unknown webhook returns 404 — `integration_test.rs`
- [x] Integration: unknown job_id returns 404 — `integration_test.rs`
- [x] Integration: IDOR — API-sourced job returns 404 — `integration_test.rs`
- [x] Integration: IDOR — job from different webhook returns 404 — `integration_test.rs`
- [x] Integration: secret required but missing returns 401 — `integration_test.rs`
- [x] Integration: valid secret (query param and Bearer) returns 200 — `integration_test.rs`
- [x] Integration: default no-wait returns current status — `integration_test.rs`
- [x] Integration: wait on terminal job returns immediately — `integration_test.rs`
- [x] Integration: wait timeout returns 202 — (covered by existing sync mode tests)
- [x] Integration: cancelled job treated as terminal — `integration_test.rs`

## Review: MCP Server Endpoint (2026-03-12)

### Critical
- [x] `source_type = "mcp"` violates DB CHECK constraint — `tools.rs:283` passes `"mcp"` but constraint only allows `trigger/user/api/webhook/hook/task`. Every `execute_task` call fails. Fix: migration `019_mcp_source_type.sql`
- [x] `"mcp"` missing from hooks `is_top_level` check — `hooks.rs:67-69` only matches `api|user|trigger|webhook`. MCP jobs won't fire workspace-level hook fallbacks. Fix: add `"mcp"` to match
- [x] Auth not enforced on MCP endpoint — `auth.rs` functions wired via task_local middleware + per-tool ACL checks

### Important
- [x] `can_execute` hardcoded — now uses ACL scope to determine Run vs View permission
- [x] Missing `#[tracing::instrument]` on public MCP functions — violates project convention
- [x] `not_found` uses wrong JSON-RPC error code — `tools.rs:133` maps "not found" to `invalid_params` (-32602). Should use custom code or `internal_error`
- [x] `list_jobs` limit accepts negative values — `tools.rs:416` `.min(100)` without `.max(1)`. Negative LIMIT = no limit in Postgres
- [x] `task.flow` iteration order is non-deterministic — `get_task` iterates HashMap, random step order each call

### Minor
- [x] `format_logs` timestamp slicing — uses `find('T')` instead of fixed offset
- [x] `json_result` error fallback doesn't JSON-escape error message — `tools.rs:142-143`
- [x] Double `Utc::now()` in `auth.rs:71-72` — minor TOCTOU; synthetic `exp` misleading for API key path
- [x] No audit trail for MCP operations — `source_id` now set to user email from auth context

### Test Coverage
- [x] Integration: `execute_task` → `get_job_status` happy path — `mcp_test.rs`
- [x] Integration: MCP endpoint disabled returns 404 — `mcp_test.rs`
- [x] Integration: `tools/list` returns all 8 tools — `mcp_test.rs`
- [x] Integration: parameter validation (invalid UUID, invalid status) — `mcp_test.rs`
- [x] Unit: `McpConfig` parsing (enabled/disabled/absent/unknown fields) — 4 tests in config.rs
- [x] Unit: `format_logs` edge cases (missing fields, short timestamps, Unicode) — 6 tests in tools.rs
- [x] Auth: 401 without token when auth enabled — `mcp_test.rs`
- [x] Regression: MCP-created jobs fire hooks correctly — `mcp_test.rs`

## Review: Timezone Support for Cron Triggers (2026-03-16)

### Important
- [x] Timezone tests use `Utc::now()` — pinned `now` for determinism in scheduler tests; triggers API test uses `Utc::now()` internally (can't inject), documented
- [x] `test_compute_next_run_with_timezone` now asserts exact UTC offset (01:00 UTC for 02:00 Copenhagen in winter)

### Test Coverage
- [x] YAML deserialization round-trip for `timezone` field — `workflow.rs`
- [x] Empty string `timezone: ""` rejected by validation — `validation.rs`
- [x] Hot-reload timezone removal (Some → None resets state) — `scheduler.rs`
- [x] `TriggerInfo` JSON serialization includes/omits `timezone` — `triggers.rs`
- [x] `compute_next_runs` with timezone and count > 1 (ordering) — `triggers.rs`
- [x] Runtime invalid timezone fallback to UTC in scheduler — `scheduler.rs`
- [x] DST spring-forward gap: trigger fires at first valid time — `scheduler.rs`
- [x] DST fall-back ambiguity: trigger fires once — `scheduler.rs`

## Bugs Found & Fixed

- [x] Workspace-level hooks not firing for authenticated API jobs — `source_type = "user"` missing from `is_top_level` check (v0.5.9)
- [x] Template render errors passed raw templates instead of failing steps (v0.5.8)
- [x] Worker `report_step_start` sent no JSON body → steps stuck
- [x] Worker `push_logs` sent wrong format → logs never stored
- [x] Tera hyphen bug: step names with hyphens → sanitize to underscores in template context
- [x] Relative script paths don't resolve via `current_dir`
