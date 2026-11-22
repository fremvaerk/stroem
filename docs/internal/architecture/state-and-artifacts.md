# State and artifacts

Task state snapshots, global workspace state and job artifacts. Moved word for word from `CLAUDE.md`, which indexes this file. A `§ <Title>` reference names a `###` section in any file of `docs/internal/architecture/`.

### Task State Snapshots
- Immutable state snapshots persisted across job runs per workspace+task
- `STATE:` stdout protocol: `STATE: {"key": "value"}` — structured state saved as `state.json` in snapshot tarball
- `/state` directory: previous snapshot mounted read-only, `/state-out`: writable directory for new state
- `StateArchive` trait with S3 and Local implementations, separate from `LogArchive`
- DB: `task_state` table tracks snapshots (id, workspace, task_name, job_id, storage_key, size_bytes, has_json, created_at)
- Snapshot rows resolved pool-only once per entry (`render_context::latest_snapshots`); the parsed `state.json` is persisted at upload (migration 047, `json` column bound as text with `::json` — never `jsonb`, never bound as `Value`: the NUL escape). Pre-047 rows: claim falls back to the archive for rendering only; no backfill (see `docs/superpowers/specs/2026-09-13-task-state-storage-hardening-design.md`). Each claim re-resolves, so intra-job state propagation for sequential steps still holds (state written by step N is visible to step N+1's templates and mount).
- Worker API: `GET /worker/state/{ws}/{task}?job_id=` (download), `POST /worker/state/{ws}/{task}/{job_id}` (upload). The state coordinates `(workspace, task, git_ref)` are the JOB's (`StateCoords::of_job`): an upload ignores the path's `{ws}`/`{task}`, a download with `?job_id=` (the worker always sends it) reads the job's; without it (an old worker) the path and the NULL partition. Partitioned per ref for pinned jobs — see § Git refs.
- Tera templates: `{{ state.key }}` and `when: "{{ not state or state.days_remaining < 30 }}"` — a `when` expression MUST be wrapped in `{{ }}`; `evaluate_condition` (`template.rs:338`) renders the string and tests truthiness, so a bare expression renders to itself and is ALWAYS true (a silent no-op)
- Config: optional `state_storage` section (prefix, max_snapshots, optional archive override). Defaults to log archive backend.
- Retention: `max_snapshots` per task (default 5), pruned on upload
- Runners: Shell (env vars), Docker (bind mounts `/state:ro` + `/state-out:rw`), Kube (emptyDir volumes)
- Always available, no opt-in. Upload only triggered if `/state-out` has content or `STATE:` lines emitted.
- Snapshots can also be uploaded out-of-band via `POST /api/workspaces/{ws}/tasks/{task}/state` and `POST /api/workspaces/{ws}/state` (see `docs/src/content/docs/guides/task-state.md` §"Uploading state manually"). Creates a synthetic `source_type="upload"` job for audit.

### Global Workspace State
- Workspace-scoped state shared across all tasks (any task can read and write)
- `GLOBAL_STATE:` stdout protocol: `GLOBAL_STATE: {"key": "value"}` — structured state in `state.json`
- `/global-state` directory: previous snapshot read-only, `/global-state-out`: writable for new state
- DB: `workspace_state` table — same structure as `task_state` but scoped by `workspace` only (task_name for provenance)
- Worker API: `GET /worker/global-state/{ws}?job_id=`, `POST /worker/global-state/{ws}/{job_id}` — coordinates from the job, as for task state
- Tera templates: `{{ global_state.key }}` — separate namespace from task state `{{ state.key }}`
- Storage key format: `{prefix}__global__/{workspace}/{job_id}.tar.gz`
- Same `StateArchive` backend and retention model as task state
- Concurrent writes: last writer wins (immutable snapshots, latest by `created_at`)

### Artifacts
- Per-job opaque files produced by successful steps. Convention dir `/artifacts/` (or `$ARTIFACTS_DIR`); recursive scan, dotfiles included, symlinks skipped + warned.
- Per-file 100 MiB cap, per-job 1 GiB cap, both configurable under `artifact_storage:`.
- Success-only upload: failed/cancelled steps discard `/artifacts/`. Upload retried 3× with backoff; terminal failure fails the step AND cleans up already-uploaded blobs for that step.
- Per-job namespace, `UNIQUE(job_id, name)`, last-writer-wins on collision (for_each authors must template filenames).
- Worker sniffs Content-Type via `infer` crate; server stores verbatim, applies `X-Content-Type-Options: nosniff` and inline-when-safe `Content-Disposition` — inline only for `web/api/artifacts.rs::SAFE_INLINE` (`image/png`, `image/jpeg`, `image/gif`, `image/webp`, `text/plain`; NOT PDF or Markdown), everything else (HTML/SVG/XML/JSON included) attachment. The input-file download reuses `SAFE_INLINE` / `disposition_for`.
- Storage via `BlobArchive` trait (unified backend for logs, state, artifacts, input files). S3 + Local impls. The trait's default `put_stream` / `get_stream` collect the whole body in memory; Local and S3 override both since file inputs (§ File Inputs), so the user artifact download (`get_stream`) streams. Artifact UPLOAD still buffers: `web/worker_api/artifacts.rs` takes the request body as `Bytes` and calls `put` (one whole file per request in memory); MCP `get_artifact` reads with `get` behind its 1 MiB cap.
- Retention: cascades with `job` row. FK `RESTRICT` + explicit two-phase delete (blob → row).
- Runner support: shell, `script:docker`, `type:docker`. Kube modes deferred (same gap as state file-mount).
- Hooks: `hook.artifacts` is a list of `{name, content_type, size_bytes, url, step_name, created_at}`.
- ACL: `View` on the task. No new permission level.
