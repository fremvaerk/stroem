---
title: Migrations 051 and 052 — templated refs
description: Schema changes and the rollout rule for templated ref values
---

Migrations `051_templated_refs.sql` and `052_templated_refs_validate.sql`
ship with [templated refs](/guides/git-refs/#templated-refs).

## The migrations

**051** is additive:

| Table | Change |
|---|---|
| `job_step` | `ref_template`, `resolve_lease_owner`, `resolve_lease_until` (nullable) |
| `job_ref_pin` | new: one commit per job, owner workspace and ref |
| `job_step` | the action-type check accepts `deferred`, added `NOT VALID` (no table scan); skipped when the check already accepts `deferred` |

**052** runs `ALTER TABLE job_step VALIDATE CONSTRAINT job_step_action_type_check`.
It scans all of `job_step` under a `SHARE UPDATE EXCLUSIVE` lock, so writes
continue. They are two migrations because each runs in its own transaction: a
validation inside 051 would hold 051's exclusive lock for the whole scan.

The scan runs at the first server start of this release, and that start
waits for it. On a large `job_step` table, run the scan before you deploy:

```sql
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
  CHECK (action_type IN ('script', 'shell', 'docker', 'pod', 'task', 'agent',
                         'approval', 'event_source', 'deferred')) NOT VALID;
ALTER TABLE job_step VALIDATE CONSTRAINT job_step_action_type_check;
```

The first two statements are 051's widening of the check: a short exclusive
lock and no scan, and the release you run now writes only values the wider
check accepts. The third is 052's scan (writes continue). Afterwards 051 sees a check that already
accepts `deferred` and leaves it as it is, and 052 finds it valid and does
nothing.

## Rollout rule

**Merge no workflow YAML with a templated `ref:` before every server replica
runs this release.**

- An older replica refuses a templated ref at job creation (`400`).
- But a deferred step created by a new replica can reach an older leader:
  its unmatched-step sweep fails the step after
  `recovery.unmatched_step_timeout_secs`, and an older replica never
  resolves it.
- Workers do not change: no claim ever returns a deferred step.
- Give every webhook with a templated `ref:` a `secret`: without one, any
  caller can force one `ls-remote` per unknown ref value, under the owner
  workspace's resolve lock.

## What changes without templated refs

- `POST /worker/jobs/claim` answers `400` for a capability outside `script`,
  `docker`, `kubernetes` and `agent`, as `/worker/register` always did. Strøm
  workers send their registered capabilities, so only a custom client that
  sends an unknown token sees a change.
- The PinStore span of a ref resolution no longer records the ref (server
  logs only).
- The PinStore keeps the commits a live job has resolved through templated
  refs; recovery has a new phase (4.6) that re-advances a deferred step left
  `ready` by a crash.
