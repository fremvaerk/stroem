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
- [x] `file_path` not shell-escaped in `build_container_file_cmd` — interpolated raw into `sh -c` strings. Fixed: `shell_escape()` applied in non-passthrough branches. Passthrough (exec-form) branches unaffected.
- [x] `interpreter_override` not shell-escaped at point of use in container commands — validation restricts charset to `[a-zA-Z0-9._\-/+]` which is safe. Added SECURITY comment at interpolation sites documenting the invariant.

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
- [x] Log retention: local JSONL grows unbounded; no cleanup after S3 upload — `retention_cleanup` in `recovery.rs` deletes local logs and S3 archives for terminal jobs older than `log_retention_days`
- [ ] No API versioning (`/api/` with no version prefix)
- [ ] DB transactions only for job+steps creation; other paths still have TOCTOU races
- [ ] `AppState` is a God Object — no compile-time capability enforcement
- [x] Error responses — `AppError` enum in `web/error.rs` with `IntoResponse`, all handlers migrated, internal details sanitized
- [x] Step-level timeout, job-level timeout, and cron concurrency policy
- [x] Retry failed workspace loads — placeholder entries with `load_error`, watcher retries on each poll cycle
- [x] Workspace retry: watcher uses `.unwrap()` on `load_error` lock — aligned with `.unwrap_or_else(|e| e.into_inner())` pattern
- [x] Workspace retry: `get_revision()` gated by `load_error` — returns None for errored workspaces
- [x] Workspace retry: documented `names()` excludes `GitSource::new()` failures, `list_workspace_info()` includes both
- [ ] No heartbeat failure → worker re-registration logic
- [x] Store workspace revision (git SHA / folder content hash) on job creation — enables linking jobs to the exact config/scripts version, diffing between runs, and detecting stale workers running old code
- [x] Move agent dispatch from server to workers — workers now claim agent steps, execute LLM calls, and manage MCP connections. Server only validates provider names. stroem-agent crate holds shared dispatch logic.
- [ ] No default timeout for running jobs/steps — a stuck pod or script runs forever if no explicit `timeout` is set. Add server-level `default_step_timeout` / `default_job_timeout` config that applies when tasks/steps don't specify their own.
- [x] Folder workspace revision pinning — server-side TarballCache keyed by (workspace, revision), workers download pinned revision via `?revision=` query param, ClaimResponse includes job revision, stale cache entries cleaned up during retention sweep

## Simplification (from codex review 2026-03-17)

- [x] Workspace load state: add `is_healthy()` helper on `WorkspaceEntry` to eliminate repeated 6-line guard pattern in 5 accessors
- [x] Extract shared `scan_and_merge_yaml_files()` from `folder.rs` and `library.rs` — deterministic sorted merge order
- [x] `useAsyncData`: keep as-is — 3 call sites, acceptable abstraction tax, stale-response guard is non-trivial
- [x] Merge duplicate API tests: consolidated into `ui/src/lib/__tests__/api.test.ts`, deleted duplicate
- [x] Delete unused UI components: removed `dropdown-menu`, `select`, `collapsible` (367 lines); kept `sheet` (used by sidebar) and `command` (used by combobox-field)
- [ ] Trim sidebar.tsx: 772 lines, 24 exports, only 13 used (46% unused) — low priority

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
- [x] Workspace cache ENOTEMPTY on concurrent step execution — revision-based immutable directories with RAII ref-counted WorkspaceGuard

### Revision-based workspace cache review (2026-03-16)
- [x] `revision_refs` key mismatch: `acquire_guard` keys by raw revision, cleanup keys by sanitized dir name — fixed: `revision_ref_count` now sanitizes before keying
- [x] Empty revision string accepted: creates corrupted layout by extracting into workspace root — fixed: `extract_tarball_inner` rejects empty sanitized revision
- [x] `.current` file not written atomically: crash mid-write leaves truncated revision string — fixed: `atomic_write` helper uses write-to-tmp + rename
- [x] `fetch_add` ordering semantically wrong: `Ordering::Acquire` on increment should be `Relaxed` — fixed
- [x] `copy_dir_all` materializes symlinks as regular files on cross-filesystem fallback path — fixed: symlink branch added
- [x] `revision_refs` / `per_workspace_locks` DashMaps grow unboundedly — fixed: `cleanup_old_revisions` now prunes stale zero-count entries whose directory no longer exists
- [x] Test `test_cleanup_removes_old_revisions` uses 50ms sleep for mtime ordering — fixed: replaced with explicit `set_dir_mtime` using `File::set_times`
- [x] Missing test: empty revision string → `test_empty_revision_rejected`
- [x] Missing test: revision with special chars → `test_special_char_revision_ref_count_consistency`
- [x] Missing test: `ensure_up_to_date` 304 path with missing revision directory → already covered by `test_current_revision_returns_none_when_dir_missing`
- [x] Missing test: all old revisions in-use with `max_retained=0` → `test_cleanup_all_old_revisions_in_use`

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

## CLI: `stroem run` (local task execution, 2026-03-25)

### Critical (must fix)
- [x] `build_run_config` ignores `--path`, uses `cwd()` instead — fixed: `cmd_run` canonicalizes path, threads `workspace_path: &Path` through `run_dag` → `execute_step` → `build_run_config`
- [x] `continue_on_failure` failures not counted in summary — fixed: added `failed_count: usize` counter, incremented on every failure regardless of `continue_on_failure`

### Important (should fix)
- [x] No `for_each` item count limit — fixed: `MAX_FOR_EACH_ITEMS = 10_000` check in `evaluate_for_each`
- [x] `std::process::exit(1)` bypasses async cleanup — fixed: `cmd_run` returns `Result<bool>`, `main` calls `process::exit(1)` after async cleanup
- [x] Step timeouts silently ignored — fixed: `execute_step` wraps `runner.execute()` in `tokio::time::timeout` when `step.timeout` is set

### Minor
- [x] `ctrlc::set_handler` error silently dropped via `.ok()` — fixed: propagates with `.context()?`
- [x] Path traversal with `action.source` — fixed: canonicalized source path checked with `starts_with(workspace_path)`
- [x] Parallel DAG branches run sequentially — documented as limitation in `docs/src/content/docs/reference/cli.md`
- [x] `sequential: false` on `for_each` not honored — documented as limitation in `docs/src/content/docs/reference/cli.md`

### Missing Tests
- [x] `continue_on_failure = true` — `test_run_continue_on_failure`
- [x] `continue_on_failure` inside `for_each` — `test_run_for_each_continue_on_failure`
- [x] Empty `for_each` array — `test_run_for_each_empty_array`
- [x] Step output used in downstream `when` condition — `test_run_when_with_step_output`
- [x] Invalid `--input` JSON — `test_err_invalid_input_json`
- [ ] Non-object `--input` JSON (e.g., `"[1,2,3]"`) — returns clear error or handles gracefully
- [ ] Action with neither `script` nor `source` — clear error from runner
- [x] Diamond-shaped cascade-skip topology — `test_cascade_skip_diamond` + `test_integ_cascade_skip_diamond_dag`
- [x] Non-existent `--path` workspace directory — `test_err_nonexistent_workspace_path`
- [x] Cyclic DAG rejected before execution starts — `test_err_cyclic_dag_rejected`
- [x] `for_each` template rendering to JSON object (not array) — `test_evaluate_for_each_json_object_errors`
- [x] `for_each` output aggregation accessible by downstream step — `test_integ_for_each_output_accessible_downstream`
- [x] `runner: pod` rejection — `test_validate_actions_rejects_pod_runner`
- [x] `--path` workspace directory used as workdir — `test_integ_workspace_path_used_as_workdir`
- [x] `for_each` exceeds max items — `test_evaluate_for_each_exceeds_limit`

## CLI: Binary Split Review (2026-03-25)

### Critical (must fix)
- [x] `completed_count` usize underflow in `local/run.rs:331` — fixed: uses `saturating_sub` to prevent wrap/panic
- [x] UTF-8 truncation panic in `local/tasks.rs` and `local/actions.rs` — fixed: `truncate_desc` helper uses `char_indices()` for safe boundary + strips newlines
- [x] Release artifact glob in `release.yml:66` — fixed: multi-line path pattern includes both `stroem-*-x86_64-...` and `stroem-x86_64-...`

