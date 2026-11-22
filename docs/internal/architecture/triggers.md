# Triggers

Cron (scheduler), webhook and event-source triggers, and trigger run status. `AGENTS.md` indexes this file. A `§ <Title>` reference names a `###` section in any file of `docs/internal/architecture/`.

### Scheduler (Cron Triggers)
- `scheduler.rs` — background task, smart sleep (wakes at next fire time), config hot-reload, `CancellationToken`
- `job_creator.rs` — shared job+step creation for API handler and scheduler
- `croner` crate for cron parsing (5/6-field with `with_seconds_optional()`)
- **Timezone**: Optional `timezone` field (IANA name), defaults to UTC. Uses `chrono-tz`.
- **Concurrency policy**: `Allow` (default) / `Skip` / `CancelPrevious` on scheduler triggers
- **Trigger run status** (spec `docs/superpowers/specs/2026-10-20-trigger-run-status-design.md`; terms Last fire and Missed run in `CONTEXT.md`): each trigger shows its last run, result and, for enabled scheduler triggers, missed runs. Module `stroem-server/src/trigger_status.rs` (`missed_runs`, `MISSED_GRACE_SECS` = 300, `MISSED_SKEW_TOLERANCE_MS` = 1000 (the first occurrence strictly less than 1 s after the last fire is absorbed, never more than one), `MISSED_CAP` = 1000 shown as `1000+`, `attach_run_status`, `viewer_for`, `trigger_visible`, `needs_attention`, `sort_attention`). `TriggerRunRepo::last_runs` is ONE query per request over two partial indexes (migration 053); a run is `source_type` `trigger`/`webhook` (`source_id` = `{defining ws}/{trigger key}`), or an event-source CONSUMER job, told apart from emitted jobs by `input ? '_fingerprint'`, never by task name. The last fire (any status, `skipped` included) is the base of the missed mark; the shown last run ignores `skipped` rows unless nothing else exists; the result is the newest job of the task-retry chain. Only the trigger's own next run clears the mark (a manual run, Re-run or Restart does not). `TriggerInfo` gains `last_run`, `last_run_hidden` and `missed`; a run the user may not view (job-row ACL via `acl::job_coords_task_path`, so pinned and cross-workspace runs are checked on their own task) is hidden, the missed mark is not. `list_triggers` now fails closed when the ACL context cannot be loaded. `GET /api/triggers/attention` (`list_attention_triggers`, `AttentionTrigger` adds `workspace` and `last_run_failed`) serves the Dashboard and skips workspaces in load error.

### Webhook Triggers
- `TriggerDef` tagged enum: `Scheduler`, `Webhook`, and `EventSource` variants
- Handler at `/hooks/{name}` (not under `/api/`). Auth: optional `secret` field (query param or Bearer header). With `force_refresh`: auth against the cached definition, reload, re-match + re-auth against the fresh one (§ Git refs). The target `task` may be `ws.task` and carry `ref:`; resolution errors go through `classify_execute_error` (400/500). Sync output and every branch of the status poll are redacted with the per-job set (503 + `job_id` when it is transiently incomplete).
- Input mapping: `body`, `headers`, `method`, `query` + YAML `input` defaults
- **Sync/async mode**: `mode: "sync"` waits for completion (default: async). `timeout_secs` max wait (default 30, max 300).

### Event Source Triggers
- `TriggerDef::EventSource` variant: long-running queue consumer processes that emit jobs via stdout `OUTPUT: ` protocol.
- **Consumer task**: `task:` field references a regular task whose flow runs the long-lived consumer process.
- **Target task**: `target_task:` field specifies which task to create jobs for (receives emitted JSON as input).
- **Stdout protocol**: Lines starting with `OUTPUT: ` followed by valid JSON become job input for target task, merged with trigger `input` defaults. All other stdout/stderr lines are captured in consumer task's log view.
- **Environment & input**: `env:` provides Tera-templated environment overrides for consumer execution. `input:` provides defaults merged into each emitted job.
- **RestartPolicy enum**: `Always` (default), `OnFailure`, `Never` — controls behavior when consumer process exits.
- **Exponential backoff**: `backoff_secs` field (default 5) — doubled on consecutive failures, capped at 5 minutes. Resets on clean exit.
- **Backpressure**: `max_in_flight` field limits concurrent pending/running jobs for target task. Server tracks count; stdout reading pauses when limit reached, creating natural Unix pipe backoff.
- **EventSourceManager**: Server-side background task creating/monitoring consumer task jobs via normal task dispatch. Handles lifecycle (start, failure, restart per policy).
- **Job tracking**: Emitted jobs have `source_type: "event_source"`, `source_id: "{workspace}/{trigger_name}"` for audit trail.
