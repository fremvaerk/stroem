---
title: Migration 042 — tags become affinity, exclusive flag added
description: Second flip on worker routing — how to keep existing reservations working
---

Migration `042_worker_exclusive.sql` flips the meaning of `worker.tags` from
**taints** (repellents that reject unmatched steps) to **affinity labels**
(routing hints that match subset-declared steps), and adds a new
`worker.exclusive` boolean to reinstate the reservation pattern that pre-042
users got implicitly from tags.

## What the flip does

Before (post-041, pre-042):

```sql
-- claim SQL
worker.tags <@ step.required_tags   -- worker's taints must all be requested
```

After (post-042):

```sql
-- claim SQL
step.required_tags <@ worker.tags                              -- affinity
AND (NOT worker.exclusive OR worker.tags <@ step.required_tags) -- reservation
```

Concretely:

| Worker `tags` | Worker `exclusive` | Step `required_tags` | Claims? |
|---|---|---|---|
| `[]` | any | `[]` | ✅ |
| `[]` | any | `["claude"]` | ❌ (was ✅ pre-042) |
| `["claude"]` | `false` | `[]` | ✅ (was ❌ pre-042) |
| `["claude"]` | `false` | `["claude"]` | ✅ |
| `["claude"]` | `true` | `[]` | ❌ |
| `["claude"]` | `true` | `["claude"]` | ✅ |

## What changes on upgrade

Migration 042 adds a single column: `worker.exclusive BOOLEAN NOT NULL DEFAULT FALSE`.
Every existing worker starts as **non-exclusive** — no rows are updated by the
migration itself. The behaviour change lives in the claim SQL, not the schema.

## Behavioural changes to double-check

### 1. Reserved workers lose their reservation until you opt in

If a pre-042 worker was configured as `tags: ["batch-runner"]` to reject
non-batch work, that reservation is gone by default. On the first server
restart with the 042 binary:

* Steps with `tags: ["batch-runner"]` still route to that worker (affinity now
  makes them prefer it — actually requires it).
* Steps with no tags will *also* land on that worker (affinity is satisfied by
  the empty set, and `exclusive: false` doesn't block them).

Fix: add `exclusive: true` to the worker's `worker-config.yaml`. The worker
will re-register on next reconnect and start refusing untagged steps again.

```yaml
# worker-config.yaml — pre-042 reserved worker
capabilities: ["script"]
tags: ["batch-runner"]

# worker-config.yaml — post-042 equivalent
capabilities: ["script"]
tags: ["batch-runner"]
exclusive: true              # ← restores the pre-042 refusal semantics
```

### 2. Tagged steps must find a matching worker

Pre-042, a step with `tags: ["gpu"]` could still get claimed by a permissive
`tags: []` worker if the ability matched. Post-042 that leak is closed — the
step's `tags` must be a subset of some worker's `tags`, so a "gpu" step is
routed *only* to workers whose tags include `"gpu"`.

Fix: if you were relying on the fallback, add the tag to the appropriate
worker (`tags: ["gpu"]`), or drop the tag from the step definition.

### 3. Recovery sweep mirrors the new claim SQL

`get_unmatched_ready_steps` uses the same three-clause AND. A step declared
"unmatched" now has strictly stronger criteria than pre-042 — you may see
fewer false positives (steps that no worker could ever claim) but also more
true positives if you didn't add `exclusive: true` where needed.

## API changes

The `/worker/register` and `/worker/jobs/claim` payloads gain an optional
`exclusive` boolean (default `false`). Old worker binaries (post-041,
pre-042) that don't send the field keep working — they just look
non-exclusive to the server, which matches their prior behaviour on the
affinity side.

The worker `WorkerRow` returned by `WorkerRepo::list` now includes the
`exclusive` column.

## Rolling upgrades

Because the schema change is additive and the wire format uses
`#[serde(default)]`, a rolling upgrade is safe in both directions:

* New server + old workers: workers register with `exclusive: false` (default),
  same as pre-042. Affinity semantics apply.
* Old server + new workers: new workers send an `exclusive` field the old
  server ignores. The old server still enforces taint semantics against
  workers it recorded pre-042.

If you rely on a reserved worker, upgrade the server and its `worker-config.yaml`
in the same window — the gap between "new server enforcing affinity" and
"worker declaring `exclusive: true`" is when generic steps can leak onto the
reserved machine.