### Important (should fix)
- [x] `stroem --path /foo run task` silently ignores the global `--path` — fixed: removed Run's own `--path`, now uses global flag
- [x] Per-workspace errors silently swallowed in `remote/tasks.rs` — fixed: prints warning to stderr on non-success response

### Minor
- [x] Path containment check bypassed when `canonicalize()` fails — fixed: prints warning when source path doesn't exist
- [x] Disabled triggers shown without visual indicator — fixed: appends " [disabled]" to trigger name
- [x] Newlines in descriptions break table formatting — fixed: `truncate_desc` strips `\n`/`\r` before truncation

### Missing Tests: New Local Commands
- [x] `inspect` step extras: `for_each`, `when`, `timeout`, `continue_on_failure`, `sequential` display branches
- [x] `inspect` task-level `timeout` display
- [x] `inspect` `on_suspended` hooks display
- [x] `inspect` empty flow (0 steps)
- [x] `tasks` description truncation at boundary (37/40 chars)
- [x] `tasks` sorting order (alphabetical) assertion
- [x] `tasks` folder absent (`-` fallback)
- [x] `actions` description truncation at boundary (27/30 chars)
- [x] `actions` runner/language absent (`-` fallback)
- [x] `triggers` webhook `mode: None` path (defaults to "async")
- [x] `triggers` scheduler with timezone display

### Missing Tests: Argument Parsing
- [x] `logs` subcommand `_requires_job_id` test
- [x] `trigger` short `-w` flag test
- [x] `--path` flag after subcommand (`stroem tasks --path /foo`)
- [ ] `STROEM_TOKEN` / `STROEM_URL` env var parsing untested (requires env manipulation in tests, skipped)

### Missing Tests: Edge Cases
- [x] `for_each` where all iterations fail with `continue_on_failure: true` (regression test for underflow)
- [x] `validate` with malformed YAML file (`.yaml` with invalid syntax)
- [x] `build_client` with non-ASCII token characters (control chars rejected, error propagated)
- [x] Non-ASCII task descriptions at truncation boundary (`truncate_desc_multibyte_safe`)

## Test Coverage

### Done
- [x] Orchestrator unit tests (9 integration tests: DAG promotion, failures, continue_on_failure, diamond joins)
- [x] CLI tests (136 unit tests across stroem + stroem-api: arg parsing, URL construction, validation, response checking, local commands, edge cases)
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
- [x] Playwright: Settings page (API key management) — 10 tests in settings.spec.ts
- [ ] Playwright: OIDC login flow
- [ ] Playwright: webhook trigger display
- [ ] Playwright: Dashboard content
- [x] Full library loading pipeline integration test (2 tests in library.rs: happy path through WorkspaceManager::new + validation failure for missing ref)
- [x] Workspace retry: watcher healthy→errored→healthy full cycle test
- [x] Workspace retry: watcher sets error on continued failure test
- [x] Workspace retry: `get_revision()` returns None for errored-after-healthy workspace test
- [ ] Git workspace source auth failure test

### Script `args` Feature Test Gaps (2026-03-26)
- [x] Validation: `agent` action with `args` rejected
- [x] Validation: `approval` action with `args` rejected
- [x] Server rendering: `render_action_spec` renders args Tera templates (e.g. `["{{ input.x }}"]` → `["prod"]`)
- [x] Server rendering: args with step output context (`{{ build.output.artifact }}`)
- [x] Server rendering: args with secret context (`{{ secret.api_token }}`)
- [x] Server rendering: args with hyphenated step name sanitization
- [x] Executor: `build_run_config` extracts args from action_spec JSON
- [x] Executor: `build_run_config` defaults to empty vec when args key absent
- [x] Executor: non-string values in args array silently dropped (filter_map behavior)
- [x] CLI: `build_run_config` renders args Tera templates
- [x] CLI: args template render error propagated (not silently dropped)
- [x] ShellRunner: shell inline script with args (verify `$1` receives value)
- [x] ShellRunner: shell source file with args (verify positional args)
- [x] ShellRunner: non-shell (Python) with args (verify `sys.argv`)
- [x] DockerRunner: `build_container_config` WithWorkspace + shell + args
- [x] DockerRunner: `build_container_config` WithWorkspace + Python + args
- [x] KubeRunner: `build_pod_json_with_workspace` shell + args
- [x] `build_container_script_cmd` with args for TypeScript, JavaScript, Go languages
- [x] `build_container_file_cmd` with args for TypeScript, JavaScript, Go languages
- [x] `build_container_script_cmd` args with special characters (`$HOME`, backticks, newlines)
- [x] Shell escape assertion: verify single-quote escaping for `it's` → `'it'\''s'`
- [x] Args + dependencies + interpreter ordering (`uv run --with dep script.py arg1`)
- [x] `each` context (`each.item`, `each.index`, `each.total`) now available in `render_action_spec` and `render_image` — works in env, cmd, script, source, manifest, args, and image fields for `for_each` loop instances
- [x] Security: `file_path` shell-escaped in `build_container_file_cmd` non-passthrough branches
- [x] Security: SECURITY comment at `interpreter_override` interpolation sites

### Job Revision Tracking Test Gaps (2026-03-20)
- [x] API detail response (`GET /api/jobs/{id}`) asserts `revision` field in JSON
- [x] API list response (`GET /api/jobs`) asserts `revision` field in JSON
- [x] Hook job inherits `revision` from originating job (end-to-end)
- [x] Sub-job inherits `revision` via `handle_task_steps` (full orchestration path)
- [x] Scheduler-triggered job stores workspace `revision`
- [x] Webhook-triggered job stores workspace `revision`
- [x] MCP `get_job_status` response includes `revision` field
- [x] MCP `list_jobs` response includes `revision` field

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

## Phase 7: AI Agent Actions

### 7A: Single-turn agent (in progress)
- [x] ActionDef fields (provider, model, system_prompt, prompt, output via OutputDef, temperature, max_tokens, tools, max_turns)
- [x] AgentToolRef enum (Task + Mcp variants)
- [x] ActionType::Agent variant
- [x] validate_agent_action() with 14 unit tests
- [x] compute_required_tags / derive_runner for agent type
- [x] DB migration 023: action_type CHECK + agent_state column
- [x] Worker claim filter: agent steps now claimable by workers with "agent" tag (moved from server-side dispatch)
- [x] AgentsConfig + AgentProviderConfig (shared via stroem-agent crate)
- [x] Agent dispatch: moved from server-side (handle_agent_steps) to worker-side (AgentExecutor) via stroem-agent crate
- [x] Integration into orchestrate_after_step / propagate_to_parent (agent_tool re-claim pattern)
- [x] Structured output via OutputDef → JSON Schema (prompt engineering + JSON parsing)
- [x] Token usage tracking — single-turn now uses `CompletionModel::completion()` via shared `call_completion`, returns real `Usage`
- [x] Retry logic for transient LLM errors (429, 500, 502, 503, 529 + connection/timeout)
- [x] Temperature / max_tokens passthrough to rig agent builder
- [x] Initial agent step dispatch at job creation time (via agents_config parameter)
- [ ] Integration tests with wiremock (mock LLM server)
- [x] Documentation: CLAUDE.md agent section, agent-actions.md user guide

### 7A Review Fixes (2026-03-20)

#### Critical
- [x] `truncate_for_error` panics on multi-byte UTF-8 — fixed: uses `is_char_boundary()` loop — `dispatch.rs`
- [x] Partial GIN index `idx_job_step_ready_claim` predicate mismatch — fixed: rebuilt in migration 023 with `NOT IN ('task', 'agent')` — `023_agent_type.sql`

#### Important
- [x] No cancellation check in dispatch loop — fixed: checks job status != "cancelled" on each iteration — `dispatch.rs`
- [x] No timeout on LLM calls — fixed: 120s `tokio::time::timeout` wrapper — `dispatch.rs`
- [x] `is_transient_error` false positives — fixed: uses specific prefixes (`status: 500`, `http 500`) instead of bare `"500"` — `dispatch.rs`
- [x] ~200 lines duplicated between `handle_agent_steps` and `dispatch_initial_agent_steps` — extracted `resolve_and_render_step` + `execute_single_turn_with_retry` shared helpers — `dispatch.rs`
- [x] Secrets in prompts sent to external LLM APIs — documented in agent-actions.md Security Considerations section
- [ ] No SSRF validation on `api_endpoint` — could point to internal services. Validate against private IP ranges in config — `config.rs`
- [x] Missing `#[serde(deny_unknown_fields)]` on `AgentsConfig` and `AgentProviderConfig` — fixed — `config.rs`
- [x] Unbounded `max_retries` — fixed: validated <= 10 in `ServerConfig::validate()` — `config.rs`
- [x] `for_each` + agent step: `each` variable not injected — fixed: injects `each.item`/`each.index`/`each.total` for loop instances — `dispatch.rs`

