---
title: Migration 041 — capabilities / tags split
description: Breaking behaviour change to worker routing, and how to migrate
---

Migration `041_worker_capabilities.sql` splits the pre-041 single-`tags`
axis on both workers and steps into two independent axes:

* **`capabilities`** — what runners a worker supports (`script` / `docker` /
  `kubernetes` / `agent`). Required, no default.
* **`tags`** — free-form **reservation labels** (taints). Empty (default)
  = permissive. Non-empty = the worker ONLY claims steps whose action
  `tags` include ALL of these labels.

The pre-041 model conflated capabilities and tags: a worker with
`tags: ["script","gpu"]` claimed any script step because subset
containment was in one direction only. This let generic script jobs leak
onto GPU-labelled workers — the "how do I preserve a worker for a
specific job" question that motivated the split.

## What changes on upgrade

Migration 041 runs during your first `stroem-server` start-up after the
upgrade. It:

1. Adds `worker.capabilities` (JSONB) and `job_step.required_ability` (TEXT).
2. Splits ability tokens out of every existing worker's `tags` into
   `capabilities` (case-insensitive: `Script`/`Docker` get moved too).
3. Backfills `job_step.required_ability` from each row's `action_type`.
4. Adds an index on `required_ability` used by the claim SQL.
5. Runs entirely inside sqlx's migration transaction — a failure rolls
   the whole thing back.

The migration is defensive against a legacy scalar in a JSONB column
(would otherwise raise `22023 cannot extract elements from a scalar`).

## Behavioural changes to double-check

### 1. Workers whose tags had no ability token

If a pre-041 worker was registered with `tags: ["gpu"]` (relying on
step-side `required_tags` to route), the migration produces
`capabilities: [], tags: ["gpu"]`. **Empty capabilities means the worker
claims nothing.** The migration does not auto-assign `script` (which
would silently promote a non-script worker into claiming script work).

Fix: update `worker-config.yaml` to declare the correct capabilities
explicitly. The worker will re-register on next reconnect.

### 2. Step-side `tags` on actions no longer *require* a matching worker

Under the pre-041 subset check, an action with `tags: ["gpu"]` REQUIRED
a worker whose tags included `gpu`. Post-041 the same YAML PERMITS
gpu-tainted workers to claim it — but a worker with no tags (the
permissive default) will also claim it, since `[] ⊆ ["gpu"]` holds.

**If you were using action-side tags as a *hard requirement*** (e.g.
"only run this training step on a GPU worker"), you must now also add
matching tags to the target worker's `worker-config.yaml`:

```yaml
# Worker (dedicated GPU host)
capabilities:
  - docker
tags:
  - gpu     # reserves this worker for gpu-requesting steps
```

```yaml
# Action (still needs no change — but now it's the *worker's* tags that
# enforce the reservation, not the step's)
train-model:
  type: script
  runner: docker
  tags: ["gpu"]
```

If instead you just want the step to prefer a GPU worker but tolerate
CPU fallback, no change is needed.

### 3. Rolling upgrade compatibility

The server includes a shim that translates pre-041 `tags: ["script","docker"]`
worker payloads to the new `capabilities` field on the fly, so old
worker binaries keep registering while you're rolling. A deprecation
warning is logged for each such request. Update your worker configs
before the next release removes the shim.

## Common mistakes the new validation catches

* `capabilities: []` — server returns 400 at register.
* `capabilities: ["scipt"]` (typo) — server returns 400.
* `tags: ["script"]` — server returns 400 (ability tokens forbidden in
  the reservation axis).
* Missing `capabilities:` in the YAML — validator emits a clear
  migration hint (post-serde-default parsing).

## SQL reference

| Concept | Pre-041 | Post-041 |
|---|---|---|
| Worker → server register field | `tags` (mixed) | `capabilities` + `tags` |
| Step → derived routing fields | `required_tags` (mixed) | `required_ability` + `required_tags` |
| Claim check | `required_tags <@ worker.tags` (subset) | `worker.capabilities @> to_jsonb(required_ability)` AND `worker.tags <@ required_tags` |
| Server-dispatched step types excluded | `task`, `agent`, `approval` | `task`, `agent`, `approval`, `event_source` |
