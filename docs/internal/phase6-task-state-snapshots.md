# Phase 6: Task State Snapshots — Design Spec

**Date**: 2026-04-08
**Status**: Draft
**Phase**: 6 (Shared Storage & Worker Affinity — first deliverable)

## Problem

Tasks in Stroem have no way to persist data between runs. Each job starts fresh with the workspace snapshot (read-only) and step outputs live only in the database for the duration of the job. Use cases like SSL certificate renewal (Let's Encrypt), incremental processing with cursors, or caching build artifacts require persistent state that survives across job runs.

## Solution

Server-managed immutable state snapshots that let tasks persist both **files** (certs, keys, compiled artifacts) and **structured JSON** (cursors, metadata, counters) between runs.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Data model | Immutable snapshots | Each job reads previous snapshot, writes new one. No concurrent mutation. Clean history. Fits existing revision-based patterns. |
| Storage backend | Dedicated `StateArchive` trait, defaults to log archive backend, optional override | Clear separation of concerns. Reuses existing S3/local infrastructure. |
| Access pattern | `/state` (read-only mount) + `/state-out` (writable dir) + `STATE:` protocol | Files via directory mount, structured data via stdout protocol. Transparent to scripts. |
| Availability | Always available, no opt-in | State dirs created for every step. Upload only if `/state-out` has content or `STATE:` lines emitted. Zero overhead when unused. |
| State resolution | At step claim time | Enables intra-job state propagation. Sequential steps see earlier steps' state. |

## Architecture

### Data Flow

```
┌──────────────────────────────────────────────────────────────────┐
│  Job #100 (task: renew-ssl)                                      │
│                                                                  │
│  Step A claimed → get_latest(ws, task) → snapshot-42             │
│  Worker downloads snapshot-42 → mounts /state                    │
│  Step A completes → /state-out empty → no upload                 │
│                                                                  │
│  Step B claimed → get_latest(ws, task) → snapshot-42             │
│  Worker downloads snapshot-42 → mounts /state                    │
│  Step B writes cert to /state-out, emits STATE: {...}            │
│  Step B completes → upload → creates snapshot-43                 │
│                                                                  │
│  Step C claimed → get_latest(ws, task) → snapshot-43  ← new!    │
│  Worker downloads snapshot-43 → sees step B's cert at /state    │
│  Step C completes → /state-out empty → no upload                 │
│                                                                  │
│  Next job #101: all steps start with snapshot-43                 │
└──────────────────────────────────────────────────────────────────┘
```

### Parallel Steps

When steps B and C run in parallel (both claimed at ~same time):
- Both see the same pre-parallel state snapshot
- Both can write independently; uploads create separate snapshots
- Downstream step D sees the latest snapshot (by `created_at`)

### Storage Architecture

```
StateArchive trait
├── S3StateArchive      — stores in S3 bucket
└── LocalStateArchive   — stores on local filesystem

Storage keys: {prefix}{workspace}/{task_name}/{job_id}.tar.gz
Default prefix: "state/"
Example: state/production/renew-ssl/550e8400-e29b-41d4-a716-446655440000.tar.gz
```

Tarball contents:
```
./state.json          ← structured state from STATE: lines (optional)
./cert.pem            ← file artifacts written to /state-out
./privkey.pem         ← any files the script places in /state-out
```

## Database Schema

### New table: `task_state`

```sql
CREATE TABLE task_state (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace   TEXT NOT NULL,
    task_name   TEXT NOT NULL,
    job_id      UUID REFERENCES job(job_id) ON DELETE SET NULL,
    storage_key TEXT NOT NULL,
    size_bytes  BIGINT NOT NULL DEFAULT 0,
    has_json    BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_task_state_lookup
    ON task_state(workspace, task_name, created_at DESC);

CREATE INDEX idx_task_state_job
    ON task_state(job_id);
```

- `job_id` is `ON DELETE SET NULL` — snapshot survives job retention cleanup
- `has_json` tracks whether `state.json` exists (avoids downloading to check)
- Primary lookup index supports `get_latest()` efficiently

## Server Configuration

```yaml
# server-config.yaml

# Optional — defaults to log_storage archive backend with "state/" prefix
state_storage:
  prefix: "state/"
  max_snapshots: 5          # retention per workspace+task
  # archive:                # optional override
  #   archive_type: s3
  #   bucket: my-state-bucket
  #   region: eu-west-1
```

```rust
// config.rs addition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateStorageConfig {
    #[serde(default = "default_state_prefix")]
    pub prefix: String,                        // default: "state/"
    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: usize,                  // default: 5
    pub archive: Option<ArchiveConfig>,        // override backend
}
```

**Initialization in `AppState::new()`**:
1. If `state_storage.archive` is set → use that backend
2. Else if `log_storage.archive` is set → reuse that backend with `state_storage.prefix`
3. Else → `state_storage` is `None` on `AppState` (feature disabled, no crash, no state)

## `StateArchive` Trait

```rust
/// Trait for persisting task state snapshots across runs.
#[async_trait]
pub trait StateArchive: Send + Sync {
    /// Store a state snapshot.
    async fn store(&self, key: &str, data: &[u8]) -> Result<()>;

    /// Retrieve a state snapshot. Returns None if not found.
    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Delete a state snapshot.
    async fn delete(&self, key: &str) -> Result<()>;
}
```

Implementations: `S3StateArchive`, `LocalStateArchive` — separate from `LogArchive` even if sharing the same underlying bucket/path.

## `STATE:` Protocol

New stdout protocol prefix alongside the existing `OUTPUT:`:

```bash
# Existing — step output (stored in job_step.output, available via Tera)
echo "OUTPUT: {\"cert_path\": \"/etc/ssl/cert.pem\"}"

# New — persistent state (stored in state snapshot, survives across jobs)
echo "STATE: {\"cert_expiry\": \"2026-12-01\", \"days_remaining\": 237}"
```

Parser:
```rust
pub fn parse_state_line(line: &str) -> Option<serde_json::Value> {
    let json_str = line.strip_prefix("STATE: ")
        .or_else(|| line.strip_prefix("STATE:"))?;
    serde_json::from_str(json_str).ok()
}
```

Multiple `STATE:` lines in a step are deep-merged (last write wins for same key). The final merged JSON is written as `state.json` inside the `/state-out` tarball before upload.

## Worker API Endpoints

### Download state

```
GET /worker/state/{workspace}/{task_name}
Authorization: Bearer <worker_token>

Response: 200 application/gzip (tarball bytes)
          204 No Content (no state exists)
Headers: X-Snapshot-Id: <uuid>
```

### Upload state

```
POST /worker/state/{workspace}/{task_name}/{job_id}
Authorization: Bearer <worker_token>
Content-Type: application/gzip
Query: ?has_json=true|false
Body: <tarball bytes>

Response: 201 { "snapshot_id": "<uuid>" }
```

Body size limit: 50 MB (configurable).

The upload handler:
1. Validates the job exists and belongs to this workspace+task
2. Builds `storage_key` from prefix + workspace + task + job_id
3. Stores tarball via `StateArchive::store()`
4. Inserts `task_state` row
5. Prunes old snapshots (keeps `max_snapshots`, deletes both DB rows and archive objects)

### ClaimResponse extension

```rust
// Added to ClaimResponse in worker_api/jobs.rs
pub state_storage_key: Option<String>,  // set if previous state exists
pub state_has_json: Option<bool>,       // whether state.json is present
```

Resolved at claim time via `TaskStateRepo::get_latest()`.

## Runner Mounting

### ShellRunner (local execution)

```
Environment variables:
  STATE_DIR=/tmp/stroem-state-{random}/state        (read-only by convention)
  STATE_OUT_DIR=/tmp/stroem-state-{random}/state-out (writable)

Scripts: cp $STATE_DIR/cert.pem ./cert.pem
```

### DockerRunner

```
Bind mounts:
  {host_state_dir}:/state:ro        ← previous snapshot
  {host_state_out_dir}:/state-out:rw ← new state output

Environment:
  STATE_DIR=/state
  STATE_OUT_DIR=/state-out
```

### KubeRunner

```
Volumes:
  state-vol:     emptyDir  → /state:ro     (main container)
  state-out-vol: emptyDir  → /state-out:rw (main container)

Init container: downloads and extracts previous state into state-vol
  (same pattern as workspace init container — curl + tar + bearer token)
```

**KubeRunner limitation (initial release)**: File-based state upload from `/state-out` requires a post-completion mechanism. For the initial release:
- Structured state via `STATE:` protocol works fully (carried in log stream)
- File-based `/state-out` support deferred for KubeRunner (document this)
- Shell and Docker runners support both file and structured state from day one

## Tera Template Integration

Previous structured state (`state.json`) is injected into the template context at claim time:

```yaml
# Workflow YAML — using state in templates
flow:
  - name: check-cert
    action: check-ssl
    input:
      previous_expiry: "{{ state.cert_expiry }}"
    when: "not state or state.days_remaining < 30"

  - name: renew-cert
    action: renew-ssl
    depends_on: [check-cert]
```

The `state` key in the template context contains the parsed `state.json` from the latest snapshot. If no previous state exists, `state` is absent from the context (use `not state` guard in `when` conditions).

## Repository: `TaskStateRepo`

```rust
pub struct TaskStateRow {
    pub id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub job_id: Option<Uuid>,
    pub storage_key: String,
    pub size_bytes: i64,
    pub has_json: bool,
    pub created_at: DateTime<Utc>,
}

impl TaskStateRepo {
    pub async fn get_latest(pool, workspace, task_name) -> Result<Option<TaskStateRow>>;
    pub async fn get(pool, id) -> Result<Option<TaskStateRow>>;
    pub async fn insert(pool, workspace, task_name, job_id, storage_key, size_bytes, has_json) -> Result<Uuid>;
    pub async fn list(pool, workspace, task_name) -> Result<Vec<TaskStateRow>>;
    pub async fn prune(pool, workspace, task_name, keep) -> Result<Vec<String>>; // returns deleted storage_keys
    pub async fn delete_all(pool, workspace, task_name) -> Result<Vec<String>>;
}
```

## Worker-Side Flow

### Pre-execution (in `poller.rs`)

```
1. Create temp dirs: state/ and state-out/
2. If claim_response.state_storage_key is Some:
   a. GET /worker/state/{ws}/{task}
   b. Extract tarball into state/ dir
   c. On failure: warn and continue with empty state
3. Pass state_dir and state_out_dir to executor
```

### Post-execution (in `poller.rs`)

```
1. Collect STATE: lines from step output → merge into JSON
2. If STATE: JSON exists → write to state-out/state.json
3. If state-out/ has any content:
   a. Build gzip tarball from state-out/
   b. POST /worker/state/{ws}/{task}/{job_id}?has_json={bool}
   c. On failure: warn (non-fatal)
4. Cleanup temp dirs
```

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| First run (no previous state) | `/state` is empty directory. `state` absent from Tera context. |
| Step writes nothing to /state-out | No upload, no new snapshot. |
| Multiple STATE: lines in one step | Deep-merged, last write wins for same key. |
| Multiple steps upload in same job | Each creates a snapshot. Latest by `created_at` wins. |
| Parallel steps | Both see pre-parallel state. Independent uploads. |
| Concurrent jobs for same task | Both read same snapshot. Both write new ones. No conflict. |
| Job fails | Partial state may be uploaded (from completed steps). Next job sees it. |
| State > 50MB upload limit | Upload rejected (413). Non-fatal warning. |
| Archive backend unavailable | State operations fail gracefully with warnings. Job continues. |

## Files to Create

| File | Purpose |
|------|---------|
| `crates/stroem-db/migrations/028_task_state.sql` | Database migration |
| `crates/stroem-db/src/repos/task_state.rs` | Repository |
| `crates/stroem-server/src/state_storage.rs` | `StateArchive` trait + S3/Local impls + `StateStorage` wrapper |
| `crates/stroem-server/src/web/worker_api/state.rs` | Download/upload endpoints |

## Files to Modify

| File | Change |
|------|--------|
| `crates/stroem-db/src/repos/mod.rs` | Add `pub mod task_state` |
| `crates/stroem-db/src/lib.rs` | Re-export `TaskStateRepo`, `TaskStateRow` |
| `crates/stroem-server/src/config.rs` | Add `StateStorageConfig`, `state_storage` field |
| `crates/stroem-server/src/state.rs` | Add `state_storage: Option<Arc<StateStorage>>` to `AppState` |
| `crates/stroem-server/src/web/worker_api/mod.rs` | Register state routes |
| `crates/stroem-server/src/web/worker_api/jobs.rs` | Extend `ClaimResponse`, add state lookup at claim time |
| `crates/stroem-server/src/web/worker_api/rendering.rs` | Extend render context with `state` from state.json |
| `crates/stroem-worker/src/client.rs` | Add state download/upload methods, extend `ClaimedStep` |
| `crates/stroem-worker/src/poller.rs` | State dir management, pre/post-execution handling |
| `crates/stroem-worker/src/executor.rs` | Pass state paths, set env vars |
| `crates/stroem-runner/src/traits.rs` | Add `state_dir`/`state_out_dir` to `RunConfig`, add `parse_state_line()` |
| `crates/stroem-runner/src/shell.rs` | Set STATE_DIR/STATE_OUT_DIR env vars |
| `crates/stroem-runner/src/docker.rs` | Add `/state:ro` and `/state-out:rw` bind mounts |
| `crates/stroem-runner/src/kubernetes.rs` | Add volumes + init container for state download |
| `crates/stroem-server/src/recovery.rs` | Integrate state cleanup with retention |

## Verification

1. **Unit tests**: `TaskStateRepo` CRUD, `StateArchive` impls, `parse_state_line()`, tarball build/extract
2. **Integration tests**: Full round-trip — create job, write STATE:, verify snapshot stored, create next job, verify state available in `/state` and Tera context
3. **Multi-step test**: Sequential flow where step B writes state, step C sees it via `/state`
4. **Runner tests**: Verify `/state` and `/state-out` mounting for shell and docker runners
5. **Retention test**: Verify pruning keeps only `max_snapshots` per task
6. **E2E test**: Add SSL cert renewal scenario to `tests/e2e.sh` — task writes cert on first run, reads it on second run