#### Minor
- [x] `_meta` field collision — documented as reserved key in agent-actions.md + code comment — `dispatch.rs`
- [x] Empty rendered prompt not caught — fixed: checks `trim().is_empty()` and fails step — `dispatch.rs`
- [x] Token counts logged as if real — fixed: log message no longer mentions token counts — `dispatch.rs`
- [x] API key in `Debug` derive — fixed: manual `Debug` impl redacts `api_key` — `config.rs`
- [x] Misleading retry state machine — fixed: simplified to single `Result` variable — `dispatch.rs`

### 7A Missing Tests

#### Integration tests (need wiremock + testcontainers)
- [ ] Agent-only job: creation → step running → LLM call → step completed → job completed
- [ ] Agent step failure: LLM error → step failed → job failed
- [ ] Chained agent steps: step A output available in step B prompt via `{{ step_a.output.field }}`
- [ ] Mixed workflow: script step → agent step (both on worker)
- [x] Worker claims agent steps with "agent" tag (SQL filter updated)
- [ ] Recovery sweeper ignores agent steps (unmatched step timeout)
- [ ] `when: false` skips agent step without LLM call
- [ ] `continue_on_failure: true` + failed predecessor → agent step still dispatched
- [ ] `for_each` + agent step expansion and dispatch

#### Unit tests
- [x] `truncate_for_error` with multi-byte UTF-8 input (emoji, CJK, exact boundary)
- [x] Unstructured response wraps to `{"text": "..."}` shape
- [x] `_meta` fields present in structured output
- [ ] Retry exhaustion: correct attempt count with transient errors
- [ ] Non-transient error: exactly one attempt, no retry
- [x] `strip_code_fences` with no closing fence (malformed input)
- [ ] Empty prompt after template rendering → step fails
- [x] `dispatch_initial_agent_steps` removed — agent steps now dispatched by workers

### 7B+C: Multi-turn Agent Tools & MCP Client (2026-03-22)

- [x] AgentConversationState types (state.rs)
- [x] `interactive` flag on ActionDef
- [x] McpServerDef struct + mcp_servers on WorkflowConfig/WorkspaceConfig
- [x] Tool definition generation (tools.rs)
- [x] Custom multi-turn dispatch loop (loop_dispatch.rs)
- [x] MCP client manager (mcp_client.rs)
- [x] Dispatch integration — handle_agent_steps branches to loop_dispatch when tools present
- [x] approve_step handler dual-purpose — supports both approval gates and agent ask_user
- [x] propagate_to_parent agent detection — resumes dispatch loop when child jobs complete
- [x] Validation: tools, max_turns, interactive, MCP server defs, workspace-level tool refs
- [x] DB migration 025: agent_tool source_type
- [x] update_agent_state repo helper
- [x] SSE transport for MCP servers (stdio + SSE via Streamable HTTP client)
- [ ] Token usage tracking via CompletionModel (partially implemented)

### 7B+C Review Fixes (2026-03-22)

#### Critical (fixed)
- [x] User response injected into conversation state on ask_user resume — `jobs.rs`
- [x] Actual workspace config resolved on ask_user resume (was `WorkspaceConfig::default()`) — `jobs.rs`
- [x] Tool result injected on task-tool child completion — `job_recovery.rs`
- [x] `env_clear()` on MCP server process + warning log — `mcp_client.rs`

#### Important (fixed)
- [x] `_meta.turns` now uses actual turn count via `DispatchOutcome::Completed.turns` — `dispatch.rs`, `loop_dispatch.rs`
- [x] System prompt uses `preamble` instead of `Message::System` in chat_history — `loop_dispatch.rs`
- [x] 120s timeout on multi-turn LLM calls — `loop_dispatch.rs`
- [x] Cancellation check on each loop iteration — `loop_dispatch.rs`
- [x] `revision` and `agents_config` passed through to child job creation — `loop_dispatch.rs`
- [x] Task tool calls validated against `action_spec.tools` allowed set — `loop_dispatch.rs`
- [x] `is_mcp_tool` validates against actual connection prefixes — `mcp_client.rs`
- [x] `timeout_secs` applied to MCP server init and tool discovery — `mcp_client.rs`
- [x] Redundant dead branch removed — `loop_dispatch.rs`

#### Remaining (not yet fixed)
- [x] Underscore/hyphen normalization collision detection — validation rejects tasks/MCP servers with same normalized name — `validation.rs`
- [ ] No rate limit on tool calls per turn / unbounded JSONB state growth — `loop_dispatch.rs`, `state.rs`
- [ ] TOCTOU race in concurrent tool call resolution — `job_recovery.rs`
- [ ] MCP client shutdown on WaitingForTools — stateful MCP servers lose state — `dispatch.rs`
- [ ] WaitingForTools step stays running forever if child job propagation silently fails — `dispatch.rs`
- [ ] No audit trail for agent-created child jobs — `loop_dispatch.rs`
- [ ] No SSRF validation on MCP server URLs — `mcp_client.rs`

### SSE Transport Review Fixes (2026-03-23)

#### Important (fixed)
- [x] `SseMcpService` + `StdioMcpService` merged into single `RmcpService` — `mcp_client.rs`
- [x] ~50 lines deduped into `discover_and_register()` helper — `mcp_client.rs`
- [x] `auth_token: Option<String>` field added to `McpServerDef`, `env` docs clarified — `workflow.rs`, `mcp_client.rs`
- [x] `timeout_secs` doc comment clarified (init + discovery + calls) — `workflow.rs`

#### Minor (fixed)
- [x] URL validated for http/https scheme in `validate_mcp_servers` — `validation.rs`
- [ ] `reqwest::Client::default()` per SSE connection — no shared connection pool (acceptable at current scale) — `mcp_client.rs`

#### Tests (added)
- [x] `is_mcp_tool()` — 6 tests: matching, no prefix, unknown server, hyphenated, prefix only, empty — `mcp_client.rs`
- [x] `tool_definitions()` — 4 tests: single/multi server, hyphenated names, empty — `mcp_client.rs`
- [x] `call_tool()` — 6 tests: routing, unknown server, no prefix, hyphenated tools, overlapping prefixes — `mcp_client.rs`

### 7B: Multi-turn + tools + ask_user (depends on 5d + 7A)
- [x] Strom task tools via rig Tool trait (StromTaskTool)
- [x] Built-in ask_user tool (reuses Phase 5d suspended status)
- [x] agent_state conversation persistence in DB
- [x] Multi-turn dispatch loop with max_turns
- [x] Concurrent tool calls (parallel child jobs)
- [x] Tool definition generation from task input schemas

### 7C: MCP client tools (depends on 7B)
- [x] McpServerDef in workspace YAML (mcp_servers section)
- [x] MCP client manager via rmcp
- [x] MCP tool discovery + execution
- [x] Mixed sync (MCP) / async (task) tool calls

### OutputDef Unification Review Fixes (2026-03-20)

#### Critical
- [x] `OutputDef.properties` missing `#[serde(default)]` — fixed: added `#[serde(default)]` — `workflow.rs`

#### Important
- [x] Non-deterministic property order in `to_json_schema()` — fixed: changed `HashMap` to `BTreeMap` — `workflow.rs`
- [x] Output field type validation only runs for `type: agent` — fixed: moved to `validate_action()` for all types — `validation.rs`

