---
title: Upgrading to 0.18.1 — JSON inputs
description: Behaviour that changes with the json input type release
---

- **Roll out servers first.** Every server replica must run 0.18.1 before any workflow YAML uses `type: json`; an older replica treats `json` as a connection type and fails the step. Upgrade the `stroem` CLI too — older `stroem validate` rejects `type: json`.
- **A connection type named `json` is rejected.**
- **Claim uses the action definition from job creation.** Defaults and input types of an action are read from the definition stored when the job was created; editing an action no longer changes the unclaimed steps of jobs already running (as `type: task` steps already behaved).
- **Re-run requires the source's own task.** `source_job_id` of a run of another task now answers `400` on every re-run.
- **An unknown or inaccessible re-run source answers `404`.** A `source_job_id` that does not exist, and one whose job you may not view, both answer `404` with `Source job not found` on every re-run (previously `400` for an unknown source and `403` or `404` for a denied one). Clients that matched on the old status codes need updating.
- **Re-run reuses a JSON value exactly.** In the Run form, a re-run shows the previous value of each `json` field read-only and reuses the stored value as is; click *Edit* to change it. API clients can ask for the same with `replay_fields`.
- **`stroem validate` warns about numeric secrets.** Secrets written as YAML numbers were never masked; quote them to have them masked (values of 3 characters or fewer are never masked).
