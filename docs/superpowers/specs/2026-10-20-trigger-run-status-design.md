# Trigger run status: last run, result and missed runs

Status: approved by the user 2026-10-20, in Codex review (revision 5 — Codex rounds 1–2 fixed;
history in §11).

Markers used below: **(user)** = decided by the user in the brainstorm; **(my call)** = proposed
by the designer, open to the user's correction; **(builder's call)** = left to the implementer.

## 1. Intent

On 2026-10-19/20 no Strøm server replica was leader for 16.5 hours (separate fix: branch
`fix/leader-stale-lock`). Every scheduled trigger stopped, including `jobs/recalc-nightly`
(01:00 UTC). Nothing in the UI showed it: the trigger list only shows the *next* run. The
user found the missing run by looking at the data.

The user wants the UI to answer, at a glance, for every trigger: **when did it last run, did
that run succeed, and did it miss scheduled runs since then?** — on the workspace page, on the
task page, and on the Dashboard.

## 2. Goals

1. For every trigger, show its **last run**: time (a link to that job) and **result** (the job
   status) **(user)**.
2. For every enabled scheduler trigger, show a **missed-runs mark** when scheduled fire times
   passed after the last run without a job: the number of missed fire times and the earliest
   one, e.g. `1 missed · 20 Oct 03:00` **(user)**.
3. **The mark resets by itself** as soon as the trigger runs again **(user)**. A gap that is
   followed by a normal run is not shown any more (e.g. yesterday's 16-hour gap of
   `zd-cu-sync`, which has run every 5 minutes since 02:20).
4. Workspace page: layout "C" **(user)**, §6.1 — a "Last run" column plus a warning box above
   the trigger table when any trigger has missed runs.
5. Task page: the trigger card shows the same last-run and missed information **(user, did not
   object)**.
6. Dashboard: layout "C" **(user)**, §6.3 — a "Triggers that need attention" table that lists,
   across all workspaces, triggers with missed runs **and** triggers whose last run failed.

## 3. Non-goals

- **No notifications** (Slack, e-mail, hooks) for missed runs **(user)**: "this page is
  sufficient". (A Prometheus alert on `sum(stroem_leader_status) == 0` belongs to the leader
  fix, not here.)
