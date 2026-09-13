# Task-State Storage Hardening — Design

Status: problem statement, not yet designed
Ships in: unscheduled; after `2026-09-11-render-context-owner-design.md`

Split out of the render-context design at its revision 7. Five review rounds
of that design each pulled one more piece of task-state storage into scope
to close the previous round's race, and each round found the next race one
layer down. The defects are real, pre-existing, and independent of the
render context; one of them is the 11 September 2026 architecture review's
red-band defect "template and mount can disagree". They need their own
design. Line numbers cite `main` at `9916985`.

## 1. Problem

A task-state snapshot is one row (`task_state` / `workspace_state`) plus one
tarball in the archive. The pairing is not stable.

**(i) Keys are per job, so one blob serves many rows.** Key =
`{prefix}{ws}/{task}/{job_id}.tar.gz` (`state_storage.rs:42-49`; global
`{prefix}__global__/{ws}/{job_id}.tar.gz`, `:57-62`). The blob is stored
before the row is inserted (`state.rs:404` then `:423`). A job whose
sequential steps each write state produces N rows that all name the same
blob, which holds only the last upload. Under "A stores; B stores and
commits; A commits", the latest row is A and the blob is B.

**(ii) The worker does not download what the claim named.** The claim
response carries `state_storage_key` (`jobs.rs:533-606`), but the worker
only tests its presence and then requests `(workspace, task)`
(`poller.rs:368`, `client.rs:619`); the server selects *latest* again
(`state.rs:329`). Any upload between claim and download changes what is
mounted at `/state`, while the templates were rendered against the earlier
snapshot. This is the red-band defect. Closing it is a wire change: the
download request must carry the key (or snapshot id) the claim named, and a
mixed fleet must keep working during rollout.

**(iii) Pruning deletes a blob that a retained row still references.**
`insert_and_prune` deletes rows beyond `keep` and returns their keys
(`task_state.rs:127-141`; workspace twin); the handler deletes each key
(`state.rs:459`, and the API path). With (i), pruning an older same-job row
deletes the blob the newer, retained row points at. Data loss today. Any
re-keying must also handle the transition: legacy rows sharing a key survive
until all but one are pruned, and the last prune of a shared key must not
delete it while a sibling remains.

**(iv) Extractor semantics are order-dependent and disagree with the
worker.** `extract_state_json` returns the *first* `state.json` at any depth
in tar order (`state.rs:215`). The API upload path sorts entries
lexicographically (`state_upload.rs:208`), so `a/state.json` precedes the
root one. Duplicate root entries take the first, whereas the worker's
`Archive::unpack` (`poller.rs:55`) lets the last overwrite. `has_json` is a
caller-supplied flag on the worker API (`state.rs:302`) whose contract does
not require root placement (`:362`); nested-only extraction is covered by a
test (`state.rs:293`). Any policy change is a compatibility change for
flagged nested-only snapshots.

**(v) Rows without a persisted sidecar cannot be backfilled** while (i)
holds: reading a legacy row's blob may return a later upload's sidecar. The
render-context design therefore ships without a backfill; rows written
before migration 047, or by a pre-047 replica during rollout, have no
`state` at cascade time until the task uploads again.

## 2. Sketch (to be designed)

1. **Per-snapshot keys.** Generate the row id before storing; key the blob by
   it; pass `Some(id)` to `insert_and_prune`, whose `snapshot_id` parameter
   already exists (`task_state.rs:108-110`). Four sites: `state.rs:109/128`,
   `:404/423`, `state_upload.rs:398/511`, `:605/719`. Legacy rows keep their
   keys.
2. **Prune with a surviving-reference check.** Delete a returned key only if
   no remaining row in the table references it. Needed for the legacy
   transition regardless of (1).
3. **Download by the claim-supplied key.** `ClaimResponse` already carries
   it; the worker's state-download request gains it; the server serves that
   key rather than re-selecting latest. Mixed fleet: a new server must accept
   a keyless request from an old worker (serve latest, as today), and an old
   server must ignore an unknown field from a new worker. Order of rollout to
   be specified.
4. **Extractor policy.** Decide root-only vs any-depth, first vs last, and
   whether `has_json` is trusted or derived; state the compatibility
   consequence for existing flagged nested-only snapshots (`state.rs:293`).
5. **Backfill of `state_json`** for rows written before 047 or by pre-047
   replicas — safe only after (1) and (2), because then no writer targets a
   legacy key and a prune cannot remove a blob a row still needs. Leader-
   gated, idempotent, and it must cope with rows inserted by a still-running
   old replica after its pass.

## 3. Constraints

- (3) is a worker-protocol change; the render-context design was ranked
  first for having none. This design must state the mixed-fleet contract
  and the rollout order explicitly.
- Migration 047 (the sidecar column) ships first, in the render-context
  design; this design adds no column.
- `max_snapshots` retention semantics must be preserved as observed by the
  API (`list`, `get`).

## 4. Non-goals

The render context itself; the `has_json` flag's meaning as "the tarball
carries a sidecar"; the `/state` mount layout on the worker.

## 5. Open questions

- Is the same-job double-upload (sequential steps each writing state) a
  supported pattern, or should a job produce at most one task-state upload?
  If the latter, (i) and (iii) shrink to a validation rule.
- Should snapshot downloads be addressed by row id rather than key, so the
  key format is never part of the wire contract?
