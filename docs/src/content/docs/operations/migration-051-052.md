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
| `job_step` | the action-type check accepts `deferred`, added `NOT VALID` (no table scan) |

**052** runs `ALTER TABLE job_step VALIDATE CONSTRAINT job_step_action_type_check`.
It scans `job_step` under a `SHARE UPDATE EXCLUSIVE` lock, so writes continue.
They are two migrations because each runs in its own transaction: a
validation inside 051 would hold 051's exclusive lock for the whole scan.

## Rollout rule

**Merge no workflow YAML with a templated `ref:` before every server replica
runs this release.**

- An older replica refuses a templated ref at job creation (`400`).
- But a deferred step created by a new replica can reach an older leader:
  its unmatched-step sweep fails the step after
  `recovery.unmatched_step_timeout_secs`, and an older replica never
  resolves it.
- Workers do not change: no claim ever returns a deferred step.

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