#### Tests Added
- [x] Backward compat: YAML with `output_schema:` parses but `action.output` is `None` — `workflow.rs`
- [x] Empty properties: `to_json_schema()` on empty `properties` map produces valid schema — `workflow.rs`
- [x] All-required fields: every field `required: true`, verify `required` array fully populated — `workflow.rs`
- [x] Non-agent action with output: `type: script` + `output` with invalid type now rejected — `validation.rs`
- [x] `type: text` rejected on agent output: tests InputFieldDef/OutputFieldDef type divergence — `validation.rs`
- [x] Mixed-type `options` array round-trips through `to_json_schema()` — `workflow.rs`
- [x] `OutputDef` parses from YAML without `properties` key (serde default) — `workflow.rs`

### 7B: Multi-turn + tools + ask_user (depends on 5d + 7A)
- [x] Strom task tools via rig Tool trait (StromTaskTool)
- [x] Built-in ask_user tool (reuses Phase 5d suspended status)
- [x] agent_state conversation persistence in DB
- [x] Multi-turn dispatch loop with max_turns
- [x] Concurrent tool calls (parallel child jobs)
- [x] Tool definition generation from task input schemas

### 7C: MCP client tools (depends on 7B)
- [x] McpServerDef in workspace YAML (mcp_servers section)
- [x] MCP client manager via rmcp
- [x] MCP tool discovery + execution
- [x] Mixed sync (MCP) / async (task) tool calls

### 7D: Move Agent Dispatch to Workers (2026-03-23)

- [x] `stroem-agent` shared crate (config, state, tools, provider, dispatch, loop_dispatch, mcp_client)
- [x] Extended ClaimResponse with agent fields (provider name, rendered prompt, MCP servers, agent_state, task tool schemas)
- [x] 3 new worker API endpoints (task-tool, suspend, agent-state)
- [x] Worker-side AgentExecutor with WorkerAgentContext (HTTP-based AgentContext impl)
- [x] Agent step routing in worker poller (agent steps skip workspace download)
- [x] Claim SQL updated: agent steps claimable by workers with "agent" tag
- [x] `compute_required_tags("agent")` returns `["agent"]`
- [x] Server-side dispatch removed: deleted `crates/stroem-server/src/agent/` module entirely
- [x] `rig-core` removed as direct server dependency (transitive via stroem-agent for shared types)
- [x] `propagate_to_parent` handles `agent_tool` child completions: injects results into `resolved_tool_results`, marks step ready for re-claim
- [x] `approve_step` handles agent ask_user: injects response into `agent_state`, marks step ready for re-claim
- [x] `ResolvedToolResult` struct + `resolved_tool_results` field on `AgentConversationState`
- [x] Worker config: `agents: Option<AgentsConfig>` + `tags: ["script", "agent"]`

### 7D Review Fixes (2026-03-23)

#### Critical
- [x] `agent_suspend_step` status transition broken — fixed: single atomic SQL transitions from `running` to `suspended` while setting output — `worker_api/jobs.rs`

#### Important
- [x] Raw SQL UPDATEs lack status guards — fixed: added `AND status = 'running'` / `AND status = 'suspended'` guards — `job_recovery.rs`, `web/api/jobs.rs`
- [x] Task tool parameter schemas lost on worker side — fixed: `TaskToolInfo.parameters_schema` carries pre-built JSON Schema from server — `loop_dispatch.rs`, `agent_executor.rs`
- [x] `resolved_tool_results.drain(..)` before LLM call can lose data — fixed: uses `.iter().clone()` then `.clear()` after injection — `loop_dispatch.rs`
- [x] No tools-list validation on `agent_task_tool` endpoint — fixed: validates task_name against step's `action_spec.tools` array — `worker_api/jobs.rs`
- [x] Agent log events not pushed to server — fixed: `WorkerAgentContext::log` calls `client.push_logs()` — `agent_executor.rs`
- [x] `unwrap_or_default()` on `serde_json::to_value` silently destroys conversation state — fixed: uses `.context()?` — `job_recovery.rs`
- [x] Duplicated system prompt / response parsing logic — fixed: inline code replaced with calls to `build_effective_system` / `build_final_output` — `dispatch.rs`
- [x] MCP server auth tokens over-shared — fixed: filters to only MCP servers referenced by step's tools — `worker_api/jobs.rs`

#### Minor
- [x] Unused `_cancel_token` parameter — documented cancellation gap in doc comment — `agent_executor.rs`
- [x] Double `get_steps_for_job` in claim handler — fixed: reuses `all_steps_for_job` from earlier fetch — `worker_api/jobs.rs`
- [x] `.unwrap()` on hardcoded YAML in `build_tool_definitions` — changed to `.expect()` — `loop_dispatch.rs`
- [x] Prompt template render errors silently dropped — fixed: `tracing::warn!` on render failure — `worker_api/jobs.rs`
- [ ] No retry on transient HTTP errors in worker agent context methods (task-tool, suspend, agent-state) — `client.rs`
- [x] Deterministic "jitter" in retry backoff — documented as intentional trade-off — `dispatch.rs`
- [ ] No worker identity verification on new endpoints — any authenticated worker can call task-tool/suspend/agent-state for any job/step — `worker_api/jobs.rs`

### 7D Missing Tests

#### Critical (zero coverage on core logic)
- [ ] `dispatch_agent_loop` — happy path (no tools), task tool call, ask_user, resume with resolved_tool_results, max turns exceeded, cancellation. Needs mock `AgentContext` — `loop_dispatch.rs`
- [ ] `propagate_to_parent` agent_tool branch — completed child, failed child, partial resolution, missing agent_state fallback — `job_recovery.rs`
- [x] `execute_agent_step` early-return error paths — 6 tests: missing provider, missing config, unknown provider, empty/missing prompt, missing action_spec — `agent_executor.rs`

#### Important (new endpoints/paths with no tests)
- [ ] `agent_task_tool` / `agent_suspend_step` / `agent_save_state` handler tests — `worker_api/jobs.rs`
- [ ] `approve_step` agent ask_user path — valid agent_state, missing agent_state fallback, wrong action_type — `web/api/jobs.rs`
- [ ] Worker client methods — `agent_task_tool`, `agent_suspend_step`, `agent_save_state` request/response format — `client.rs`

#### Medium (public function gaps)
- [x] `build_effective_system` — 4 tests: all combinations of system_prompt ± output schema — `dispatch.rs`
- [x] `build_final_output` — 7 tests: structured output, code fences, non-object error, invalid JSON, unstructured fallback — `dispatch.rs`
- [ ] Worker poller agent routing — workspace download skipped, WaitingForTools/Suspended don't report completion — `poller.rs`
- [x] `state.rs` — 3 tests: `resolved_tool_results` round-trip, empty omission, backward compat — `state.rs`

## Phase 5: Advanced Flow Control

- [x] 5a: Conditional flow steps (`when` expressions) — model, template evaluation, orchestrator, DB migration, validation, API, UI, docs, tests
- [x] 5b: For-each loops (fan-out/fan-in) — `for_each` + `sequential` on FlowStep, expand/check/aggregate in job_creator.rs, `each` variable, DB migration 022, validation, API, UI, docs
- [x] 5b review fixes (see Review section below)
- [ ] 5c: While loops (retry-until patterns)
- [x] 5d: Approval gates — `type: approval` action, `suspended` step status, `message` Tera template, `on_suspended` hooks, approve/reject API, recovery timeout, cancellation, frontend approval card

### 5d Review Fixes (2026-03-20)

#### Critical
- [x] Race condition: `approve_step`/`reject_step` repo methods with `WHERE status = 'suspended'` guard — `job_step.rs`
- [x] `on_suspended` hooks fire for root approval steps — `fire_initial_suspended_hooks()` called from all callers (tasks.rs, scheduler.rs, hooks.rs, mcp/tools.rs) — `job_creator.rs`
- [x] `on_suspended` hooks fire from `propagate_to_parent` — snapshot-diff pattern added — `job_recovery.rs`

#### Important
- [x] Input validation on approve — required fields enforced server-side against action's input schema — `jobs.rs`
- [x] `approval_message` preserved after approve — output merged not overwritten — `jobs.rs`
- [x] `rejection_reason` truncated to 4096 bytes (UTF-8 safe) — `jobs.rs`
- [x] `approval_message`/`approval_fields` surfaced unconditionally for `action_type == "approval"` — `jobs.rs`