- **No replay** of missed fires. The scheduler keeps its state in memory (`scheduler.rs`
  `TriggerState.last_run/next_run`; a restart computes the next fire from *now*) and never
  replays a missed fire; that is an open item in `docs/internal/TODO.md` ("missed-fire
  catch-up for cron triggers") and stays out of scope.
- **No history of past gaps** **(user)** (see goal 3).
- No change to how or when triggers fire.

## 4. Anti-goals (it fails even if it "works")

- **False marks.** A mark that appears when nothing is wrong teaches people to ignore it.
  New triggers, disabled triggers, workspaces with `triggers: false` on this server, and a run
  that starts a little late must not produce a mark (§5.3). Known residual cases are listed in
  §8.
- **A slow Dashboard.** The Dashboard is the landing page and reloads every 5 s
  (`ui/src/pages/dashboard.tsx`); the new block must cost a bounded number of cheap queries per
  request, never one query per trigger and never a scan of a trigger's whole job history.
- **Leaking triggers or jobs.** Users see only triggers of tasks they may view (same rule as
  the existing trigger list), and only job data they may view (§7.3).

## 5. Definitions

### 5.1 A run of a trigger

A job whose `source_type` / `source_id` identify the trigger. `source_id` is always
`"{workspace}/{trigger_key}"`, where `workspace` is the workspace that **defines** the trigger
and `trigger_key` is the trigger's key in that workspace's `triggers:` map — also when the
target is a cross-workspace `ws.task` or carries `ref:` (the job then lives in the target
workspace, so match on `source_id`, never on `job.workspace`).

| Trigger type | `source_type` | Written by | Extra condition |
|---|---|---|---|
| scheduler | `trigger` | `scheduler.rs::fire_trigger` → `trigger_target::create_target_job`; `concurrency: skip` writes a `status = 'skipped'` row via `JobRepo::create_skipped` | — (a `skipped` row is a run) |
| webhook | `webhook` | `web/hooks.rs` (`source_id = "{ws}/{trigger_key}"` — the map key, **not** the webhook's `name:` URL segment) | — |
| event source | `event_source` | consumer: `event_source.rs::create_event_source_job` | the job input has the `_fingerprint` key (the consumer marker, below) |

Event-source **emitted** jobs (`web/worker_api/event_source.rs::emit_event`) use the same
`source_type = 'event_source'` and the same `source_id` (the worker sends it; the endpoint
checks it is `"{req.workspace}/{trigger}"` and that `req.task` is the trigger's `target_task`).
Both kinds live in the defining workspace. Emitted jobs are not runs of the trigger.

**Consumer marker.** A consumer job is told apart from an emitted job by a fact persisted on the
job row, not by the trigger's current configuration: `source_type = 'event_source' AND input ?
'_fingerprint'`. Every consumer job ever written carries `_fingerprint` in its input — the first
release (v0.14.0/v0.14.1, 084a454f) wrote `{"_fingerprint": …}` with the synthetic task name
`_event_source:{trigger}`; since the refactor 820839b8 (v0.14.2) the input also holds the
`_event_source` object and `task_name` is the trigger's `task` — and the event-source manager
reads exactly this key to recognise its consumers' configuration
(`event_source.rs::load_active_event_source_jobs`). `_event_source` alone (what the claim uses
today, `web/worker_api/jobs.rs` `event_source_config`) would miss the v0.14.0/v0.14.1 rows, and
retention is off by default (`config.rs`, `RetentionConfig`), so those rows can still be a
trigger's newest consumer. The trigger's current `task` / `target_task` must **not** be used: validation only checks the current
configuration (`stroem-common/src/validation.rs`, consumer ≠ target), so after the consumer
`task` changes (A → C) or consumer and target swap (A/B → B/A), a `task_name` filter would hide
real consumer history or count old emitted jobs as runs. With the marker, all of the trigger's
consumer jobs count, whatever their `task_name`. A v0.14.0/v0.14.1 consumer's task name is not
a live task, so for a non-admin under ACL its job path (`acl::job_task_path`) normally denies it
and the UI shows "no access" (§7.3). Accepted residual: an emitted payload that itself carries
a `_fingerprint` key (emitted input = trigger `input` defaults merged with the payload object,
`stroem-worker/src/executor.rs`) is counted as a consumer — the claim has the same weakness for
`_event_source` (TODO.md).

Not runs of the trigger **(user)**: any other job of the same task — a manual run
(`source_type = user/api/mcp`), a Re-run (`rerun`) or Restart (`restart`) of the trigger's run,
a run by another trigger, a `type: task` child or a hook job. They neither clear a missed mark
nor change the result; only the trigger's next own run does. The one exception is task retry
(`source_type = 'retry'`, `source_id` = the failed job's id): it is the server finishing the
trigger's own run, see §5.2.

A fire that never creates a job — logged `Trigger '…' MISSED: …` (workspace unavailable, target
not resolvable, trigger redefined between load and fire) or `failed to create job` — leaves no
row. That fire counts as missed (§5.3), which is the intent.

### 5.2 Last run and result

- **Last fire** `L` = the `created_at` of the trigger's newest run of **any** status, `skipped`
  included. It drives the missed mark (§5.3): a `skipped` row proves the scheduler fired on time.
- **Last run** (what the UI shows) = the trigger's newest run that is **not** `skipped` (my call,
  after the builder check). With `concurrency: skip`, a long job writes a `skipped` row at
  every fire while it runs; showing those would hide the real job's `running` or `failed`
  state. Only when the trigger has no run other than `skipped` rows, show the newest `skipped`
  row. Show its `created_at` (local time, like the rest of the UI) as a link to the job
  (`/jobs/{id}`) **(user, did not object)**. For a `running` event-source consumer the time
  reads "running since <created_at>".
- **Result** = that job's current `status`: `pending`, `running`, `completed`, `failed`,
  `cancelled` or `skipped` (the full `job_status_check` set, migration 021) **(user, did not
  object)**.
- **Task retry** (my call — refines "job status as is"; the user kept it when choosing "only the
  trigger's jobs"): when the last run has a task-retry chain, the result is the status of the
  **newest job in that chain** — every retry job's `retry_of_job_id` is the chain's root, the
  trigger's run (`settlement/retry.rs`, `JobRepo::link_retry_tx`), so the newest is the one with
  the highest `retry_attempt`. The time link still points at the trigger's own run. Without
  this, a nightly run that failed once and succeeded on retry would sit on the Dashboard as
  "last run failed" for a day.
- Re-run and Restart do **not** change the result **(user)** — see §5.1 and §8.
- No run in the database → "never" and no missed mark (§5.3).

### 5.3 Missed runs (scheduler triggers only)

For an **enabled** scheduler trigger in a workspace whose triggers are enabled on this server
(`WorkspaceManager::triggers_enabled`), with a last fire `L` (§5.2):

```
missed = { t ∈ occurrences(cron, timezone) : L < t ≤ now − grace }
```

- Show `count` and `first = min(missed)`; no mark when `missed` is empty.
- `occurrences` uses the trigger's **current** cron and timezone, computed the way the existing
  code does: `stroem_common::cron::parse` (croner, 5/6 fields, seconds optional), then
  occurrences in the trigger's IANA timezone (`chrono-tz`) converted to UTC — as
  `web/api/triggers.rs::compute_next_runs` (`iter_after` on the local time) and
  `scheduler::compute_next_run` (`find_next_occurrence`) do. Follow the scheduler for bad
  values: an unparsable cron → no mark (the scheduler skips the trigger); an unparsable
  timezone → UTC (the scheduler and `compute_next_runs` both fall back to UTC).
- **Grace** (lateness that still counts as on time): **5 minutes** (my call). The scheduler
  loop sleeps at most `scheduler::MAX_SLEEP` (30 s) and fires when `now >= next_run`, so a fire
  is normally late by seconds (prod job data before the incident: 33–52 s); a
  `force_refresh` trigger reloads its workspace first and can be later. 5 minutes keeps the
  mark quiet for slow fires and still shows a missed daily run within minutes. A constant; no
  config.
- **No last fire → no mark** — we cannot know when a new trigger was added.
- **Reset**: by construction, the trigger's next own run moves `L` past the gap **(user)**. A
  manual run, Re-run or Restart of the task does not reset it (§5.1).
- **Cap**: stop counting at 1000 and show `1000+` (my call), so a `* * * * * *` trigger that
  stopped long ago does not iterate unbounded.
- Disabled triggers (`enabled: false`) and `triggers: false` workspaces: no mark (they are not
  expected to fire). Their last run is still shown.
- Webhook and event-source triggers: never a missed mark (no schedule) **(user, did not
  object)**.

### 5.4 "Needs attention" (Dashboard)

A trigger needs attention when it has a missed-runs mark, or its result (§5.2) is `failed`
**(user)**. `cancelled` and `skipped` do not count (my call — the user asked for failures; a
`skipped` row is `concurrency: skip` doing its job, and a cancel is a human or
`cancel_previous` decision). It stops needing attention when the next run happens (missed) or
the next result is not `failed`. Disabled triggers and `triggers: false` workspaces never need
attention (my call — nobody expects them to run, and their last failure is old news).

## 6. UI (user-chosen layouts)

Mockups (HTML) were shown in the brainstorm; the chosen ones are described here.

### 6.1 Workspace page (`ui/src/pages/workspace-detail.tsx`, Triggers card)

Today the table has the columns Name, Type, Task, Schedule, Next run.

- New column **"Last run"** between "Schedule" and "Next run" **(user)**:
  - a status pill (the existing `components/status-badge.tsx` `StatusBadge`) + the time as a
    link to the job; "never" in muted text when there is no run;
  - when missed runs exist, a second line with an amber pill: `N missed · <first missed time>`.
- **Warning box** above the table, only when at least one trigger in this workspace has missed
  runs **(user)**: "⚠ N triggers missed scheduled runs — `<trigger>`, `<trigger>`. Run the
  tasks manually if you need the data. The mark stays until each trigger's next scheduled
  run." (amber border/background, like the mockup; the last sentence is my call — it follows
  from the user's choice in §5.1 that a manual run does not clear the mark).

### 6.2 Task page (`ui/src/pages/task-detail.tsx`, Triggers card)

Each trigger entry (today: name, type, cron, enabled badge, next runs) shows the same last-run
pill/link and the missed pill **(user, did not object)**. No warning box (my call — the card
is small; the pill is enough).

### 6.3 Dashboard (`ui/src/pages/dashboard.tsx`)

A card **"Triggers that need attention"** between the four counters and "Recent Jobs"
**(user)**. Hidden entirely when nothing needs attention, and when the call fails (the rest of
the Dashboard must still render). One row per trigger:

| Workspace | Trigger | Problem | Last run |
|---|---|---|---|
| link to `/workspaces/<ws>` | name, link to the workspace page | amber `N missed · <time>` and/or red `last run failed` | status pill + time link to the job |

Order (my call): missed first (most missed first), then failed (most recent first).
Workspace shown by key, like the rest of the job/task UI.

## 7. Server side

### 7.1 API

- `GET /api/workspaces/{ws}/triggers` (`web/api/triggers.rs::list_triggers`, `TriggerInfo`)
  and the task detail response (`web/api/tasks.rs`, `triggers: Vec<TriggerInfo>`, the
  triggers whose `task()` is this task) gain the last-run and missed data per trigger —
  additive fields; existing clients unaffected. `TriggerInfo::from_def` is synchronous today;
  the new data needs the DB, so it is filled in by the handlers after one batched lookup
  (builder's call how).
- The Dashboard needs one cross-workspace call (new endpoint, e.g.
  `GET /api/triggers/attention`, or an extension of an existing one — builder's call) that
  returns only the triggers that need attention.
- Field names and shapes: builder's call.

### 7.2 Queries and index

- Last runs are read with **one query per request**, not per trigger. Per trigger the query
  needs: the last fire (newest row, any status), the last run (newest non-`skipped` row, or
  for an event source the newest consumer job) and, when that run has a retry chain, the
  chain's newest job (`retry_of_job_id` = the run, `idx_job_retry_of`, migration 027; ≤ 10
  rows).
- **Every one of these lookups is a direct index probe whose cost does not depend on how many
  other rows the trigger's `source_id` has** — not on its run history (a 5-minute trigger has
  ~8 600 runs per 30 days of retention), not on `skipped` rows (task timeouts and the server
  default timeouts are optional, so one blocked run under `concurrency: skip` can collect an
  unbounded number of `skipped` rows — a `* * * * * *` trigger blocked for 30 days writes ~2.6
  million), and not on emitted event-source jobs (a long-lived consumer can emit without
  limit). An index that holds those rows and is walked past them does not meet this.
- A shape that meets it (the builder may choose an equivalent one): two partial indexes in one
  new migration,
  - *runs*: `job (source_type, source_id, created_at DESC) WHERE (source_type IN ('trigger',
    'webhook') AND status <> 'skipped') OR (source_type = 'event_source' AND input ?
    '_fingerprint')` — the newest row is the last run (`?` on the `jsonb` `input` column is
    immutable, so it is allowed in a partial-index predicate);
  - *skipped fires*: `job (source_id, created_at DESC) WHERE source_type = 'trigger' AND status
    = 'skipped'` — the last fire `L` is the later of the newest run and the newest skipped row;

  each probed with `unnest(…) CROSS JOIN LATERAL (SELECT … WHERE <the index predicate> AND
  source_type = … AND source_id = … ORDER BY created_at DESC LIMIT 1)`. Queries must repeat the
  partial-index predicate so the planner can use it. A plain `DISTINCT ON` over all matching
  rows reads every run and is not acceptable for the Dashboard's 5-second refresh. Both
  indexes are also small: they hold only trigger jobs, not manual, child, hook or emitted ones.
- Prove it in stroem-db tests with `EXPLAIN` (like
  `templated_refs_test::claim_and_unmatched_sweep_still_use_a_ready_partial_index`): the
  last-run probe uses the *runs* index with thousands of `skipped` rows and thousands of
  emitted jobs newer than the run, and does no sequential scan.
- No existing index serves this. `idx_job_source_active (source_type, source_id)` is partial
  (`status IN ('pending','running')`, migration 013); `idx_job_source_type (source_type)` is
  partial and excludes `'trigger'` (migration 031). `job` is large in prod: follow CLAUDE.md
  § Database — own migration (053), `CREATE INDEX IF NOT EXISTS` under new names, a header
  with the manual `CREATE INDEX CONCURRENTLY` pre-run (migration 050 is the model), and a
  user-facing upgrade note under `docs/src/content/docs/operations/`.

### 7.3 ACL

- A trigger is listed only when its task is not `Deny` for the user — exactly the existing
  `list_triggers` rule (`acl.evaluate` on the defining workspace and the task's live folder);
  admins and an unconfigured ACL see everything. The Dashboard call applies the same rule per
  workspace.
- A run can live elsewhere than the trigger suggests: a cross-workspace `ws.task` target
  lives in the target workspace, and a pinned (`ref:`) job is authorised by its own
  `job.task_folder` (CLAUDE.md § Git refs → "Job-scoped read paths authorise with the job
  row"). So the last-run fields (job id, time, result) are returned only when the user may
  view that **job** by the job-row rule (`acl::job_task_path`); otherwise they are omitted
  (the UI shows "no access") and the missed mark is still computed — it is a fact about the
  trigger, which the user may see (my call). The same rule applies to the job a task-retry
  result comes from. A trigger whose failed run the user may not view is not listed on the
  Dashboard as "last run failed" (builder's call, from the no-leak anti-goal).
- If the user's ACL context cannot be loaded, the new endpoints fail (error), never fall
  open (builder's call). Today `list_triggers` silently shows every trigger in that case
  (`if let Ok(...)`); it changes to fail closed with this feature.

### 7.4 Other constraints

- The missed computation is a pure function (cron, timezone, last run, now, grace, cap) →
  unit-testable without a DB; put it next to `compute_next_runs` or in `stroem-common`
  (builder's call).
- Compute on any replica (read-only). It does not depend on which replica is leader — which is
  the point: it shows a leaderless cluster too.
- Redaction: return only identifiers, timestamps and status — never job input/output — so the
  job-content redaction rules (CLAUDE.md § Git refs → Redaction) are not involved.
- A workspace in load error has no config (`get_config` → `None`), so its triggers cannot be
  listed: the workspace page already shows the load error, and the Dashboard skips that
  workspace. After it loads again, the marks for runs missed during the outage appear
  (until the next run).

## 8. Known limitations (accepted, document them)

- A changed cron/timezone is evaluated against a last run made under the old schedule; this
  can show a false mark until the trigger fires once on the new schedule.
- A trigger re-enabled after `enabled: false` (or a workspace switched back from
  `triggers: false`) shows the fire times it skipped while disabled as missed, until it fires
  again — the database does not record when it was enabled.
- Job retention (`retention.job_days`, counted from `completed_at`) can delete the last run of
  a rarely firing trigger → "never", no mark.
- A trigger renamed in YAML is a new trigger (new `source_id`) → "never" until it fires.
- The mark is not shown for a trigger that has never fired at all (no history).
- A manual run, Re-run or Restart of the task after a missed or failed run does not clear the
  mark or the "last run failed" state **(user)**; they stay until the trigger's next own run
  (task retry excepted, §5.2). The warning box says so (§6.1).
- The Dashboard does not list triggers of a workspace that is currently in load error (§7.4).

## 9. Left to the builder

API field names and the Dashboard endpoint shape; pill colours beyond reusing `StatusBadge`;
loading and error states (the Dashboard must still render when the new call fails); how often
the Dashboard refreshes the new block (it may refresh less often than every 5 s); the exact
index shape, within §7.2's requirement; test split.

## 10. Tests and docs (repo rules)

- Unit: the missed computation (no history, on-time run, late run within grace, 1 missed,
  many missed + cap, 6-field cron, timezone + DST, unparsable cron), the "needs attention"
  rule, the task-retry result.
- Integration (`crates/stroem-server/tests`, a `mod` in `tests/main.rs`, fixtures from
  `stroem-test-support`): the endpoints return last run + missed for seeded jobs; event-source
  consumer vs emitted jobs; cross-workspace target matched by `source_id`; `skipped` row
  counts as a fire but the shown last run is the real job behind the `skipped` rows; a manual
  run / Re-run / Restart does not clear a mark or change the result, a task retry does;
  disabled trigger and `triggers: false` give no mark; ACL hides denied
  triggers and omits last-run fields of a job the user may not view; the Dashboard call
  returns only "needs attention" triggers; an event source whose consumer `task` changed
  (A → C) or whose consumer and target swapped still counts its consumer jobs, never its
  emitted ones; a v0.14.1-style consumer row (`_event_source:{trigger}` task, input with only
  `_fingerprint`) is found; a run blocked behind many `skipped` rows is still found; the indexes are
  used (`EXPLAIN`, §7.2).
- Migration test in stroem-db (`migration_test.rs`).
- UI: vitest for the three parts (column, warning box, Dashboard card hidden/shown/failed
  call).
- Docs: `docs/src/content/docs/guides/triggers.md` (what "last run" and "missed" mean, grace,
  limitations), the upgrade note for the index (§7.2), CLAUDE.md (scheduler / UI sections),
  `docs/internal/TODO.md`, CONTEXT.md if a new domain term is introduced.

## 11. Revision history

- **Rev 1** (brainstorm, 2026-10-20): draft; layouts C chosen by the user.
- **Rev 2**: facts checked against the code (`source_id` uses the trigger map key, not a
  webhook's `name:`; emitted event-source jobs validated by `emit_event`; MISSED fires leave no
  row; `idx_job_source_type` excludes `trigger`); added the job-row ACL for runs (§7.3), the
  query-cost constraint and event-source emitted-job problem (§7.2), the task-retry result
  (§5.2), the re-enabled-trigger and load-error limitations (§8).
- **Rev 3** (builder check): the user decided that only the trigger's own jobs (plus task
  retry) count — a manual run, Re-run or Restart does not clear a mark or change the result
  (§5.1, §8); warning box text follows. My calls: the last run shown skips `skipped` rows while
  the last fire `L` does not (§5.2); bad timezone → UTC like the scheduler (§5.3); ACL load
  failure fails closed; a failed run the user may not view is not on the Dashboard (§7.3).
- **Rev 4** (Codex round 1): the last-run probe no longer walks past `skipped` rows (their
  number is unbounded — timeouts are optional) — §7.2 now requires probes independent of
  `skipped` rows, emitted jobs and history, with a two-partial-index shape; event-source
  consumers are identified by the persisted `_event_source` input marker the claim already
  uses, not by the trigger's current `task` (§5.1).
- **Rev 5** (Codex round 2): the consumer marker is `input ? '_fingerprint'`, not
  `_event_source` — consumers written by v0.14.0/v0.14.1 carry only `_fingerprint` (and the
  synthetic task name `_event_source:{trigger}`); every later consumer carries both.
