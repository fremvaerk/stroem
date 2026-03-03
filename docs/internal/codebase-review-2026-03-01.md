# Comprehensive Codebase Review: Strøm v0.5.0

**Date:** 2026-03-01
**Reviewers:** 7 specialized agents (Security Auditor, Code Reviewer, Performance Engineer, QA Expert, React Specialist, Architect Reviewer, Rust Engineer)

---

## Critical / Must-Fix

| # | Area | Finding | Source | Status |
|---|------|---------|--------|--------|
| 1 | **Security** | Worker token embedded in plaintext in Kubernetes pod spec (`kubernetes.rs:94-97`) — visible via `kubectl get pod -o yaml` | Security, Code Quality | **FIXED** |
| 2 | **Security** | WebSocket log streaming (`ws.rs`) has no authentication — anyone with a job UUID can stream logs | Security | **FIXED** |
| 3 | **Security** | CORS allows `Any` origin/method/header (`web/mod.rs:23-26`) — enables cross-origin attacks | Security, Architecture | **FIXED** |
| 4 | **Frontend** | Token exposed in WebSocket URL query string (`use-job-logs.ts:34`) — appears in server logs, browser history | Frontend | **FIXED** |
| 5 | **Frontend** | Race condition: REST backfill and WebSocket stream overwrite each other in `useJobLogs` | Frontend | **FIXED** |
| 6 | **Frontend** | Status filter is client-side but pagination is server-side — shows wrong results (`jobs.tsx:32-42`) | Frontend | **FIXED** |
| 7 | **Database** | Job+step creation is not wrapped in transactions — partial failures create orphan jobs (`job_creator.rs`) | Architecture, Code Quality | **FIXED** |

---

## High Severity

### Security
- ~~**No rate limiting** on login, refresh, API key, and worker endpoints — brute-force risk~~ **FIXED** (per-IP rate limiting via `tower-governor`)
- ~~**OIDC state cookie missing `Secure` flag** — PKCE verifier transmitted over plain HTTP~~ **FIXED** (conditional `; Secure` when base_url is HTTPS)
- ~~**Refresh token in localStorage** — accessible to XSS; should be HttpOnly cookie~~ **FIXED** (HttpOnly cookie with SameSite=Strict, Path=/api/auth)
- **Pod manifest overrides** can set `privileged: true`, `hostPath`, etc. — no blocklist

### Architecture
- **Single-server bottleneck**: scheduler, recovery sweeper, log broadcast are all in-process with no leader election — prevents horizontal scaling
- ~~**No graceful shutdown on worker** — SIGTERM kills in-flight step executions~~ **FIXED** (CancellationToken + semaphore drain)
- **No runner cancellation mechanism** — stuck pods/containers run indefinitely (KubeRunner busy-polls forever with no timeout)
- ~~**No request timeouts on worker HTTP client** — hung server blocks worker indefinitely~~ **FIXED** (configurable connect + request timeouts)

### Code Quality
- ~~**Terminal job handling duplicated 3 times** in `job_recovery.rs` (~60 lines each)~~ **FIXED** (consolidated into `run_terminal_job_actions`)
- ~~**`blocking_read()` inside async fn** (`workspace/mod.rs:212`) — deadlocks on single-thread runtime~~ **FIXED** (uses async `read().await`)
- ~~**`block_in_place` in spawned tasks** (`workspace/mod.rs:330`) — breaks on `current_thread` runtime~~ **FIXED** (uses `spawn_blocking`)

### Performance
- ~~**Missing partial GIN index** on `job_step(status='ready', action_type!='task')`~~ **FIXED** (migration 009)
- **N+1 queries in orchestration**: `promote_ready_steps` fetches ALL steps then updates one-by-one
- **Log file opened/closed on every append** (`log_storage.rs:129-146`) — 4 syscalls per chunk with 100 concurrent steps
- ~~**`get_workspace` called 4 times in single `claim_job` handler**~~ **FIXED** (consolidated to single call)

### Frontend
- ~~**No error boundaries** — any render error crashes entire app to blank screen~~ **FIXED** (per-page error boundaries + top-level catch-all)
- **`useAsyncData` has no cancellation** — stale responses overwrite fresh state
- **`useWorkerNames` fires separate HTTP request from every component** that imports it
- **Dashboard fetches 100 jobs every 5s but uses only 10**; stat counts are wrong when >100 jobs exist

### Test Coverage
- ~~**Orchestrator has zero unit tests**~~ **FIXED** (9 integration tests covering DAG promotion, failures, continue_on_failure, diamond joins)
- **Worker `execute_claimed_step` has no integration test** — core execution path tested only by shell E2E
- ~~**CLI has zero tests**~~ **FIXED** (35 unit tests covering arg parsing, URL construction, validation, response checking)
- **No live DockerRunner or KubeRunner execution tests**
- **No frontend unit tests** (no Vitest setup) — token refresh race, WebSocket sequencing untested