#### Minor
- [x] Frontend: client-side validation of required approval fields before submit — `approval-card.tsx`
- [x] Frontend: `options` field renders as ComboboxField — `approval-card.tsx`
- [x] Frontend: `boolean` field renders as Checkbox — `approval-card.tsx`
- [x] Frontend: static indicator on suspended badge (no pulse) — `status-badge.tsx`
- [x] Frontend: "Waiting since" shown on suspended steps — `approval-card.tsx`
- [x] Warning log when approval message template is missing — `job_creator.rs`
- [x] Design doc migration number stale (says 023, actual is 024) — fixed in phase5d-approval-gates.md

#### Tests
- [x] Integration: approval happy path (suspend → approve → downstream runs)
- [x] Integration: rejection path (reject → step fails → downstream skipped → job fails)
- [x] Integration: cancel job with suspended step → step cancelled
- [x] Integration: root step suspends immediately
- [x] Integration: approval message in API response
- [x] Unit: `approve_step` returns 409 for non-suspended step
- [x] Unit: `approve_step` returns 404 for missing step
- [x] Unit: rejection reason truncation (ASCII, multi-byte UTF-8, exact boundary)
- [x] Unit: approval output merge preserves message
- [ ] Integration: approval timeout → recovery sweeper fails step (needs mock time)
- [ ] Integration: `on_suspended` hooks fire (needs hook workspace setup)
- [ ] Unit: `approve_step` returns 403 for View-only user (needs auth setup)
- [ ] Unit: approval step with `when: false` is skipped not suspended
- [ ] Unit: `for_each` + approval: `each` variable in message template rendering

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

## Review: Phase 5b For-Each Loops

### Critical
- [x] `type: task` + `for_each` broken — `propagate_to_parent` does not call `check_loop_completion` or `expand_for_each_steps` on parent job. Fixed: wired both into `propagate_to_parent` — `job_recovery.rs`
- [x] Non-atomic expansion — `expand_for_each_steps` did INSERT + N UPDATEs + mark_running separately. Fixed: added loop fields to `NewJobStep`/`StepInsertRow`, single INSERT — `job_step.rs`, `job_creator.rs`
- [x] DAG renders loop instance steps — Fixed: filter `loop_source !== null` before building nodes — `workflow-dag.tsx`

### Important
- [x] `mark_running_server` has no status guard — Fixed: added `AND status = 'pending'` — `job_step.rs`
- [x] Cancellation doesn't handle for_each placeholders — Fixed: added `cancel_server_managed_steps()` for running steps with `worker_id IS NULL` — `job_step.rs`, `cancellation.rs`
- [x] Progress badge only counts `completed` — Fixed: counts all terminal, shows failed count separately — `step-timeline.tsx`
- [x] `instancesExpanded` state resets on re-render — Fixed: lifted to `StepTimeline` level, keyed by step name — `step-timeline.tsx`
- [x] Dead `placeholderNames` set — Fixed: removed — `step-timeline.tsx`
- [x] Missing index on `loop_source` — Fixed: added partial index — `022_for_each.sql`
- [x] `skip_unreachable_steps` doesn't guard for_each placeholders — Fixed: added `for_each_expr.is_some()` guard — `job_step.rs`
- [x] `set_loop_metadata` removed — no longer needed after atomic expansion fix
- [x] Aria: "show N iterations" button — Fixed: added `aria-expanded` — `step-timeline.tsx`
- [x] `indented` padding — Fixed: uses `cn()` — `step-timeline.tsx`
- [x] `for_each` display truncation — Fixed: shows `[N items]` for arrays > 5 — `task-detail.tsx`

### Minor
- [x] `each.total` not exposed — Fixed: added to template context — `rendering.rs`, `job_creator.rs`
- [x] Instance step names leak into `build_step_render_context` — Fixed: filtered out `loop_source.is_some()` — `job_creator.rs`
- [x] `for_each_expr` stores JSON-serialized value — Fixed: stores raw string for templates, JSON for arrays — `job_creator.rs`
- [ ] `loop_total` redundantly stored per-instance row — derivable from COUNT, can drift on partial retries — `022_for_each.sql`
- [x] No empty-loop state in UI — Fixed: shows "0 iterations" badge — `step-timeline.tsx`

### Missing Test Coverage
- [ ] Integration: parallel for_each expansion + completion + output aggregation
- [ ] Integration: sequential for_each — promotion chain + failure stops remaining
- [ ] Integration: `type: task` + `for_each` (child job per instance, propagation)
- [ ] Integration: for_each with `continue_on_failure: true` and some failed instances
- [ ] Integration: cancellation mid-loop (all instances + placeholder cleaned up)
- [ ] Integration: recovery sweep with for_each instances
- [ ] Integration: concurrent expansion idempotency
- [ ] Integration: for_each root step (no deps)
- [ ] Integration: nested for_each (loop depends on loop)
- [ ] Integration: for_each with `when` condition false → step skipped without expansion
- [ ] Integration: for_each with empty array → step skipped
- [ ] Integration: for_each with dynamic template expression from upstream output
- [ ] Unit: `check_loop_completion` output aggregation ordered by loop_index

### Minor
- [x] Test 12 comment says "skipped as unreachable" but cascade-skip now happens in `promote_ready_steps` — update comment — `orchestrator_test.rs`

## Review: KubeRunner Log Stream Reconnection

### Critical
- [x] Dedup only skips 1 line but `sinceTime` is second-precision — Fixed: HashSet dedup over entire last second, timestamps enabled on all connections — `kubernetes.rs`

### Important
- [x] `parse_k8s_timestamp` failure silently drops `since_time` — Fixed: log warning + fall back to `since_seconds: Some(5)` — `kubernetes.rs`
- [x] No exponential backoff — Fixed: `min(1 << attempt, 30)` seconds — `kubernetes.rs`
- [x] `stderr_lines = stderr` overwrites termination reason — Fixed: `stderr_lines.extend(stderr)` — `kubernetes.rs`

### Minor
- [x] Lines without space in reconnect mode — Fixed: always use timestamps, consistent parsing — `kubernetes.rs`
- [x] `saw_any_line` semantics — Fixed: renamed to `received_any_data`, set before dedup skip — `kubernetes.rs`

### Test Coverage
- [x] Unit: `split_timestamp_line` normal + no-space + empty
- [x] Unit: `truncate_to_second` with nanos, no fraction, millis, no-Z
- [x] Unit: `parse_k8s_timestamp` valid RFC3339, valid no-nanos, invalid
- [ ] Integration: dedup with multiple lines at same timestamp (needs mock K8s API)
- [ ] Integration: clean EOF path, max reconnect exhaustion (needs mock K8s API)

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
- [x] Event source triggers (Phase 5e): `type: event_source` trigger with stdout JSON-line protocol, exponential backoff restart policy, backpressure via `max_in_flight`, EventSourceManager background task
- [ ] Structured `ask_user` input: allow agent to pass a JSON schema with `ask_user` so the UI renders form fields instead of free-text (same pattern as approval gate `approval_fields`)
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

## Review: Remove `cmd` from `type: script` (2026-03-17)

### Critical
- [x] `docs/public/llms.txt` line ~15 says `type: script` requires `cmd` or `script` — must say `script` or `source` (auto-generated from doc sources, fix source + regenerate)
- [x] `docs/public/llms.txt` line ~420 states `cmd:` is still accepted on `type: script` via backward compat — factually wrong, remove
- [x] `docs/src/content/docs/reference/worker-api.md` line ~91 — worker API example shows `"cmd"` in `action_spec` for a script step — external worker implementations will use wrong key
- [x] `workspace-ops/.workflows/healthcheck.yaml` and `notify.yaml` — live workflow YAML files using `type: shell` + `cmd:`, will fail validation on load
- [x] `crates/stroem-server/src/workspace/library.rs` line ~1040 — test fixture creates `type: script` + `cmd: echo hi`

### Important
- [x] `crates/stroem-worker/src/executor.rs` lines 211-213 — old in-flight jobs with `"cmd"` in action_spec get misleading error "must contain 'script' or 'source'"; should detect `cmd` key and provide actionable message
- [x] `crates/stroem-server/tests/integration_test.rs` — test `test_cmd_rendering_failure_fails_step` has stale name and 3+ comments referencing `cmd` instead of `script`

### Minor
- [x] `crates/stroem-common/src/validation.rs` line ~1172 — test named `test_validate_action_script_cmd_deprecated_warning` but now tests error, not warning
- [x] No explicit executor test for `type: docker` + `cmd` flowing through to `RunConfig.cmd`

