# API and auth

API errors, authentication, ACL and task duration stats. Moved word for word from `CLAUDE.md`, which indexes this file. A `§ <Title>` reference names a `###` section in any file of `docs/internal/architecture/`.

### Error Handling (AppError)
- `AppError` enum in `web/error.rs`: `BadRequest`, `Unauthorized`, `Forbidden`, `NotFound`, `Conflict`, `Internal`
- `Internal` logs full error, returns generic message. `From<anyhow::Error>` and `From<sqlx::Error>` impls.

### Authentication
- **User auth**: Optional JWT (access 15min, refresh 30d with rotation). Enabled via `auth` section in config.
- **Worker auth**: Bearer token from config (`worker_token`)
- **API keys**: `strm_` prefix + 32 hex chars. SHA256 stored in DB. Optional expiry. Frontend: Settings page.
- **Rate limits**: per-IP `tower_governor` limits on the auth routes (login, refresh, logout/me/OIDC, API keys) in `build_api_routes`. `auth.rate_limit.enabled: false` (env `STROEM__AUTH__RATE_LIMIT__ENABLED=false`) removes them all and logs a startup warning — tests only; the Playwright overlay `docker-compose.test.yml` sets it via env.
- **OIDC SSO**: Authorization Code + PKCE via `openidconnect`. JIT user provisioning (auth_link → email → create). State in signed HttpOnly cookie.

### ACL (Access Control)
- Config-driven: optional `acl` section. No config = everything open.
- Admin flag bypasses all checks. Groups managed by admins via API.
- Rule evaluation: all matching rules checked, **highest permission wins** (Run > View > Deny).
- Glob matching with `*` wildcard. Task path: `"{folder}/{task_name}"` or `"{task_name}"`.
- How a JOB (not a task) is authorised — pinned jobs, job lists, MCP: the "Job-scoped read paths authorise with the job row" rule in § Git refs (pinned references).

### Task Duration Stats
- `GET /api/workspaces/{ws}/tasks/{name}/stats?limit=50` — p50/p95/avg/min/max + recent durations + per-step breakdown over last N **completed** runs (View permission sufficient). Failed/cancelled runs excluded; `for_each` instance rows excluded from per-step breakdown.
- Queries: `JobRepo::get_task_duration_stats` / `get_recent_durations` and `JobStepRepo::get_step_duration_stats_for_task` use Postgres `percentile_cont(...)` directly.
- UI: `<DurationInsightsCard>` on Task Detail page; ETA / overrun pill on Job Detail page driven by `lib/eta.ts::computeEta`; per-step `p50: Xs` badges in `<StepTimeline>`.
- Pinned jobs are excluded (`git_ref IS NULL` in all three queries): stats describe the live task.