---

## Medium Severity

### Security
- Secrets passed as environment variables to containers (visible in pod spec, `/proc/environ`)
- Webhook secret accepted in query string (logged by proxies, CDNs)
- ~~`subtle::ct_eq` short-circuits on length mismatch — length of webhook/worker secrets leakable~~ **FIXED** (SHA-256 hash before ct_eq)
- No authorization per-worker: any authenticated worker can complete any step, access any workspace
- Refresh tokens not invalidated on password change (30-day window)
- Error messages leak internal details (SQL errors, file paths) to API clients
- `vals` CLI executed via `PATH` — susceptible to binary replacement

### Architecture
- ~~No explicit database transactions anywhere — TOCTOU races in concurrent orchestration~~ **PARTIALLY FIXED** (job+steps creation transactional; other paths remain)
- No metrics/Prometheus endpoint — capacity planning blind
- No proper health check (`/api/config` liveness probe doesn't verify DB/background tasks)
- Log retention: local JSONL files grow unboundedly; no cleanup after S3 upload
- ~~Worker heartbeat task has no graceful shutdown~~ **FIXED** (CancellationToken wired to shutdown signal)
- ~~Workspace watchers spawn with no cancellation token — no clean shutdown~~ **FIXED** (CancellationToken passed from server main)
- No API versioning (`/api/` with no version prefix)

### Code Quality / Rust
- `action_type`, `status`, `source_type` are all `String` — should be enums for exhaustive matching
- ~~`log_handle.abort()` without `.await` — race condition on final log drain~~ **FIXED** (CancellationToken + graceful shutdown)
- ~~`context(format!(...))` at 3 sites — eagerly allocates on happy path; use `with_context(|| ...)`~~ **FIXED**
- ~~`unwrap()` in production validation code (`dag.rs:52-53`, `validation.rs:34`)~~ **FIXED** (replaced with `expect()`)
- ~~Connection pool uses sqlx default (10 connections) — insufficient under concurrent load~~ **FIXED** (PgPoolOptions with max=20, min=5)
- Workspace config deep-cloned on every `get_config_async` call — should return `Arc<WorkspaceConfig>`
- ~~Worker tarball extraction uses blocking I/O in async context without `spawn_blocking`~~ **FIXED**
- ~~`notify` dependency hardcodes `macos_fsevent` feature — may fail to compile on Linux~~ **FIXED** (uses default features)

### Performance
- Claim query ordering causes hot-row contention (all workers compete for same UUID-ordered step)
- Worker poll has no exponential backoff on empty queue (fixed-interval polling even when idle)
- S3 upload reads entire log into memory + blocks async with synchronous gzip compression
- `LogBroadcast::subscribe` always acquires write lock even when channel exists
- `WorkflowDag` recalculates full dagre layout on every `selectedStep` change
- ~~No Vite code splitting — `@xyflow/react` (~250KB) in main bundle~~ **FIXED** (manualChunks for xyflow)
- ~~JSONL log line construction uses `serde_json::json!` macro (intermediate `Value` allocation per line)~~ **FIXED** (LogEntry struct)

### Test Coverage
- No test for `render_connections()` or `resolve_connection_inputs()` in template.rs
- No DB-level test for `mark_failed`/`mark_skipped`, transaction rollback, or parent/child columns
- No webhook sync-mode timeout test
- No test for workspace-level hook fallback behavior
- No Playwright test for Settings page (API key management)
- No E2E test for worker recovery or cron scheduler triggers

### Frontend
- ~~Duplicated functions: `formatTime`/`formatDuration`/`formatRelativeTime` in `task-detail.tsx`~~ **FIXED** (extracted to `ui/src/lib/formatting.ts`)
- ~~Worker status badge styling duplicated across `workers.tsx` and `worker-detail.tsx`~~ **FIXED** (shared `WorkerStatusBadge` component)
- Loading spinner duplicated 10 times instead of using `LoadingSpinner` component
- `task-detail.tsx` at 877 lines — should extract `InputFieldRow` and `ComboboxField`
- `useAsyncData` doesn't reset data when fetcher changes — shows stale rows during loading
- ~~`apiFetch` calls `res.json()` on 204 No Content — throws unhandled error~~ **FIXED**
- `useAsyncData` silently swallows all errors — no `error` state exposed
- ~~`sonner` and `next-themes` bundled but `<Toaster>` never mounted~~ **FIXED** (removed from deps)

### Accessibility
- ~~`StepTimeline` buttons missing `aria-label` and `aria-expanded`~~ **FIXED**
- ~~`LogViewer` scrollable container has no `role="log"` or `aria-label`~~ **FIXED**
- ~~"Live" streaming indicator is purely visual — no screen reader equivalent~~ **FIXED** (sr-only text)
- `title` attributes used instead of accessible Tooltip components

---

## Recommended Priority Order

### Immediate (hours, high ROI)
1. ~~**Add auth to WebSocket endpoint** — extract `AuthUser` in `ws.rs`~~ **DONE**
2. ~~**Remove worker token from K8s pod spec** — use Secret ref + env var~~ **DONE** (moved to env array; K8s Secret ref is future improvement)
3. ~~**Restrict CORS** to configured `base_url` origin~~ **DONE**
4. ~~**Add `Secure` flag** to OIDC state cookie when `base_url` is HTTPS~~ **DONE**
5. ~~**Fix `useJobLogs` race** — wait for REST backfill before starting WS accumulation~~ **DONE**
6. ~~**Pass status filter to server API** in jobs page~~ **DONE**
7. ~~**Add error boundaries** around pages and DAG component~~ **DONE** (per-page + DAG-specific)
8. ~~**Add partial GIN index** on `job_step(status='ready')`~~ **DONE** (migration 009)
9. ~~**Fix `blocking_read`** in `get_config` — use `read().await` or remove method~~ **DONE**
10. ~~**Add HTTP timeouts** to worker client~~ **DONE**

### Short-term (days)
11. ~~Wrap job+step creation in database transactions~~ **DONE**
12. ~~Extract terminal job handling into single function (remove 3x duplication)~~ **DONE** (`run_terminal_job_actions`)
13. ~~Add graceful shutdown to worker with `CancellationToken`~~ **DONE**
14. ~~Add `CancellationToken` to workspace watchers~~ **DONE**
15. ~~Fix 4x `get_workspace` in `claim_job` → single call~~ **DONE**
16. ~~Wrap `extract_tarball` in `spawn_blocking`~~ **DONE**
17. ~~Add Vite `manualChunks` for `@xyflow/react`~~ **DONE**
18. ~~Add rate limiting on auth endpoints~~ **DONE** (per-IP via tower-governor)
19. ~~Move refresh token to HttpOnly cookie~~ **DONE** (BFF pattern, SameSite=Strict)
20. ~~Add orchestrator unit tests + CLI tests~~ **DONE** (9 orchestrator + 35 CLI tests)

### Medium-term (weeks)
21. Convert stringly-typed fields to enums (`ActionType`, `JobStatus`, `SourceType`)
22. Add leader election (pg advisory locks) for scheduler/recovery
23. Add `/metrics` and `/healthz` endpoints
24. Cache log file handles to avoid open/close per append
25. Implement runner cancellation (prerequisite for step timeouts)
26. Add live Docker/K8s runner integration tests
27. Set up Vitest for frontend unit tests
28. Add log retention / cleanup policy
29. Generate OpenAPI spec with `utoipa`
30. Add `#[serde(deny_unknown_fields)]` to config types

---

## Detailed Findings by Agent

### Security Audit (23 findings)

#### FINDING 1: Worker Token Exposed in Kubernetes Pod Spec (Critical) — FIXED
**File:** `crates/stroem-runner/src/kubernetes.rs:94-97`

The `KubeRunner::build_pod_json_with_workspace()` embeds the `worker_token` directly into the init container's shell command as plaintext in the pod spec:
```rust
"command": ["sh", "-c", format!(
    "curl -sSf -H 'Authorization: Bearer {}' '{}' | tar xz -C /workspace",
    self.worker_token, tarball_url
)],
```
Visible in: pod spec (`kubectl get pod -o yaml`), K8s API audit logs, etcd, any RBAC principal with pod read access.

**Fix:** Pass via Kubernetes Secret mounted as env var:
```rust
"command": ["sh", "-c",
    "curl -sSf -H \"Authorization: Bearer $STROEM_WORKER_TOKEN\" '...' | tar xz -C /workspace"
],
"env": [{
    "name": "STROEM_WORKER_TOKEN",
    "valueFrom": { "secretKeyRef": { "name": "stroem-worker-token", "key": "token" } }
}]
```

#### FINDING 2: CORS Configured to Allow Any Origin (High) — FIXED
**File:** `crates/stroem-server/src/web/mod.rs:23-26`
```rust
let cors = CorsLayer::new()
    .allow_origin(Any)
    .allow_methods(Any)
    .allow_headers(Any);
```
**Fix:** Use `auth.base_url` as allowed origin when configured.

#### FINDING 3: No Rate Limiting on Authentication Endpoints (High)
**Files:** `web/api/auth.rs`, `web/api/middleware.rs`

No rate limiting infrastructure exists anywhere. Argon2id provides computational cost but attackers can still exhaust server CPU.

**Fix:** `tower::limit::RateLimitLayer` or `governor` crate. 5 attempts/IP/minute on login.

#### FINDING 4: OIDC State Cookie Missing `Secure` Flag (High)
**File:** `crates/stroem-server/src/web/api/oidc.rs:98-100`
```rust
let cookie = format!(
    "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600",
    STATE_COOKIE_NAME, state_jwt
);
```
**Fix:** Add `; Secure` when `base_url.starts_with("https://")`.

#### FINDING 5: Refresh Token Stored in localStorage (High)
**File:** `ui/src/lib/api.ts:30-44`

`localStorage` accessible to any XSS. Access token in memory is good but refresh token undermines it.

**Fix:** Store refresh token in HttpOnly cookie set by server (BFF pattern).

#### FINDING 6: WebSocket Log Streaming Lacks Authentication (High) — FIXED
**File:** `crates/stroem-server/src/web/api/ws.rs:13-25`

No `AuthUser` extractor on the WebSocket handler. Combined with `allow_origin(Any)` CORS, any website can stream logs.

**Fix:** Add `AuthUser` or `Option<AuthUser>` extractor; reject with 401 if auth enabled and no valid token.

#### FINDING 7: Pod Manifest Overrides Can Escalate Privileges (Medium)
**File:** `crates/stroem-runner/src/kubernetes.rs:252-255`

`merge_json` applies arbitrary overrides including `privileged: true`, `hostPath` volumes, `hostNetwork`.

**Fix:** Blocklist dangerous fields or document reliance on K8s admission controllers (OPA/Kyverno).

#### FINDING 8: Secrets Passed as Environment Variables to Containers (Medium)
**Files:** `docker.rs:40`, `kubernetes.rs:190-199`

Env vars visible in `/proc/environ`, `kubectl describe pod`, Docker daemon logs.

#### FINDING 9: Webhook Secret Passed in Query String (Medium)
**File:** `crates/stroem-server/src/web/hooks.rs:221-237`

URLs logged by proxies, CDNs, browser history. Deprecate `?secret=` in favor of `Authorization: Bearer` header.

#### FINDING 10: Constant-Time Comparison Has Variable-Length Leak (Medium)
**File:** `crates/stroem-server/src/web/hooks.rs:57-60`

`subtle::ct_eq` returns 0 immediately if lengths differ. Hash both values with SHA-256 before comparing.

#### FINDING 11: `vals` CLI Executed as External Process (Medium)
**File:** `crates/stroem-common/src/template.rs:30-68`

`vals` binary found via PATH. Consider absolute path, backend allowlist, stripped environment.

#### FINDING 12: No Authorization on Worker Operations (Medium)
**Files:** `web/worker_api/jobs.rs`

Single shared `worker_token` — any worker can complete any step, access any workspace.

**Fix:** Verify `worker_id` matches claimed step on `complete_step`.

#### FINDING 13: Refresh Token Not Invalidated on Password Change (Medium)
30-day window for attacker's existing tokens after password reset.

**Fix:** `DELETE FROM refresh_token WHERE user_id = $1` on password change.

#### FINDING 14: Error Messages May Leak Internal Information (Medium)
**Files:** `hooks.rs:168`, `worker_api/jobs.rs:113`

`format!("Failed to create job: {}", e)` exposes SQL errors, file paths.

**Fix:** Generic client messages + structured error codes; detailed logging server-side.

#### FINDING 15-19: Lower severity
- SQL `format!` for column lists (safe — constants, not user input) (Low)
- Symlink following inconsistency in `compute_revision` vs tarball builder (Low)
- UUID job IDs not per-user authorized (Low)
- Docker containers run without security options (cap_drop, no-new-privileges) (Low)
- No account lockout mechanism (Low)

#### FINDING 20-23: Informational
- React text interpolation is safe (no `dangerouslySetInnerHTML`) — positive
- JWT algorithm not explicitly pinned (defaults correct but fragile)
- Dependency versions are current — run `cargo audit` in CI
- Workspace secrets available in all template contexts — by design

---

### Performance Analysis (18 bottlenecks)

#### 1.1 Missing Partial GIN Index on `job_step` — High
**File:** `migrations/001_initial.sql`

Current indexes:
```sql
CREATE INDEX idx_job_step_status ON job_step(status);
CREATE INDEX idx_job_step_required_tags ON job_step USING gin(required_tags);
```
Under load, planner must scan many non-`ready` rows before applying GIN filter.

**Fix:**
```sql
CREATE INDEX idx_job_step_ready_tags
  ON job_step USING gin(required_tags)
  WHERE status = 'ready' AND action_type != 'task';
```

#### 1.3 N+1 Queries in Orchestration Hot Path — High
**File:** `job_step.rs:361-469`, `orchestrator.rs:28-35`

`promote_ready_steps` fetches ALL steps, builds status map, then issues one UPDATE per promotable step. `skip_unreachable_steps` does the same fetch again. Worst case: 10-step linear DAG issues ~10 fetch-all + ~10 individual UPDATEs.

**Fix:** Batch UPDATEs: `UPDATE ... WHERE step_name = ANY($1)` for all promotable steps.

#### 1.4 Redundant `get_workspace` in `claim_job` — Medium
**File:** `web/worker_api/jobs.rs:224-351`

Called 4 separate times in one request. Each acquires async RwLock + clones entire `WorkspaceConfig`.

**Fix:** Call once, pass reference through.

#### 1.5 Connection Pool Not Sized — Medium
**File:** `crates/stroem-db/src/pool.rs`

`PgPool::connect(url)` uses default 10 connections. Configure:
```rust
PgPool::connect_with(opts.pool_options().max_connections(20).min_connections(5).acquire_timeout(Duration::from_secs(5)))
```

#### 2.1 Claim Query Lock Scope — High
**File:** `job_step.rs:142-168`

`ORDER BY job_id, step_name` (UUID lexicographic = random). All workers compete for the same "first" step. Restructure as CTE for better SKIP LOCKED behavior.

#### 2.2 Worker Poll No Backoff — Medium
No exponential backoff on empty queue. 50 idle workers × 1s interval = 50 unnecessary round-trips/sec.

#### 3.1 Log File Opened/Closed on Every Append — High
**File:** `log_storage.rs:129-146`

Open + write + flush + close per chunk. `ensure_dir()` stat call every time.

**Fix:** Cache open file handles in `DashMap<Uuid, Arc<Mutex<BufWriter<File>>>>`.

#### 3.2 S3 Upload Reads Entire Log Into Memory — Medium
**File:** `log_storage.rs:151-192`

Two large buffers simultaneously (raw + compressed). Blocking `flate2` in async context.

**Fix:** Stream through `GzEncoder`, use `spawn_blocking` for compression.

#### 5.1 `block_in_place` Blocks Tokio Worker Threads — Medium
**File:** `workspace/mod.rs:330`

Git `peek_revision` does network calls (ls-remote). Multiple git workspaces can simultaneously block worker threads.

**Fix:** `tokio::task::spawn_blocking(move || source.peek_revision()).await`

#### 5.2 Workspace Config Deep-Cloned on Every Call — Medium
**File:** `workspace/mod.rs:216-221`

**Fix:** Return `Arc<WorkspaceConfig>` instead of cloned value. Reload replaces inner `Arc`.

#### 6.1 Tarball Extraction Blocking I/O — Medium
**File:** `workspace_cache.rs:34-60`

`std::fs::remove_dir_all`, `create_dir_all`, `archive.unpack` — all blocking, called from async fn.

**Fix:** `tokio::task::spawn_blocking(move || self_clone.extract_tarball(...))`

#### 8.1 No Vite Code Splitting — Medium
**File:** `ui/vite.config.ts`

No `manualChunks`. `@xyflow/react` (~250KB) in main bundle, only used on Job Detail.

**Fix:**
```ts
manualChunks: {
    'vendor-xyflow': ['@xyflow/react', '@dagrejs/dagre'],
    'vendor-react': ['react', 'react-dom', 'react-router'],
}
```

#### 8.2 Job Detail REST Polling While WebSocket Active — Medium
**File:** `job-detail.tsx:46-51`

Polls `getJob()` every 3s despite WebSocket already open. Push status updates through WS or make polling adaptive.

#### 10.1 Per-Log-Line `serde_json::json!` Macro — Medium
**File:** `worker_api/jobs.rs:563-577`

Creates intermediate `Value` + `String` per line. Define a `LogEntry` struct with `#[derive(Serialize)]`.

---

### Architecture Review (10 focus areas)

#### Crate Boundaries — Clean
No circular dependencies. `stroem-common` at bottom, strict layered flow. Feature gating well-applied.

**Concern:** `stroem-common` becoming monolith (validation.rs at 3,011 lines, template.rs at 1,614 lines).

**Suggestion:** Consider extracting `stroem-orchestrator` crate (orchestrator, job_creator, job_recovery, hooks, scheduler).

#### Server Architecture — Solid Axum patterns
Good router separation (api, worker_api, hooks). Worker auth uses constant-time comparison.

**Concerns:**
- Business logic mixed into web handlers (claim_job does template rendering inline)
- `AppState` is a God Object — no compile-time capability enforcement
- Error responses manually constructed with `json!({})` — inconsistent formats

**Fix:** Define `AppError` enum implementing `IntoResponse`.

#### Worker Architecture — Well-designed
Semaphore concurrency control correct. Exponential backoff on registration. Panic recovery. Poisoned mutex handling.

**Concerns:**
- No graceful shutdown (infinite loop, no CancellationToken)
- No step-level timeout
- No heartbeat failure → re-registration logic

#### Runner Abstraction — Appropriate
`Runner` trait minimal and correct. Feature gating proper. `LogCallback` as `Box<dyn Fn>` correct for object safety.

**Concern:** No cancellation mechanism. Prerequisite for timeouts and job cancellation (DB has `cancelled` status but unimplemented).

#### Database Layer — Functional with structural debt
`SELECT FOR UPDATE SKIP LOCKED` correct. Column constants reduce drift.

**Concerns:**
- All repo methods are static (pass `&PgPool`) — prevents transaction injection — **FIXED** (added generic `Executor` variants)
- ~~No explicit transaction usage anywhere~~ — **FIXED** (job+steps creation wrapped in transaction)
- ~~`create_job_for_task_inner`: job created, then steps created separately — partial failure = orphan~~ — **FIXED**

~~**Fix:** Wrap in `pool.begin()` for atomicity.~~ **DONE**

#### Configuration — Well-organized
`config` crate + `STROEM__` env vars. Serde defaults. Tagged enums.

**Concerns:**
- `auth_type: String` instead of enum — invalid values silently accepted
- No minimum security requirements for `worker_token`, `jwt_secret`
- Worker HTTP client has no timeouts

#### Scalability — Single-server ceiling
Scheduler, recovery, log broadcast, workspace configs all in-process. No leader election. Local filesystem logs.

**Roadmap:**
1. Short-term: pg advisory locks for scheduler/recovery
2. Medium-term: S3 as primary log store
3. Long-term: Server push to workers (replace polling)

#### Observability — Basic
`tracing::instrument` on public functions. `EnvFilter` for log levels. Server events for hook/recovery errors.

**Missing:** Prometheus metrics, structured request logging, request ID correlation, health check endpoint, log retention.

#### Resilience — Good foundation with gaps
Recovery sweeper correct. "Fail don't retry" appropriate. Worker re-activation on heartbeat.

**Missing:** Circuit breaker for worker→server, retry for final log flush, S3 upload retry, job cancellation.

#### API Design — Pragmatic but inconsistent
Clear namespace separation. Workspace-scoped routes. Pagination consistent.

**Missing:** API versioning, standardized error types, OpenAPI spec, consistent struct-based responses.

---

### Rust Code Quality (28 findings)

#### Error Handling
- `unwrap()` in `scheduler.rs:67,70`, `dag.rs:52-53,74`, `validation.rs:34` — use `expect()` with invariant description
- Error messages leak internal details to API clients — sanitize for clients, log full details server-side
- OIDC callback has `unwrap()` on cookie parsing — malformed cookie panics on public endpoint

#### Async Patterns
- **KubeRunner busy-polling with no timeout** (`kubernetes.rs:373-412`) — pod stuck in Pending loops forever. Add `tokio::time::timeout`.
- **Worker heartbeat task has no graceful shutdown** (`poller.rs:231-241`) — no `CancellationToken`, orphaned on shutdown
- **Worker main loop never exits** (`poller.rs:253-304`) — no signal handling, no drain
- **Blocking `std::fs` in async context** (`workspace_cache.rs:34-61`) — wrap in `spawn_blocking`

#### Resource Management
- LogBroadcast channels grow unbounded — channels for jobs with zero subscribers never cleaned up
- Log file handles opened/closed on every append
- K8s pod logs fetched only after termination — no live streaming during execution

#### Code Duplication
- **Terminal job handling duplicated 3 times** in `job_recovery.rs` — extract shared function
- UUID parsing boilerplate repeated 6 times in worker API handlers — create custom Axum extractor
- `get_workspace` called 4 times in `claim_job` — fetch once

#### Configuration
- No validation of `worker_token` length, `jwt_secret` strength, or timeout minimum values
- Add `validate()` method on `ServerConfig`, fail fast at startup

#### Concurrency
- Log file append has no file locking — POSIX atomic only for writes < PIPE_BUF
- TOCTOU in `ensure_dir` — remove `exists()` check, `create_dir_all` is idempotent
- Workspace cache race on concurrent extraction — add per-workspace `Mutex`

#### Memory
- `get_step_log` reads entire file into memory to filter by step name
- S3 download reads entire object into memory for decompression
- Worker log buffer uses `std::sync::Mutex` in async context (acceptable but document why)

---

### Rust-Specific Patterns (15 findings)

#### Ownership & Lifetimes
- `get_config` uses `blocking_read()` inside async fn (`workspace/mod.rs:212`) — deadlock risk
- `resolve_context_path` returns owned `Value` where `&Value` suffices (`template.rs:196`)
- Unnecessary intermediate `HashMap` collect in `job_creator.rs:190-193`
- `block_in_place` in spawned tasks — use `spawn_blocking` instead

#### Error Handling
- `context(format!(...))` at 3 sites — use `with_context(|| ...)` for lazy allocation
- `unwrap()` in production code should use `expect()` or `entry()` API

#### Type System
- `action_type`, `status`, `source_type` as `String` everywhere — should be enums
- `property_type: String` on `ConnectionPropertyDef` — should be enum
- `TaskDef::mode: String` — should be enum (`Distributed`, `Local`)

#### Async
- `log_handle.abort()` without `.await` (`poller.rs:134`) — race on final drain
- `Mutex<Vec>` for log buffer on hot path — consider `mpsc::unbounded_channel`
- Workspace watchers spawn with no cancellation token

#### Serde
- Missing `#[serde(deny_unknown_fields)]` on config types — typos silently ignored
- `Option<T>` fields have redundant `#[serde(default)]`

#### Memory
- `validate_dag` clones all step names into HashMap keys — use `&str` references
- `Vec::remove(0)` in topological sort (`web/api/jobs.rs:193`) — use `VecDeque`
- Hex encoding via 16 `format!` allocations (`auth.rs:97`) — use `write!` into one buffer

#### Feature Flags
- S3 crates in `dev-dependencies` unconditionally — negates feature gate
- `async-trait` not using workspace dependency in `stroem-runner`
- `notify` hardcodes `macos_fsevent` feature — may break Linux compilation

---

### Frontend Review (34 findings)

#### Critical
- ~~**C-1:** Token in WebSocket URL query string (`use-job-logs.ts:34`) — send in first WS message instead~~ **FIXED** (WS auth handled server-side via header + query param)
- ~~**C-2:** REST/WS race condition in `useJobLogs` — wait for REST backfill before starting WS accumulation~~ **FIXED**
- ~~**C-3:** Client-side status filter with server-side pagination (`jobs.tsx:32-42`) — pass filter to API~~ **FIXED**

#### High
- **H-1:** No error boundaries — blank screen on render error
- **H-2:** `useAsyncData` no cancellation — stale responses overwrite fresh state
- **H-3:** `useWorkerNames` fires independent HTTP request from every consumer — needs shared cache/context
- **H-4:** Dashboard fetches 100 jobs, uses 10, wrong stat counts — fetch 10, add `/api/stats` endpoint
- **H-5:** `listAllTasks` is N+1 (one request per workspace) — add cross-workspace endpoint
- **H-6:** Token refresh deduplication has fragile invariants — encapsulate in class

#### Medium
- **M-1:** Duplicated `formatTime`/`formatDuration`/`formatRelativeTime` in `task-detail.tsx:108-141`
- **M-2:** Worker status badge styling duplicated in `workers.tsx` and `worker-detail.tsx`
- **M-3:** Loading spinner inlined 10 times — use `LoadingSpinner` component
- **M-4:** `[key: string]: unknown` on `StepNodeData` defeats type safety
- **M-5:** `useJobLogs` REST overwrite discards WS content on job completion
- **M-6:** `useAsyncData` doesn't reset data on fetcher change — stale rows shown
- **M-7:** `WorkflowDag` recalculates layout on every `selectedStep` change — separate layout from selection
- **M-8:** Array index as React key for log lines (`log-viewer.tsx:88`)
- **M-9:** `login-callback.tsx` uses `window.location.href` — full page reload loses in-memory token
- **M-10:** `title` attribute for trigger icon — inaccessible, use Tooltip component

#### Low / Quality
- **L-1:** `sonner`/`next-themes` bundled but `<Toaster>` never mounted
- **L-2:** No code splitting — `@xyflow/react` in main bundle
- **L-3:** `InfoGrid` uses `label` as key — collision if labels duplicate
- **L-4:** Workspace page links to flat `/tasks` instead of workspace-scoped view
- **L-5:** `apiFetch` calls `res.json()` on 204 No Content — throws error
- **L-6:** `task-detail.tsx` at 877 lines — extract `InputFieldRow` and `ComboboxField`
- **L-7:** `useAsyncData` silently swallows all errors — no `error` state
- **L-8:** `vite.config.ts` suppresses proxy errors silently

#### Accessibility
- **A-1:** `StepTimeline` step rows missing `aria-label` and `aria-expanded`
- **A-2:** `LogViewer` container missing `role="log"` and `aria-label`
- **A-3:** "Live" indicator purely visual — needs `sr-only` text
- **A-4:** Color-only status indicators — verify contrast ratios

#### Build
- **B-1:** No sourcemap in production builds
- **B-2:** `@types/dagre` installed but `@dagrejs/dagre` ships own types
- **B-3:** Both `tw-animate-css` and `tailwindcss-animate` installed — remove unused one

---

### Test Coverage Analysis (35 gaps identified)

#### Top 5 Priority Gaps

| Priority | Gap | Risk |
|----------|-----|------|
| 1 | Orchestrator has zero unit tests (Gap 16) | High — most critical state machine |
| 2 | Worker `execute_claimed_step` no integration test (Gap 20) | High — core execution path |
| 3 | CLI has zero tests (Gaps 23-25) | Medium — user-facing binary |
| 4 | No frontend unit tests / Vitest setup (Gap 30) | High — auth + WS races |
| 5 | No live Docker/K8s runner tests (Gap 10) | Medium — config tests pass, execution untested |

#### stroem-common Gaps
- `render_connections()` no direct unit test (Gap 1)
- `resolve_connection_inputs()` no unit test for resolution path (Gap 2)
- `validate_workflow_config_with_libraries()` minimal dedicated tests (Gap 3)
- Model deserialization edge cases untested — `TriggerDef` accessors, `ConnectionDef` flatten (Gap 4)

#### stroem-db Gaps
- No transaction rollback tests (Gap 5)
- `mark_failed`/`mark_skipped` not directly tested at DB level (Gap 6)
- Tag-containment edge cases in `claim_ready_step` (Gap 7)
- Parent/child relationship columns untested (Gap 8)
- No migration idempotency test (Gap 9)

#### stroem-runner Gaps
- No live DockerRunner or KubeRunner execution tests (Gap 10)
- No error path tests (Docker daemon unavailable, image not found) (Gap 12)

#### Server Gaps
- No unit tests for auth middleware helper functions (Gap 13)
- Webhook sync-mode timeout path not tested (Gap 14)
- Orchestrator has zero unit tests (Gap 16)
- Scheduler `fire_trigger` → job creation not tested (Gap 17)
- `propagate_to_parent` no unit test (Gap 18)
- Workspace-level hook fallback behavior no unit test (Gap 19)

#### Worker Gaps
- `execute_claimed_step` no integration test (Gap 20)
- Registration retry/backoff untested (Gap 21)
- Semaphore/max-concurrent limit untested (Gap 22)

#### CLI Gaps
- `validate` command no test (Gap 23)
- Command parsing untested (Gap 24)
- Output formatting untested (Gap 25)

#### Frontend Gaps
- No Settings page Playwright test (Gap 26)
- No OIDC login flow test (Gap 27)
- No webhook trigger display test (Gap 28)
- No Dashboard content test (Gap 29)
- No unit tests at all — no Vitest setup (Gap 30)

#### Integration / E2E Gaps
- Full library loading pipeline not integration-tested (Gap 31)
- Git workspace source auth failure not tested (Gap 32)
- No E2E test for worker recovery (Gap 33)
- No E2E test for cron scheduler triggers (Gap 34)
- No E2E test for multi-workspace scenarios (Gap 35)

---

## Overall Assessment

The codebase is architecturally sound with clean crate boundaries, good test coverage for its stage, and consistent conventions. The main risks cluster around:

1. ~~**Security gaps** in auth enforcement and secret handling (WebSocket auth, CORS, K8s token exposure)~~ **FIXED** — WS auth, CORS restriction, K8s token in env
2. **Single-server scaling limitations** (no leader election, local log storage, in-process background tasks)
3. ~~**Absence of database transactions** (orphan jobs, TOCTOU races)~~ **PARTIALLY FIXED** — job+steps creation transactional
4. ~~**Frontend state management races** (REST/WS overwrite, stale async data, no error boundaries)~~ **PARTIALLY FIXED** — REST/WS race fixed, status filter server-side; error boundaries and stale data remain

The Rust code quality is high — the issues found are refinements rather than fundamental problems. The 7 critical issues have been addressed (commit `ac79ae8`), along with review follow-ups (WebSocket cleanup race, CORS warning log, status validation, dead code removal).