## Review: Skipped Job Status (2026-03-18)

### Important
- [x] Scheduler: double serialization of `tstate.input` + `.ok()` inconsistency — reuse `input.clone()` from line 266 instead of re-serializing

### Test Coverage
- [x] Integration: `create_skipped()` DB test (testcontainers) — assert `completed_at IS NOT NULL`, `started_at IS NULL`, `status = 'skipped'`, no steps, not counted as active, included in retention sweep, appears in status counts
- [ ] E2E: cron trigger with `concurrency: skip` + active job → verify skipped job appears in `GET /api/jobs?status=skipped` and `GET /api/stats` shows `skipped >= 1`

## Data Retention (review findings 2026-03-18)

- [x] FK violation on `parent_job_id`: migration 020 adds `ON DELETE SET NULL` to `job.parent_job_id`, `job.worker_id`, `job_step.worker_id`.
- [x] FK violation on `job.worker_id` and `job_step.worker_id`: same migration.
- [x] Unbounded result set: `get_old_terminal_jobs()` now takes `batch_size` param with `LIMIT` clause (default 1000).
- [x] N+1 query: returns `RetentionJobInfo` struct with metadata directly, eliminating per-job `get()`.
- [x] Zero-value config validation: `worker_retention_hours` and `log_retention_days` must be >= 1 if set.
- [x] Retention interval: `retention_interval_secs` config (default 3600) with `last_retention_run` atomic tracking in AppState.

## Review: Event Source Triggers (2026-03-27)

### Critical
- [x] `source_id` format mismatch: worker now builds source_id from action_spec `workspace` + `action_name` to match server's `"{workspace}/{trigger_name}"` format.
- [x] Worker now calls `report_step_start()` at the start of `execute_event_source` so jobs transition to `running`.
- [ ] Resolved secrets stored in plaintext in `action_spec` JSON in `job_step` table. Consistent with regular step behavior but longer exposure window. TODO added for resolving at claim time.

### Important
- [x] Fingerprint now includes `task`, `input`, `restart_policy`, `backoff_secs`, `max_in_flight`.
- [x] Duplicate active jobs: reconciliation HashMap changed to `Vec` per source_id, duplicates cancelled.
- [x] Semaphore-full race: worker now reports step failure when event source semaphore is full.
- [x] Step completion: supervision loop now reports step completion to server on exit (all paths).
- [x] Worker shutdown now drains both regular and event source semaphores.

### Minor
- [x] `max_in_flight` backpressure implemented via tokio::sync::Semaphore in stdout processing.
- [x] K8s pod name now includes random UUID suffix to avoid collision on restart.
- [ ] K8s pod log stream can't distinguish stdout from stderr — known limitation of `log_stream` API.
- [x] Minimum 1s delay on success restart to prevent tight-loop spinning.
- [x] `merge_input` doc corrected from "deep-merge" to "shallow-merge".
- [x] 256KB body size limit added to emit endpoint via `DefaultBodyLimit`.
- [ ] No rate limiting on emit endpoint — mitigated by `max_in_flight` backpressure on worker side.
- [x] Docker containers now have `no-new-privileges` security option.
- [x] Emit endpoint validates `source_id` against workspace triggers (type, enabled, target task).

### Missing Tests
- [x] `compute_fingerprint` — determinism, order-independence, field sensitivity, None vs empty (6 tests)
- [x] `reconcile` integration tests — create/replace/skip/cancel branches (5 integration tests)
- [x] `emit_event` HTTP handler — happy path, 404, auth, invalid source_id (4 integration tests)
- [ ] `process_stdout_lines` — valid JSON, malformed, empty lines, cancellation, EOF (requires mock client)
- [x] Backoff formula — extracted to `compute_backoff_delay` with 5 unit tests
- [x] `max_event_sources` config — default value (5), explicit value (2 unit tests)
- [ ] Poller event source semaphore branching (requires mock server, lower priority)
- [x] DB migration — source_type + action_type CHECK constraints (2 integration tests)
- [x] `restart_policy_str` round-trip for all three variants

## Bugs Found & Fixed

- [x] Workspace-level hooks not firing for authenticated API jobs — `source_type = "user"` missing from `is_top_level` check (v0.5.9)
- [x] Template render errors passed raw templates instead of failing steps (v0.5.8)
- [x] Worker `report_step_start` sent no JSON body → steps stuck
- [x] Worker `push_logs` sent wrong format → logs never stored
- [x] Tera hyphen bug: step names with hyphens → sanitize to underscores in template context
- [x] Relative script paths don't resolve via `current_dir`

## Retry Mechanism Review (Phase 5f, 2026-04-08)

### Critical (must fix)
- [x] `agent_state` not cleared on step retry — added `agent_state = NULL, suspended_at = NULL` to `reset_for_retry` SET clause
- [x] Task-level retry fires on child jobs — added `parent_job_id.is_none()` guard before `try_retry_job`
- [x] Claim query index regression — updated NOT IN list to `('task', 'agent', 'approval', 'event_source')` matching GIN index predicate
- [x] FK `retry_of_job_id` without `ON DELETE SET NULL` — changed to `ON DELETE SET NULL` in migration

### Important (should fix)
- [x] Loop instance retry vs placeholder state — moved retry intercept before `check_loop_completion` so failed instances with retries get reset before placeholder aggregation
- [x] Deterministic jitter — replaced with `rand::random::<u64>() % jitter_max`
- [x] `try_retry_job` not transactional — wrapped set_retry_fields + set_retry_job_id + retry_at UPDATE in `pool.begin()` / `tx.commit()` transaction
- [x] Missing index on `retry_of_job_id` — added `CREATE INDEX idx_job_retry_of ON job(retry_of_job_id) WHERE retry_of_job_id IS NOT NULL`
- [x] "waiting for retry" badge frozen in UI — extended polling guard with `hasRetryPending` check

### Minor
- [ ] TOCTOU between `get_step` and `reset_for_retry` — log message/delay based on stale data (DB state is correct, low practical risk). (`crates/stroem-server/src/job_recovery.rs`)
- [x] `ready_at` set to future timestamp on retry — changed to `ready_at = NOW()` (retry_at still controls claimability)
- [x] No unit tests for `compute_retry_delay` — added 5 tests: fixed, exponential, capped, default strategy, jitter bounds
- [x] Retry badge accessibility — extended step row `aria-label` with attempt/retry info
- [x] Retry history React key not scoped to parent step — changed to `retry-${attempt.attempt}`

### Missing Tests
- [x] `compute_retry_delay` unit tests: fixed delay, exponential delay, min(6) cap, jitter output
- [x] Step retry integration test: `test_step_retry_resets_failed_step` — fail → reset → retry_attempt incremented → retry_history populated
- [x] Retries exhausted integration test: `test_step_retry_exhausted_fails_job` — step stays failed, job fails
- [x] Claim respects `retry_at`: `test_step_retry_claim_respects_retry_at` — step not claimable before backoff expires
- [x] Task retry integration test: `test_task_retry_creates_new_job_on_failure` — job fails → new retry job created → linked properly
- [x] Task retry exhausted: `test_task_retry_exhausted_fires_hooks` — `on_error` hooks fire only on final failure
- [x] `continue_on_failure` + step retry: `test_step_retry_with_continue_on_failure` — retry first, `continue_on_failure` after exhaustion
- [x] Step retry success on second attempt: `test_step_retry_success_on_second_attempt`
- [x] Child job retry blocked: `test_task_retry_child_job_no_retry` — `type: task` child jobs do not trigger task-level retry
- [x] Edge case: `test_retry_edge_zero_max_retries_no_retry_job` — max_retries=0 means no retry

### Documentation
- [x] Update `docs/src/content/docs/reference/workflow-yaml.md` with retry fields for step, action, and task
- [x] Add retry guide/section to `docs/src/content/docs/guides/retry.md`
- [x] Regenerate `docs/public/llms.txt` (via generate-llms-txt.ts update + docs build)

## Task State Snapshots Review (Phase 6a, 2026-04-08)

### Critical (must fix)
- [x] `STATE:` lines read from already-drained log buffer — always empty. Fixed: collect STATE: lines before the final drain.

### Important (should fix)
- [x] Kubernetes state volumes are emptyDir but never populated — documented as known limitation with comment in kubernetes.rs
- [x] Orphaned archive blobs on DB insert failure — added compensating delete on insert failure
- [x] `extract_state_json` aborts scan on any tarball entry error — changed to `continue` on entry errors
- [x] S3 `retrieve` swallows all errors as "not found" — now returns `Err` for non-NoSuchKey errors
- [x] `insert` + `prune` not atomic — added `insert_and_prune` method with transaction wrapping
- [x] Docker NoWorkspace mode sets `STATE_DIR` env var but doesn't mount state dirs — guarded env var injection with `WithWorkspace` check

### Minor
- [x] Non-deterministic ordering on timestamp ties — added `id DESC` tiebreaker to all ORDER BY clauses
- [x] TempDir creation failure silently disables state — added `warn` log on failure
- [ ] Blocking tar/gzip I/O in async context — acceptable for small snapshots, consider `spawn_blocking` for large ones. (`crates/stroem-worker/src/poller.rs:55-81`)
- [ ] No decompressed size limit — acceptable given 50MB upload limit. Low risk.
- [x] `InMemoryStateArchive` uses `.unwrap()` on poisoned RwLock — changed to `unwrap_or_else(|e| e.into_inner())`
- [x] `storage_key` function does not sanitize workspace/task_name — added `..` → `__` sanitization

### Missing Tests
- [x] `TaskStateRepo` integration tests — 6 tests: insert+get_latest, get_by_id, list, prune, delete_all, FK ON DELETE SET NULL
- [ ] Worker API state endpoint tests — upload happy path, download 204/200, job ownership validation, pruning, storage-not-configured 404, stale DB reference → 204
- [x] Tera state injection test — 2 tests: with state_json Some and None
- [x] `extract_state_json` unit tests — 7 tests: valid, missing, invalid JSON, empty bytes, corrupt gzip, multiple files, nested path
- [x] `build_state_tarball` / `extract_state_tarball` roundtrip test — 5 tests: dir_has_content, tarball roundtrip, malformed, empty dir
- [x] `LocalStateArchive` unit tests — 8 tests: store/retrieve, missing key, delete, path traversal, InMemoryArchive, key format
- [x] Docker runner state mount test — 2 tests: with state dirs, without state dirs
- [x] Kube runner state volume test — 2 tests: with state dirs, without state dirs
- [ ] STATE: line merge logic test — extract merge logic into testable function
- [ ] Concurrent upload race condition test — two uploads for same task, verify invariant after both prune calls

### Documentation
- [x] Add user-facing docs in `docs/src/content/docs/guides/task-state.md` — usage, STATE: protocol, /state mount, examples
- [x] Regenerate `docs/public/llms.txt` (added task-state.md to generate-llms-txt.ts sections)

## Global Workspace State Review (Phase 6a.2, 2026-04-10)

### Important (should fix)
- [x] `WorkspaceStateRepo` integration tests — 3 tests added: insert+get_latest, insert_and_prune, delete_all
- [x] Docker runner test for global state bind mounts — `test_global_state_dir_bind_mount_with_workspace`
- [x] Kube runner test for global state volumes — `test_global_state_volumes_present_when_global_state_dir_set` + `test_no_global_state_volumes_when_global_state_dir_none`

### Minor
- [x] Duplicate lock acquisition for STATE: + GLOBAL_STATE: collection — combined into single pass
- [x] Shared `max_snapshots` config — added `global_max_snapshots: Option<usize>` to `StateStorageConfig` and `StateStorage`, with `global_max_snapshots()` fallback accessor
- [x] `task_name` column in `workspace_state` — renamed to `written_by_task` with SQL COMMENT
- [x] Redundant `is_some()` + `if let Some` pattern — collapsed to single `if let Some(ref storage)` for both task and global state lookups
- [x] "Deep-merge" comment is misleading — changed to "Shallow merge (top-level key overwrite)"
- [x] `max_snapshots` doc comment on `StateStorage` — updated to say "per task", added doc for `global_max_snapshots()`

### Missing Tests
- [x] `WorkspaceStateRepo` integration tests — insert+get_latest, insert_and_prune, delete_all isolation
- [x] Docker global state bind mount test — verifies `/global-state:ro` and `/global-state-out:rw`
- [x] Kube global state volume test — verifies `global-state-vol`/`global-state-out-vol` present and absent
- [x] State merge logic tests — extracted `merge_state_entries()` function with 6 unit tests: empty, single object, multiple distinct keys, last-writer-wins, non-objects skipped, all non-objects returns None

## Future: Snapshot Management API (follow-up to archive-upload v1)

**v1 status**: shipped on branch `feat/state-upload` (commits `7d12461`..`09d5507`). Design: `docs/internal/2026-04-21-state-upload-design.md`. Plan: `docs/internal/2026-04-21-state-upload-plan.md`. E2E test added but **not yet run** — operator should run `./tests/e2e.sh` before merging.

Shipping v1 as archive-only upload (`POST /api/workspaces/{ws}/tasks/{task}/state` and `POST /api/workspaces/{ws}/state`, whole gzip tarball → one new snapshot, synthetic `source_type="upload"` job for audit). These are the follow-ups for a richer snapshot-management experience:

- [ ] **Multi-file upload in one request** (option β) — `multipart/form-data` with `file=@...` (repeatable), `state_json=...`, `base=latest|empty`, `merge_json=true|false`. Server unpacks latest (if `base=latest`), overlays uploaded files, deep-merges `state_json`, repacks as new snapshot. Subsumes archive-only case (a single `tar` field could carry a pre-built archive).
- [ ] **Individual file download** — `GET /api/workspaces/{ws}/tasks/{task}/state/files/{path}` to retrieve one file from the latest snapshot without pulling the whole tarball. Symmetric `GET /api/workspaces/{ws}/state/files/{path}` for global.
- [ ] **State JSON read/patch** — `GET /api/workspaces/{ws}/tasks/{task}/state/json` returns the current `state.json`; `PATCH` with a JSON merge-patch document creates a new snapshot with merged values. Parallels global.
- [ ] **List snapshot contents** — `GET /api/workspaces/{ws}/tasks/{task}/state/files` returns `[{path, size, sha256}]` for the latest snapshot. Useful for UI and debugging.
- [ ] **Snapshot history + rollback** — `GET /api/workspaces/{ws}/tasks/{task}/state/snapshots` lists prior snapshots with IDs; `POST .../state/snapshots/{id}/restore` creates a new snapshot copied from an older one (for "oops, revert").
- [ ] **Delete-files directive** — form field `delete_paths=a.pem&delete_paths=b.json` in β multipart removes listed files from the `base=latest` overlay before repacking.
- [ ] **If-Match / content-hash guard** — optional header `If-Match: <snapshot_id>` on mutating calls, rejects with 409 if the snapshot has advanced since the client last read. Prevents last-writer-wins surprises when multiple operators/automations touch the same task state.
- [ ] **Session-based editor** (option γ) — only if β+download+patch prove insufficient. `POST /state/edit` opens a session, PUT/DELETE files, PATCH JSON, `POST commit` materializes one snapshot. Server-side scratch dir + TTL + cleanup. Probably overkill; defer until a concrete use case demands it.
- [ ] **UI affordance** — drag-drop file upload on task detail page, JSON editor for `state.json`, diff viewer against previous snapshot.

## State Upload v1 Review (2026-04-22)

Findings from parallel review agents (code-reviewer + security-auditor + database-optimizer + qa-expert) on branch `feat/state-upload`. Verdict: Needs Attention before merge. No critical exploitable vulnerabilities given auth-gated access, but several important correctness/security issues and test coverage gaps.

### Important — fix before merge

- [x] **Decompression bomb in `unpack_tarball`** — `crates/stroem-server/src/web/api/state_upload.rs:161`. `Vec::with_capacity(entry.size() as usize)` + `read_to_end` has no per-entry or aggregate cap. A ~4 MB gzip can balloon to gigabytes on unpack before the 50 MB post-repack check fires. **Fix**: use `entry.take(MAX_SNAPSHOT_BYTES as u64 + 1)` or accumulate a running byte count and error past the cap. Apply to both uploaded and prior-snapshot unpack paths.
- [ ] **`source_id` misreports API-key auth as user email** — `state_upload.rs:295` (`resolve_source_id`). Always emits `user:{email}` regardless of auth method, contradicting design spec §"Synthetic seed job" (spec says `api_key:{key_prefix}` for API keys). Audit trail can't distinguish SSO sessions from CI pipelines. **Fix**: branch on `auth_user.is_api_key` and emit `api_key:{first 8 chars}` for API-key callers.
- [ ] **Dead `insert_synthetic_upload_job` function** — `state_upload.rs:207-241`. `pub async fn` tagged `#[allow(dead_code)]`, disconnected from both handlers (which inline their own INSERT). Signature diverges from what actually runs. **Fix**: either delete entirely or refactor both `commit_*` functions to call it. The `#[allow(dead_code)]` is hiding what would otherwise be a compiler warning.
- [ ] **`has_root_state_json` double-decompresses the repacked tarball** — `state_upload.rs:266-270, 458`. Called from both commit functions after `build_snapshot` already unpacked+repacked. **Fix**: return `has_json: bool` alongside bytes from `build_snapshot` directly. Performance + correctness risk (two unpack paths could disagree).
- [x] **ACL silent pass-through when `auth_user` is None + ACL configured** — `state_upload.rs:311` (`check_run_permission`). The outer `if let Some(auth) = auth_user` skips the entire ACL check when `auth_user` is None, even if `state.acl.is_configured()`. Currently shielded by upstream `require_auth`, but any middleware reordering creates an authz bypass. **Fix**: if ACL is configured and `auth_user.is_none()`, return `Unauthorized`.
- [x] **`tracing::instrument` doesn't skip `params`** — `state_upload.rs:334, 548`. Query param values (user-supplied, potentially secrets despite docs warning) land in tracing spans at DEBUG. No redaction backstop. **Fix**: add `params` to `skip(...)` list on both handler attributes.
- [x] **Path traversal in tarball entries accepted** — `state_upload.rs:152-157` (`unpack_tarball`). Strips `./` but does not reject `..` components or absolute paths. A malicious `../../etc/passwd` entry gets stored verbatim in the archive. Workers are shielded by container boundaries today, but a future CLI `download` command could be tricked into writing outside the intended directory. **Fix**: in `unpack_tarball`, reject any entry whose path contains `..` as a component or starts with `/`.

### Missing tests — add alongside fixes

- [x] **51 MB body returns 413** — untested; `DefaultBodyLimit` layer is wired but never exercised by an integration test. Add to `tests/state_upload_test.rs`. Synthesise `vec![0u8; 50 * 1024 * 1024 + 1]` as raw body.
- [x] **Malformed/corrupted gzip body returns 400** — untested. Send non-gzip bytes (`b"not gzip"`) and assert `BAD_REQUEST`. Add to `tests/state_upload_test.rs`.
- [x] **Path-traversal tarball entry** — no test asserts behavior (rejection vs sanitisation). Add unit test to `state_upload.rs` after deciding on Important #7's fix.
- [x] **Symlink/hardlink tarball entries are skipped** — code path at `state_upload.rs:148-150` exists but untested. Use `tar::Header::new_gnu()` with `entry_type` set to `tar::EntryType::Symlink`. Add to unit tests.
- [ ] **Merge with prior state.json + no query params preserves prior verbatim** — the "read-modify-rewrite with no changes" case. Add to unit tests in `state_upload.rs`.
- [ ] **Merge with empty-prior (no state.json) + new query params creates state.json** — specific combination untested. Add to unit tests.
- [ ] **Missing workspace → 404** for both endpoints. Add to integration tests.
- [ ] **Missing task → 404** for task endpoint. Add to integration tests.
- [ ] **State storage not configured → 404** — `build_test_app` always wires it. Add a `build_test_app_no_storage` variant and one test per endpoint.
- [ ] **Max_snapshots pruning removes archive blobs on disk** — current `upload_merge_mode_preserves_prior_files` counts rows but doesn't verify physical blob deletion. Upload 6 snapshots with `max_snapshots=5`, verify 5 rows + 1 blob deleted from the local archive dir. Account for the fire-and-forget `tokio::spawn` prune (short sleep/yield after the 6th upload).
- [ ] **API-key `source_id` format** — paired with Important #2 fix. After adding `api_key:{prefix}` path, add a test using `strm_…` token and assert `source_id` starts with `api_key:`.
- [ ] **Non-admin user hitting global endpoint returns 403** — deferred during Task 6 (needs a `build_test_app_with_auth` variant that enables JWT). Adds the only coverage for the `if !auth.is_admin()` branch — currently an auth-bypass risk if the code is ever refactored.
- [ ] **`GET /api/jobs?source_type=upload` filter returns upload jobs** — audit-trail goal from the design spec. First verify `ListJobsQuery` even supports a `source_type` filter; if not, add the filter or adjust the audit narrative. Add integration test either way.

### Minor — nice to have

- [ ] **CLI swallows response-body read errors** — `crates/stroem-cli/src/remote/state.rs:69`. `response.text().await.unwrap_or_default()` gives user "upload failed: 500 — " with no detail. Use `.context("read response body")?` or log the error.
- [ ] **CLI basename collision silently overwrites** — `remote/state.rs:86-91`. `./certs/cert.pem` and `./backup/cert.pem` both map to `cert.pem`; second wins. Add a duplicate-basename check with a clear error, or at minimum a `tracing::warn!`.
- [x] **Prior snapshot fetch in merge mode has no size cap** — `state_upload.rs:373`. Low risk (own storage), but a defensive `bytes.len() <= MAX_SNAPSHOT_BYTES` assertion after retrieval would be robust against a mis-sized snapshot written by a past/future version.
- [ ] **Route `DefaultBodyLimit` layer applied per-route instead of router-level** — `crates/stroem-server/src/web/api/mod.rs:264-273`. Inconsistent with `worker_api/mod.rs:92` pattern. Low severity.
- [ ] **SQL duplication between handlers and `insert_and_prune` repo methods** — `state_upload.rs:445, 664` vs `crates/stroem-db/src/repos/task_state.rs:90` + `repos/workspace_state.rs:88`. Acknowledged trade-off (handlers need the same tx to include the synthetic job INSERT). Long-term fix: refactor the repo methods to accept `&mut Transaction<'_, Postgres>` instead of `&PgPool`, then handlers can delegate. Non-blocking.
- [ ] **`UPDATE job SET output` round-trip could be eliminated** — `state_upload.rs:535, 758`. Generate `snapshot_id` before the tx so the initial `INSERT INTO job` can carry the final `output` directly. Saves one statement per upload. Non-blocking optimization.
- [ ] **No index on `job.source_type`** — any audit query filtering by `source_type = 'upload'` does a seq scan today. Also true for `'retry'`, `'hook'`, etc. Add only if query patterns warrant.
- [ ] **CLI: `state.json` passed as a file** — CLI `build_tarball` happily packs it; server rejects with 400. Add a client-side guard for a clearer error.
- [ ] **CLI: paths with spaces / unicode in filename** — `path.file_name().to_string_lossy()` handles them; add test for documentation.
- [ ] **Concurrent uploads for same (ws, task) stress test** — add `// TODO(test): concurrent upload stress` near `commit_task_upload` for when fault-injection tooling exists.
- [ ] **E2E suite not yet run end-to-end** — `tests/e2e.sh` additions landed but weren't executed (requires docker-compose build cycle). Operator should run `./tests/e2e.sh` before merging.

### Cleared by review (non-issues)

- Migration 030 is safe and consistent with previous `source_type` CHECK migrations (005, 019, 025, 026, 027). `IF EXISTS` guard makes it idempotent.
- Pruning correctness: the code uses sequential statements (not CTEs). PostgreSQL sees prior-statement effects across a transaction, so the prune `ORDER BY created_at DESC OFFSET max_snapshots` correctly preserves the newly-inserted row (it's the newest by `created_at`).
- Transaction ordering: archive write → DB tx → compensating storage.delete on tx failure is correct. Compensating path fires for `pool.begin()` failures as well.
- `ON DELETE SET NULL` retention interaction: when the job-retention sweeper prunes a synthetic upload job, state rows become orphaned with `job_id = NULL`. Existing readers in `TaskStateRepo` / `WorkspaceStateRepo` already type `job_id` as `Option<Uuid>`. No regression.
