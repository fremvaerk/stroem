# File Inputs (`type: file`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A person or a program gives a task a file (UI, REST, MCP inline or upload URL, `stroem-api trigger --file`), and every runner step of the job gets a local read-only path `{root}/{file_id}/{safe_name}` to it, rendered by `{{ input.<field> }}`.

**Architecture:** Uploads stream into the blob archive and become `input_file` rows; job creation attaches them (`job_input_file`) by three rules (A1 own upload, A2 lineage, A3 target lookup). Templates keep the file ID as the stored value and render a path: the ID → path mapping lives only in `render_context::build_entries`, driven by the job's attachment rows (sampled once per entry inside `Snapshots`). The claim tells the worker which files to place; the worker (local, Docker) or a pod init container (Kube) downloads, verifies size + SHA-256 and exposes them read-only.

**Tech Stack:** Rust (axum 0.8, sqlx 0.9 runtime queries, aws-sdk-s3, bollard, kube, reqwest 0.13, sha2 0.11), PostgreSQL, React 19 + TS + shadcn/ui, Vitest, Playwright, Starlight docs.

**Spec:** `docs/superpowers/specs/2026-10-29-file-inputs-design.md` (revision 6, Codex MERGE-READY; revision 7 added by Task 0). Section numbers (§) below refer to it. U1–U9 are closed user decisions.

## Global Constraints

- Upload body limit `input_files.max_file_bytes` = `104857600` (100 MiB); `upload_ttl_secs` = `86400`; `upload_url_ttl_secs` = `900`; `mcp_inline_max_bytes` = `1048576` (decoded), validated `≤ 1468006` (1.4 MiB); `prefix` = `"inputs/"` (§ 10). Env `STROEM__INPUT_FILES__…`.
- File ID = `file_` + UUID v4 simple form (32 lowercase hex). Path = `{root}/{file_id}/{safe_name}`; container root `/inputs`; local root = the worker's `inputs_dir`; non-worker contexts `/inputs` (§ 5.2).
- Safe name (§ 4.2): basename only; every char outside `[A-Za-z0-9._-]` → `_`; leading `.` or `-` → `_`; ≤ 128 bytes keeping the extension; empty / `.` / `..` → 400 `invalid file name`.
- Error texts, verbatim (§ 8): `Input field '{f}': the file cannot be used`; `Input field '{f}': a file is required`; `invalid file name`; `file is larger than {max} bytes` (413); `Input field '{f}': inline content is larger than {cap} bytes`; `file storage is not configured` (503); old worker: `this step has file inputs, which the worker does not support; upgrade the worker`; MCP without base URL: `upload URLs need auth.base_url; send the file inline or use the REST upload`. Never an ID, name, path or rendered value in an error.
- `FileInputError` is a typed leaf; `classify_execute_error` downcasts it to 400 BEFORE the `"is not available"` check.
- Pod: init container `stroem-inputs-init`, volume `stroem-inputs` (emptyDir), main container mount `/inputs` read-only; same curl image and retry policy as `workspace-init` (`--retry 11 --retry-delay 5`); script text is a constant (§ 6.4).
- Worker download: `GET /worker/jobs/{job_id}/input-files/{file_id}`; 503 retried like pinned tarballs: 12 attempts, 5 s apart.
- No path holds a whole file in memory (§ 9). S3 part buffer 8 MiB.
- Rollout order in every doc: every server replica → every worker → YAML with `type: file` (§ 12).
- Every cargo command is prefixed `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`; DB/Docker tests also need `DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true`. Never set `CARGO_TARGET_DIR`. At most 2 cargo processes at a time across all agents. Never run any docker prune / volume / image removal — stop and report. Never start a background or workspace-wide test run inside a task (the controller runs the full suite). Fresh worktree: `mkdir -p crates/stroem-server/static` before building stroem-server (never commit it).
- One integration-test binary per crate: a new test file is a `mod` line in `tests/main.rs`. Never call `testcontainers` directly. sqlx runtime queries only; values via `.bind()`.
- Conventional commits; no AI co-author or "Generated with" trailer. Repo files keep normal English (STE is for chat only).

## Builder choices (spec § 16)

- Modules: `stroem_common::file_field` (pure rules + shared types); `stroem_db::repos::input_file` (`InputFileRepo`, `JobInputFileRepo`, `UploadTokenRepo`); `stroem_server::input_files` (pub: streaming core, acceptance, loaders, `FileGrant`); handlers in `web/api/input_files.rs` and `web/worker_api/input_files.rs`.
- `JobFiles` is carried in `render_context::Snapshots.files`, so it is sampled once per entry (advance / init / claim) like the state snapshots and reaches every `JobContext` without new fields.
- S3 `put_stream`: buffer up to one 8 MiB part; a body that ends inside the first part is one `put_object`; otherwise a multipart upload with 8 MiB parts, aborted on any error.
- `stroem run` copies the file (no hard link) and sets mode `0444`.
- Metrics: `stroem_input_file_uploads_total{origin,result}` (`result` ∈ `ok|too_large|invalid_name|error`), `stroem_input_file_bytes_total{origin}`.
- `/api/uploads/{token}`: the token is consumed atomically at the start of the upload; a failed upload needs a new URL (documented).
- Local roots: `inputs_dir` must be absolute, else the claim is 400.
- Pod init-container failures still surface as the existing exit 1 without text (spec § 6.4 accepts the existing pod-failure path); recorded in TODO.md, not fixed.
- E2E: the compose worker has `capabilities: [script]` and no Docker daemon, so the § 13 E2E "docker step" is replaced by an `#[ignore]` DockerRunner test run locally (Task 16) — spec revision 7 records it.

## Review Focus

1. **Client aborts mid-upload or sends a chunked body over the limit.** Nothing is kept: no row, no blob, no local temp file, no open S3 multipart upload; the response is 413 for the limit. Tests in Task 5 (stream error and over-limit, local and MinIO).
2. **The same upload in two fields of one job** (`a` and `b` both name one ID). Two attachment rows, ONE `input_files` entry in the claim, one path; both fields render it. Test in Task 11.
3. **A step retried on the same worker, or failing before its runner starts.** Each claim attempt gets a fresh `inputs_dir`; the read-only tree (`0444` / `0555`) is removed on every exit path. Test in Task 18.
4. **Hostile or non-ASCII original names on download.** `Content-Disposition` never carries raw `"`, `;`, `\`, control chars; a non-ASCII name downloads under `filename*` UTF-8. Test in Task 6.
5. **An upload from the same user for another workspace / task, reused in a direct execute.** A1 rejects it with the value-free 400 (workspace and task both checked). Test in Task 8.

---

### Task 0: Spec revision 7, DinD result, plan commit

**Files:**
- Modify: `docs/superpowers/specs/2026-10-29-file-inputs-design.md` (status line, revision history, § 6.3, § 13)
- Modify: `docs/internal/TODO.md` (new section "File inputs (2026-10-30)")
- Create: this plan

- [ ] **Step 1:** Add revision 7 (2026-10-30): (1) § 6.3 check done — in the Helm DinD topology the sidecar shares no volume with the worker; `DockerRunner` uses `Binds`, so a missing source path is created empty on the daemon side and a `runner: docker` step sees an empty `/inputs` (also `/workspace`, `/state`, `/artifacts`); reproduced locally with `docker:27-dind` + a client container sharing only the network namespace; chart fix out of scope, guide documents it; (2) § 13 E2E docker step replaced (compose worker `capabilities: [script]`, no daemon) by an `#[ignore]` DockerRunner test. Status line → `approved (Codex MERGE-READY on revision 6), revision 7`.
- [ ] **Step 2:** In `TODO.md` add a "File inputs (2026-10-30)" section with: Helm DinD bind mounts empty (all `runner: docker` mounts, result above, fix = shared volume at the same path in worker and sidecar); pod `/state` and `/global-state` always empty although `guides/task-state.md:124-127` says otherwise (U7); pod init-container failures show no reason text (`classify_pod_status` reads only `step`); every § 15 non-goal as an unchecked item.
- [ ] **Step 3:** Commit: `docs(spec): file inputs revision 7 and implementation plan`.

---

### Task 1: `stroem_common::file_field` and the `file` render rule

**Files:**
- Create: `crates/stroem-common/src/file_field.rs` (unit tests in sibling `crates/stroem-common/src/file_field/tests.rs`)
- Modify: `crates/stroem-common/src/lib.rs` (`pub mod file_field;`), `crates/stroem-common/src/template.rs:711-725` (constants), `:597` (`render_input_typed`)
- Modify: `crates/stroem-common/src/models/workflow.rs:88-91` (doc comment lists `file`)

**Interfaces — Produces:**
```rust
pub const FILE_TYPE: &str = "file";
pub const CANONICAL_INPUTS_ROOT: &str = "/inputs";
pub const MAX_SAFE_NAME_BYTES: usize = 128;
pub fn safe_name(original: &str) -> Option<String>;
pub fn new_file_id() -> String;                 // "file_" + Uuid::new_v4().simple()
pub fn is_file_id(s: &str) -> bool;             // "file_" + exactly 32 [0-9a-f]
pub fn file_path(root: &str, file_id: &str, safe_name: &str) -> String;
#[derive(Debug, Clone, PartialEq, Eq)] pub struct FileRef { pub file_id: String, pub safe_name: String }
#[derive(Debug, Clone)] pub struct JobFiles { /* root, fields: BTreeMap<field, file_id>, files: BTreeMap<file_id, FileRef> */ }
impl Default for JobFiles  // == JobFiles::empty() (root CANONICAL_INPUTS_ROOT)
impl JobFiles {
    pub fn new(root: impl Into<String>, attachments: impl IntoIterator<Item = (String, FileRef)>) -> Self;
    pub fn empty() -> Self;
    pub fn root(&self) -> &str;
    pub fn with_root(&self, root: &str) -> Self;
    pub fn is_empty(&self) -> bool;
    pub fn files(&self) -> impl Iterator<Item = &FileRef>;        // distinct by id, id order
    pub fn field_file(&self, field: &str) -> Option<&FileRef>;   // task-level attachment
    pub fn path_of(&self, file_id: &str) -> Option<String>;
    pub fn template_input(&self, input: &Value) -> Value;         // job input: field holding its attached id → path
    pub fn template_prepared(&self, prepared: &Value, schema: &HashMap<String, InputFieldDef>) -> Value; // `file` fields holding an attached id → path
    pub fn resolve_target(&self, value: &str) -> Option<&FileRef>; // A3: attached id, or exactly `{root}/{id}/{its safe_name}`
}
#[derive(Debug, Clone, PartialEq, Eq)] pub enum FileInputErrorKind { CannotUse, Required, InlineTooLarge { cap: u64 } }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct FileInputError { pub field: String, pub kind: FileInputErrorKind }
impl Display (§ 8 texts) + std::error::Error (no source)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedInputFile { pub file_id: String, pub safe_name: String, pub size_bytes: u64, pub sha256: String }
// template.rs
pub fn render_input_typed_with_files(input_map: &HashMap<String, Value>, schema: Option<&HashMap<String, InputFieldDef>>, context: &Value, files: &JobFiles) -> Result<Value>;
// render_input_typed(...) keeps its signature and calls the new fn with &JobFiles::empty()
```
`PRIMITIVE_TYPES` and `RESERVED_TYPE_NAMES` gain `FILE_TYPE`.

- [ ] **Step 1: Write the failing tests** in `file_field/tests.rs`:
  - `safe_name_table`: `"report.csv"→"report.csv"`, `"x;rm -rf ~.csv"→"x_rm_-rf__.csv"`, `"../../etc/passwd"→"passwd"`, `"C:\\Users\\me\\x.txt"→"x.txt"`, `"-rf"→"_rf"`, `".bashrc"→"_bashrc"`, `"a b.csv"→"a_b.csv"`, `"rapport-é.csv"→"rapport-_.csv"`, `"$(id).sh"→"__id_.sh"`, `"line\nbreak"→"line_break"`, `"" / "." / ".." / "dir/"→None`, `"a".repeat(200)+".csv"` → 124 `a` + `".csv"` (len 128), `"b".repeat(300)` → 128 `b`.
  - `file_id_shape`: `is_file_id(&new_file_id())`; rejects uppercase hex, 31/33 hex, missing prefix, `"file_"+"g"*32`.
  - `template_input_maps_only_attached_fields`: input `{"a":"file_X","b":"file_X","c":"file_X"}` with attachments `a→X`, `b→X` (root `/r`) → `a`,`b` = `/r/file_X/r.csv`, `c` unchanged.
  - `template_prepared_maps_file_fields_only`: schema `d: file, s: string`, both holding attached id → `d` path, `s` unchanged; unattached id in `d` unchanged.
  - `resolve_target_cases`: attached id → Some; `{root}/{id}/{safe}` → Some; same with another root → None; another job's id → None; `{root}/{id}/other.csv` → None; `{root}/{id}/{safe}/x` → None; empty → None.
  - `collision_case_rev4`: two attachments `a→X (report.csv)`, `b→Y (report.csv)`: `path_of(X) != path_of(Y)` and `files().count()==2`.
  - `file_input_error_texts`: the three Display strings exactly as in Global Constraints; `source().is_none()`.
  - `connection_resolution_leaves_file_fields_alone` (`resolve_connection_inputs` with a `file` field holding an id → unchanged; § 11 "confirm with tests").
  - In `template.rs` tests: `render_file_target_accepts_id_and_path` (schema `data: file`, step input `"{{ input.f }}"` where context `input.f` is the path → output `data` = id), `render_file_target_rejects_unknown` (→ `FileInputError{field:"data",CannotUse}` via `downcast_ref`), `render_file_target_empty_omits_field` (`""` and `null` → key absent), `render_input_typed_without_files_rejects_file_value`.

- [ ] **Step 2: Run** `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-common file_field` → FAIL (module missing).
- [ ] **Step 3: Implement** `file_field.rs` and the template change. `render_input_typed_with_files` partitions like the `json` branch: a `file`-typed field is rendered with `render_template` when it is a string (non-string → `CannotUse`), trimmed; empty → omitted; else `files.resolve_target` → `Value::String(file_id)` or `Err(anyhow::Error::new(FileInputError{..}))`. Truncation in `safe_name` keeps an extension of ≤ 32 bytes (`rfind('.')` > 0), else cuts at 128.
- [ ] **Step 4: Run** `cargo test -p stroem-common` → PASS (all, including existing template tests).
- [ ] **Step 5: Commit** `feat(common): file input type, safe names and file-target rendering`.

---

### Task 2: Validation rules and agent tool schema (§ 4.1, § 7.6)

**Files:**
- Modify: `crates/stroem-common/src/validation.rs` (`check_input_field_options` :1033, `validate_approval_action` :2014, trigger checks :466-618, `validate_agent_action` tools loop :1983); tests in its existing `tests.rs` sibling (or create `validation/tests.rs` if the module is still inline — CLAUDE.md § CodeQL)
- Modify: `crates/stroem-agent/src/tools.rs:61-104,137` (skip `file` fields)

**Interfaces — Consumes:** `FILE_TYPE` (Task 1).

- [ ] **Step 1: Write the failing tests:**
  - `file_field_rejects_default`, `_options`, `_multiple`, `_secret` — each an `Err` whose text names the field and the attribute.
  - `connection_type_named_file_is_rejected` (RESERVED); `connection_property_of_type_file_is_rejected`.
  - `approval_action_with_file_input_is_rejected`.
  - `required_file_task_targeted_by_scheduler_warns`, `…by_webhook_warns`, `…by_event_source_warns` (warning text contains the trigger name and `cannot supply a file`); optional file field → no warning.
  - `agent_tool_task_with_required_file_is_error`; optional → ok.
  - stroem-agent: `input_schema_skips_file_fields` (optional `file` field absent from `properties` and `required`).
- [ ] **Step 2: Run** `cargo test -p stroem-common validation` and `cargo test -p stroem-agent tools` → FAIL.
- [ ] **Step 3: Implement.** Agent tools resolve the tool task in the same config only (dotted names are skipped, as today).
- [ ] **Step 4: Run** both → PASS.
- [ ] **Step 5: Commit** `feat(common): validate file input fields`.

---

### Task 3: Migration 054 and repos

**Files:**
- Create: `crates/stroem-db/migrations/054_input_files.sql`
- Create: `crates/stroem-db/src/repos/input_file.rs`; Modify: `repos/mod.rs`, `src/lib.rs` (`pub use repos::input_file::{…}`)
- Modify: `crates/stroem-db/src/repos/job.rs:1535` (`JobRepo::delete`)
- Create: `crates/stroem-db/tests/input_file_test.rs`; Modify: `tests/main.rs`, `tests/migration_test.rs` (`test_054_input_files_schema`)

**Schema** (§ 4.3; house style of `038_job_artifact.sql`):
```sql
CREATE TABLE input_file (
  file_id TEXT PRIMARY KEY CHECK (file_id ~ '^file_[0-9a-f]{32}$'),
  workspace TEXT NOT NULL, task_name TEXT NOT NULL,
  uploaded_by_user_id UUID REFERENCES "user"(user_id) ON DELETE SET NULL,
  origin TEXT NOT NULL CHECK (origin IN ('api','mcp_inline','mcp_url')),
  original_name TEXT NOT NULL, safe_name TEXT NOT NULL, content_type TEXT NOT NULL,
  size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0), sha256 TEXT NOT NULL, storage_key TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), deleting_at TIMESTAMPTZ);
CREATE INDEX ix_input_file_created_at ON input_file(created_at);
CREATE TABLE job_input_file (
  job_id UUID NOT NULL REFERENCES job(job_id) ON DELETE RESTRICT, field TEXT NOT NULL,
  file_id TEXT NOT NULL REFERENCES input_file(file_id) ON DELETE RESTRICT, PRIMARY KEY (job_id, field));
CREATE INDEX ix_job_input_file_file_id ON job_input_file(file_id);
CREATE TABLE input_file_upload_token (
  token_sha256 TEXT PRIMARY KEY, user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
  workspace TEXT NOT NULL, task_name TEXT NOT NULL, original_name TEXT NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL, used_at TIMESTAMPTZ);
```

**Interfaces — Produces** (`stroem_db::{…}`):
```rust
pub struct NewInputFile<'a> { pub file_id: &'a str, pub workspace: &'a str, pub task_name: &'a str, pub uploaded_by_user_id: Option<Uuid>, pub origin: &'a str, pub original_name: &'a str, pub safe_name: &'a str, pub content_type: &'a str, pub size_bytes: i64, pub sha256: &'a str, pub storage_key: &'a str }
#[derive(Debug, Clone, sqlx::FromRow)] pub struct InputFileRow { file_id, workspace, task_name, uploaded_by_user_id: Option<Uuid>, origin, original_name, safe_name, content_type, size_bytes: i64, sha256, storage_key, created_at, deleting_at: Option<DateTime<Utc>> }
pub struct InputFileRepo;
impl InputFileRepo {
  pub async fn insert(pool: &PgPool, f: &NewInputFile<'_>) -> Result<()>;
  pub async fn get(pool: &PgPool, file_id: &str) -> Result<Option<InputFileRow>>;
  pub async fn sweep_candidates(pool: &PgPool, ttl_secs: i64, limit: i64) -> Result<Vec<String>>; // deleting_at set, OR (no attachment AND created_at < NOW()-ttl)
  pub async fn mark_deleting(pool: &PgPool, file_id: &str) -> Result<Option<InputFileRow>>;     // § 4.7 step 1
  pub async fn delete(pool: &PgPool, file_id: &str) -> Result<bool>;                             // only deleting_at set, no attachment
}
pub enum AttachRule<'a> { Own { workspace: &'a str, task_name: &'a str, user_id: Option<Uuid>, ttl_secs: i64 }, Lineage { job_id: Uuid } }
#[derive(Debug, Clone, sqlx::FromRow)] pub struct JobInputFileRow { pub job_id: Uuid, pub field: String, pub file_id: String, pub original_name: String, pub safe_name: String, pub content_type: String, pub size_bytes: i64, pub sha256: String, pub storage_key: String }
pub struct JobInputFileRepo;
impl JobInputFileRepo {
  /// `FOR SHARE` on the input_file row, then: deleting_at IS NULL AND (any rule matches); inserts the attachment. Ok(false) = no rule matched.
  pub async fn attach_tx(tx: &mut sqlx::PgConnection, job_id: Uuid, field: &str, file_id: &str, rules: &[AttachRule<'_>]) -> Result<bool>;
  pub async fn list_for_job(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobInputFileRow>>;       // ORDER BY field
  pub async fn get_for_field(pool: &PgPool, job_id: Uuid, field: &str) -> Result<Option<JobInputFileRow>>;
  pub async fn get_attached(pool: &PgPool, job_id: Uuid, file_id: &str) -> Result<Option<JobInputFileRow>>;
}
#[derive(Debug, Clone, sqlx::FromRow)] pub struct UploadTokenRow { pub user_id: Uuid, pub workspace: String, pub task_name: String, pub original_name: String }
pub struct UploadTokenRepo;
impl UploadTokenRepo {
  pub async fn insert(pool: &PgPool, token_sha256: &str, user_id: Uuid, workspace: &str, task_name: &str, original_name: &str, expires_at: DateTime<Utc>) -> Result<()>;
  pub async fn consume(pool: &PgPool, token_sha256: &str) -> Result<Option<UploadTokenRow>>; // UPDATE … SET used_at=NOW() WHERE used_at IS NULL AND expires_at > NOW() RETURNING
  pub async fn delete_expired(pool: &PgPool) -> Result<u64>;
}
```
`A1` (Own) SQL predicate: `workspace = $ AND task_name = $ AND uploaded_by_user_id IS NOT DISTINCT FROM $ AND created_at > NOW() - make_interval(secs => $)`. `Lineage`: `EXISTS (SELECT 1 FROM job_input_file WHERE job_id = $ AND file_id = $)`.
`JobRepo::delete` becomes one transaction: `DELETE FROM job_input_file WHERE job_id=$1`, then the job delete.

- [ ] **Step 1: Write the failing tests** (`input_file_test.rs`, all `#[tokio::test(flavor = "multi_thread")]` with `crate::common::setup_db`):
  - `test_054_input_files_schema` (migration_test): the three tables, both indexes, the `file_id` CHECK rejects `"x"`.
  - `attach_own_upload_matches_user_task_ttl` (match; other user → false; other task → false; other workspace → false; NULL owner matches `user_id: None` only; created 2 days ago with ttl 86400 → false).
  - `attach_lineage_requires_attachment_on_source`.
  - `attach_refuses_deleting_file`.
  - `list_for_job_joins_file_metadata`.
  - `upload_token_single_use_and_expiry` (second consume → None; expired → None; `delete_expired` removes it).
  - `job_delete_removes_attachments_not_files`.
  - `sweep_vs_creation_creation_first`: tx A `attach_tx` (uncommitted) holds `FOR SHARE`; spawn `mark_deleting` → it blocks; commit A → `mark_deleting` returns `None`, file kept.
  - `sweep_vs_creation_sweep_first`: tx S holds the `FOR UPDATE` (call the repo's two statements in a test-held tx through a `#[doc(hidden)] pub async fn mark_deleting_tx(tx, file_id)`), spawn `attach_tx` in another tx → it blocks; commit S → `attach_tx` returns `false`.
  - `sweep_candidates_skips_attached_and_young`.
- [ ] **Step 2: Run** `DOCKER_HOST=… TESTCONTAINERS_RYUK_DISABLED=true CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo test -p stroem-db --test integration input_file_test:: migration_test::test_054` → FAIL.
- [ ] **Step 3: Implement** migration + repos (`mark_deleting` = `mark_deleting_tx` in a pool tx: `SELECT … FOR UPDATE`, then a SEPARATE `SELECT EXISTS(attachment)`, then `UPDATE … SET deleting_at = COALESCE(deleting_at, NOW())`).
- [ ] **Step 4: Run** the same command, then all of `cargo test -p stroem-db --test integration` → PASS.
- [ ] **Step 5: Commit** `feat(db): input file tables and repositories (migration 054)`.

---

### Task 4: `input_files` config, `AppState`, archive wiring

**Files:**
- Modify: `crates/stroem-server/src/config.rs` (struct, `ServerConfig.input_files`, `validate`, tests), `src/state.rs`, `src/main.rs:276-313`
- Modify (scripted): every `ServerConfig { … }` literal (25 files)

**Interfaces — Produces:**
```rust
#[derive(Debug, Clone, Deserialize, Serialize)] #[serde(deny_unknown_fields)]
pub struct InputFilesConfig { #[serde(default = …)] pub max_file_bytes: u64, pub upload_ttl_secs: u64, pub upload_url_ttl_secs: u64, pub mcp_inline_max_bytes: u64, pub prefix: String, pub archive: Option<ArchiveConfig> }  // Default = § 10 values
pub const MAX_MCP_INLINE_BYTES: u64 = 1_468_006;
// ServerConfig: #[serde(default)] pub input_files: Option<InputFilesConfig>
// AppState
pub input_blob: Option<Arc<dyn BlobArchive>>;
pub input_files_config: InputFilesConfig;               // config.input_files.unwrap_or_default()
pub fn with_input_blob(self, archive: Option<Arc<dyn BlobArchive>>) -> Self;
pub fn input_file_storage_key(&self, workspace: &str, file_id: &str) -> String; // "{prefix}{ws}/{file_id}"
```

- [ ] **Step 1: Scripted literal update.** Add `input_files: None,` on the line after every `artifact_storage:` field inside a `ServerConfig` literal (`grep -rln --include='*.rs' 'artifact_storage:' crates`); verify with `cargo check -p stroem-server --all-targets` (+ `stroem-e2e`) that only real `ServerConfig` literals changed (an `artifact_storage:` inside another struct would not compile — revert those lines).
- [ ] **Step 2: Write the failing tests** in `config.rs` tests: `input_files_defaults` (all § 10 values), `input_files_env_override` (`STROEM__INPUT_FILES__MAX_FILE_BYTES=10`), `validate_rejects_zero_caps` (each of the four numbers = 0 → error naming the key), `validate_rejects_mcp_inline_over_cap` (1_468_007 → error), `input_storage_key_shape` (`inputs/ws/file_…`).
- [ ] **Step 3: Run** `cargo test -p stroem-server --lib config::` → FAIL.
- [ ] **Step 4: Implement.** `main.rs`: `input_blob` = `input_files.archive` → `build_blob_archive`, else `artifact_blob.clone()`, else `blob_archive.clone()`; `None` → one `tracing::warn!("input_files: no archive configured; file uploads answer 503")`.
- [ ] **Step 5: Run** `cargo test -p stroem-server --lib config::` → PASS; `cargo check --workspace --all-targets` → OK.
- [ ] **Step 6: Commit** `feat(server): input_files configuration and archive wiring`.

---

### Task 5: Real streaming in the blob archives; upload/download core

**Files:**
- Modify: `crates/stroem-server/src/blob_storage.rs` (Local + S3 `put_stream` / `get_stream` overrides)
- Create: `crates/stroem-server/src/input_files/mod.rs`, `input_files/stream.rs`; Modify `src/lib.rs` (`pub mod input_files;`)
- Modify: `crates/stroem-server/tests/log_peak_alloc_test.rs` (new cases); tests for S3 in `tests/s3_integration_test.rs`

**Interfaces — Produces** (`stroem_server::input_files::stream`):
```rust
pub const S3_PART_BYTES: usize = 8 * 1024 * 1024;
#[derive(Debug)] pub enum PutError { TooLarge { max: u64 }, Body(anyhow::Error), Storage(anyhow::Error) }
#[derive(Debug, Clone)] pub struct Stored { pub size_bytes: u64, pub sha256: String }  // lowercase hex
/// Counts and hashes `body` while `archive.put_stream` consumes it; over `max_bytes` → the stream errors,
/// the backend discards the partial object, Err(TooLarge). Never buffers more than one backend part.
pub async fn put_counted(archive: &dyn BlobArchive, key: &str, content_type: &str, body: BoxStream<'static, anyhow::Result<Bytes>>, max_bytes: u64) -> Result<Stored, PutError>;
/// Response body that streams the blob; `None` when the blob is missing.
pub async fn download_body(archive: &dyn BlobArchive, key: &str) -> anyhow::Result<Option<axum::body::Body>>;
```
Local `put_stream`: temp file in the target dir, write chunk by chunk, `sync_all`, rename (data then `.ct`), temp removed on any error. Local `get_stream`: `tokio::fs::File` + `ReaderStream` (64 KiB). S3 `put_stream`: per Builder choices; `abort_multipart_upload` on error. S3 `get_stream`: `get_object` body chunks without `collect()`.

- [ ] **Step 1: Write the failing tests:**
  - `blob_storage` unit: `local_put_stream_roundtrip_and_no_temp_left`, `local_put_stream_error_leaves_nothing` (stream yields 2 chunks then `Err` → key absent, dir has no `.tmp.`), `local_get_stream_yields_multiple_chunks` (1 MiB file → ≥ 2 chunks).
  - `input_files::stream` unit: `put_counted_reports_size_and_sha256` (known vector `"abc"` → `ba7816bf…`), `put_counted_over_limit_is_too_large_and_keeps_nothing` (no Content-Length, 3 × 400 KiB with max 1 MiB).
  - `s3_integration_test.rs` (MinIO): `s3_put_stream_small_single_put`, `s3_put_stream_multipart_roundtrip` (20 MiB), `s3_put_stream_error_aborts_multipart` (after 2 parts → `Err`; `list_multipart_uploads` empty, key absent).
  - `log_peak_alloc_test.rs`: cases `input upload, local` (`put_counted` of a 50 MiB disk-backed `ReaderStream`, bound formula `2 * 64 KiB + 1 MiB`), `input download, local` (drain `download_body` frame by frame, same bound), and under `#[cfg(feature = "s3")]` `input upload, s3` / `input download, s3` (bound `2 * S3_PART_BYTES + 2 MiB`). The file's checker applies its ×1.5.
- [ ] **Step 2: Run** `cargo test -p stroem-server --lib blob_storage:: input_files::` and `cargo test -p stroem-server --test log_peak_alloc_test` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the unit tests, `cargo test -p stroem-server --test integration s3_integration_test::`, `cargo test -p stroem-server --test log_peak_alloc_test` → PASS (paste the peak table into the commit body).
- [ ] **Step 5: Commit** `feat(server): stream blob uploads and downloads`.

---

### Task 6: Upload route (live task) and job input-file download

**Files:**
- Create: `crates/stroem-server/src/web/api/input_files.rs`; Modify `web/api/mod.rs` (routes in `protected`)
- Create: `crates/stroem-server/src/input_files/upload.rs`
- Modify: `crates/stroem-server/src/web/api/artifacts.rs:57-110` (make `SAFE_INLINE`, `disposition_for`, `sanitize_filename`, `url_encode` `pub(crate)`)
- Modify: `crates/stroem-server/src/metrics.rs` (+ names test)
- Create: `crates/stroem-server/tests/input_file_upload_test.rs`; Modify `tests/main.rs`

**Routes:** `POST /api/workspaces/{ws}/tasks/{name}/files?name=` (layer `DefaultBodyLimit::max(max_file_bytes)`), `GET /api/jobs/{id}/input-files/{field}`.

**Interfaces — Produces** (`stroem_server::input_files::upload`):
```rust
pub struct UploadTarget<'a> { pub workspace: &'a str, pub task_name: &'a str, pub original_name: &'a str, pub content_type: &'a str, pub user_id: Option<Uuid>, pub origin: &'static str }
#[derive(Debug, Serialize)] pub struct UploadResponse { pub file_id: String, pub name: String, pub size_bytes: u64, pub sha256: String, pub content_type: String, pub expires_at: DateTime<Utc> }
pub enum UploadError { InvalidName, TooLarge { max: u64 }, NotConfigured, Internal(anyhow::Error) }  // → 400 / 413 / 503 / 500 with § 8 texts
pub async fn store_upload(state: &AppState, t: UploadTarget<'_>, body: BoxStream<'static, anyhow::Result<Bytes>>, content_length: Option<u64>) -> Result<UploadResponse, UploadError>;
pub fn task_has_file_field(task: &TaskDef) -> bool;
```
`store_upload`: safe name → archive present → `Content-Length > max` → 413 early → `new_file_id` → `put_counted` → `InputFileRepo::insert` (on failure delete the blob) → metrics → `expires_at = created_at + upload_ttl_secs`. Handler without `source_job_id`: auth (auth on + no user → 401), workspace + LIVE task (404), `tasks::require_task_run` (Deny 404, View 403), no `file` field → 400 `task has no file input`, then `store_upload` with `user_id = auth_user.user_id()` (None when auth off), `origin = "api"`, `Content-Type` header or `application/octet-stream`. Body = `axum::body::Body::into_data_stream()` mapped to `anyhow`. Download handler mirrors `download_artifact`: `check_job_acl` Deny → 404 `Job not found`; no attachment → 404; `Content-Type` stored, `nosniff`, `disposition_for(content_type, original_name)`, `Content-Length` = `size_bytes`, `Cache-Control: private, max-age=0, must-revalidate`.

- [ ] **Step 1: Write the failing tests** (`input_file_upload_test.rs`; build `AppState` like `artifact_upload_test.rs:55-125` with `.with_input_blob(Some(LocalBlobArchive))`, workspace with task `files` (`data_file: {type: file}`) and `plain` (no file field); ACL fixtures as in `state_upload_test.rs`):
  - `upload_returns_201_with_metadata` (body `"hello"` → `size_bytes 5`, sha of `hello`, `name` = original, `file_id` matches `^file_[0-9a-f]{32}$`, row exists with `origin = 'api'`, blob at `inputs/default/{id}`).
  - `upload_acl_run_view_deny` (Run 201, View 403, Deny 404), `upload_auth_off_records_null_owner`.
  - `upload_task_without_file_field_is_400`, `upload_unknown_task_is_404`, `upload_invalid_name_is_400` (`name=..`), `upload_missing_name_is_400`.
  - `upload_over_limit_is_413_and_keeps_nothing` (config `max_file_bytes: 1024`, 2 KiB chunked body without Content-Length → 413 body `file is larger than 1024 bytes`, zero rows, archive dir empty), `upload_without_archive_is_503`.
  - `download_input_file_acl_and_headers` (attach via `JobInputFileRepo::attach_tx` to a created job; Run/View → 200 body + headers `x-content-type-options: nosniff`, `content-length`; Deny → 404; unknown field → 404; `text/html` → `attachment`; original name `rapport "é";.csv` → disposition has no raw `"`/`;` and has `filename*=UTF-8''`).
  - `metrics_test.rs`: `input_file_upload_metrics_recorded` (local recorder; one ok upload of 5 bytes → uploads_total{origin="api",result="ok"} 1 and bytes_total 5).
- [ ] **Step 2: Run** `cargo test -p stroem-server --test integration input_file_upload_test:: metrics_test::input_file` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same → PASS; then `cargo test -p stroem-server --test integration artifact_` (helpers made `pub(crate)` unchanged) → PASS.
- [ ] **Step 5: Commit** `feat(server): upload and download input files`.

---

### Task 7: Upload for a Re-run (`source_job_id`, § 4.2 order)

**Files:** Modify `web/api/input_files.rs`; reuse `tasks::{source_job_not_found, check_rerun_source, require_task_run}` (make `pub(crate)` where needed), `jobs::check_job_acl`, `pinned_source::resolve_source_pin`. Test: `tests/input_file_upload_test.rs`.

Order, exactly (§ 4.2): (1) load source row; (2) missing OR `check_job_acl == Deny` → identical 404 `Source job not found`, nothing before it; (3) pinned (`git_ref` set) AND View → 403 `View-only access`; (4) `check_rerun_source(&source, ws)` + `source.task_name == {task}` → 400; (5) pinned → `resolve_source_pin` + `.task(name)`, unpinned → live task (404 `Task not found`); (6) `require_task_run` on the selected task's folder, then the task must declare a `file` field (400). Row records `(ws, task)`.

- [ ] **Step 1: Write the failing tests:** `upload_for_rerun_unpinned_ok`; `upload_for_rerun_denied_source_matches_missing` (for a denied source: other `{ws}`, other `{task}`, a `type: task` child source, a source without `raw_input` — each response status AND body equal to the missing-source response); `upload_for_rerun_pinned_task_only_at_ref` (PinnedFixture: task + file field exist only at the ref → 201); `upload_for_rerun_pinned_view_only_is_403_even_when_ref_broken` (`PinnedFixture::break_remote` → still 403, not 400); `upload_for_rerun_unpinned_view_is_403` (step 6 mapping); `upload_for_rerun_task_mismatch_is_400`.
- [ ] **Step 2: Run** `cargo test -p stroem-server --test integration input_file_upload_test::upload_for_rerun` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(server): replacement uploads for a re-run`.

---

### Task 8: Accepting files at job creation (A1, A2, required)

**Files:**
- Modify: `crates/stroem-server/src/input_files/mod.rs`, `src/job_creator.rs` (inner + public entries), `src/web/api/mod.rs:424` (`classify_execute_error`)
- Modify call sites: `web/api/tasks.rs` (execute, rerun, pinned rerun), `web/api/jobs.rs` (restart), `mcp/tools.rs` (execute_task), `settlement/retry.rs`, `settlement/hooks.rs:699`, `settlement/dispatch.rs:933`, `trigger_target.rs:190`, `event_source.rs:664`, `web/worker_api/event_source.rs:95`, `web/worker_api/jobs.rs:1621`, and every test caller (`grep -rn 'create_job_for_task_detailed\|create_job_for_rerun\|create_restart_job\|create_job_for_task_pinned\|create_job_for_task_inner' crates/stroem-server`)
- Create: `crates/stroem-server/tests/input_file_creation_test.rs`

**Interfaces — Produces** (`stroem_server::input_files`):
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileGrant {
    /// No A1. A2 only through the creation mode's lineage (Rerun/Restart → source_job_id, Retry → failed_job_id).
    Indirect,
    /// A creation the caller starts directly (execute, MCP, Re-run, Restart): A1 for this caller's uploads.
    Direct { user_id: Option<Uuid>, ttl_secs: i64 },
    /// A `type: task` child: A2 against `parent_job_id`.
    TaskChild,
}
impl FileGrant { pub fn direct(state: &AppState, user_id: Option<Uuid>) -> Self; }
/// Shape + required check of every task-level `file` field (before merge_defaults). Ok = (field, file_id) pairs.
pub fn task_file_values(task: &TaskDef, input: &Value) -> Result<Vec<(String, String)>, FileInputError>;
/// In the creation transaction, after the job row: attach each pair or fail with FileInputError::CannotUse.
pub async fn attach_all_tx(tx: &mut PgConnection, job_id: Uuid, workspace: &str, task_name: &str, values: &[(String, String)], grant: FileGrant, lineage: Option<Uuid>) -> anyhow::Result<()>;
```
`create_job_for_task_inner` gains a trailing `files: FileGrant`; `create_job_for_task_detailed`, `create_job_for_rerun`, `create_restart_job`, `create_job_for_task_pinned` gain `files: FileGrant` as their last parameter; `create_child_job_for_task_detailed` passes `Indirect`. Grants: execute / rerun / pinned rerun / MCP / restart → `FileGrant::direct(&state, user_id)`; retry, hooks, triggers, webhook, event source, agent tool → `Indirect`; dispatch child → `TaskChild` (Task 12 wires the S6 side). Lineage: `Rerun|Restart → source_job_id`, `Retry → failed_job_id`, `TaskChild → parent_job_id`, else none. A1 is tried only for `Direct`; A2 when lineage is set (rules list `[Lineage, Own]` as applicable). Required: a `required` `file` field absent, `null` or `""` → `Required`, in every mode. A non-string value or a string that is not a file ID → `CannotUse`. `classify_execute_error`: first `if let Some(f) = e.downcast_ref::<FileInputError>() { return AppError::BadRequest(f.to_string()); }`. MCP `execute_task`: `FileInputError` → `ErrorData::invalid_params(f.to_string(), None)`.

- [ ] **Step 1: Write the failing tests** (`input_file_creation_test.rs`; helper `upload(state, ws, task, user, bytes) -> file_id` via `store_upload`):
  - `execute_accepts_own_upload_and_attaches` (row in `job_input_file`, `job.input.data_file` = id, `raw_input` = id); `execute_same_upload_twice_within_ttl`.
  - `execute_rejects_unknown_other_user_other_task_other_workspace_expired` — each 400 body exactly `Input field 'data_file': the file cannot be used` (expired: `UPDATE input_file SET created_at = NOW() - interval '2 days'`).
  - `execute_rejects_deleting_file`, `execute_rejects_non_string_value`.
  - `required_file_missing_fails_in_every_mode`: execute (400, outermost text contains `required`), MCP execute (invalid params), Re-run without a value, Restart of a source whose field was absent, `type: task` child, event-source emit (dropped), `type: task` hook (logged) — each `FileInputError::Required` (scheduler / webhook are Task 9).
  - `masked_sentinel_in_file_field_is_not_replayed` (Re-run with `"••••••"` in `data_file` → 400 CannotUse — `resolve_rerun_sentinels` leaves primitives alone).
  - `rerun_reuses_source_file_a2`, `restart_replays_source_file_a2`.
  - `retry_chain_after_ttl_uses_failed_job` (job 1 fails → retry 2 created (A2 vs 1); expire the upload; retry 2 fails → retry 3 accepted (A2 vs 2, not root 1)).
  - `trigger_and_hook_creation_reject_valid_upload` (`create_target_job` and a `type: task` hook naming a valid own upload → creation error is `FileInputError::CannotUse`; required file + scheduler → `Required`).
  - `event_source_emit_with_file_value_is_dropped` (worker emit route → job not created, error logged).
  - `classify_file_input_error_is_400_before_is_not_available` (unit in `web/api/mod.rs` tests: `anyhow!(FileInputError…).context("x is not available")` → `BadRequest`).
  - MCP (`mcp_test.rs`): `mcp_execute_with_unknown_file_id_is_invalid_params`.
- [ ] **Step 2: Run** `cargo test -p stroem-server --test integration input_file_creation_test::` → FAIL (compile errors first are expected).
- [ ] **Step 3: Implement**; update every caller (tests pass `FileGrant::Indirect` unless the test is about files).
- [ ] **Step 4: Run** the new tests → PASS; then the WHOLE `cargo test -p stroem-server --test integration` and `--lib` (signature change) → PASS.
- [ ] **Step 5: Commit** `feat(server): attach input files at job creation`.

---

### Task 9: Trigger, webhook and action-hook preflight (§ 4.5, § 4.6)

**Files:** Modify `src/input_files/mod.rs`, `src/scheduler.rs:418-435`, `src/web/hooks.rs:107-148`, `src/settlement/hooks.rs` (action-hook branch before `create_with_parent_tx` :764). Tests: `tests/input_file_creation_test.rs`, scheduler tests via `scheduler::fire_trigger_once`.

**Interfaces — Produces:**
```rust
/// Fails when `schema` has a required `file` field, or `input` gives a non-null value to any `file` field.
pub fn preflight(schema: &HashMap<String, InputFieldDef>, input: &Value) -> Result<(), FileInputError>;
```
Scheduler: right after `resolve_trigger_target` succeeds, `preflight(target task's input, trigger input)`; failure → the existing `Trigger '…' MISSED: …` branch, before the concurrency policy. Webhook: after target resolution, on the final webhook input → `classify_execute_error(anyhow::Error::new(e))` (400). Action hook: `preflight(action.input, rendered_input)` → bail (logged to the source job by `fire_hooks_of_kind`).

- [ ] **Step 1: Write the failing tests:** `scheduler_required_file_skip_policy_is_missed_without_skipped_row`, `scheduler_required_file_cancel_previous_cancels_nothing` (a running job of that trigger stays running), `scheduler_trigger_input_with_file_value_is_missed`, `webhook_required_file_is_400_and_creates_no_job`, `action_hook_required_file_fails_and_logs_to_source_job`, `task_hook_required_file_fails_and_logs_to_source_job`, unit `preflight_cases` (optional absent ok, `null` ok, value → CannotUse, required → Required).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(server): file-input preflight for triggers, webhooks and hooks`.

---

### Task 10: Template context — ID → path in `build_entries`

**Files:**
- Modify: `crates/stroem-server/src/render_context.rs` (`Snapshots.files`, `build_entries` :267-300, new loader, unit tests)
- Modify: `src/settlement/mod.rs:315` (advance), `src/settlement/dispatch.rs:1371` (init), `src/web/worker_api/jobs.rs:982` (claim root, Task 11), `src/web/worker_api/jobs.rs:1153` + `rendering.rs:503` + other `Scope::ActionBody {` sites (6)
- Modify: `src/input_files/mod.rs`

**Interfaces — Produces:**
```rust
// render_context.rs
pub struct Snapshots { pub task: Option<Snapshot>, pub global: Option<Snapshot>, pub files: JobFiles }  // Default: JobFiles::empty()
Scope::ActionBody { prepared_input: Option<&'a Value>, action_schema: Option<&'a HashMap<String, InputFieldDef>> }
// input_files/mod.rs
pub async fn load_attachments(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Vec<JobInputFileRow>>;
pub fn job_files(rows: &[JobInputFileRow], root: &str) -> JobFiles;
pub fn claimed_files(rows: &[JobInputFileRow]) -> Vec<ClaimedInputFile>;   // distinct by file_id
```
`build_entries`: every scope that uses `job.job_input` (StepInput, AgentPrompt, Condition, ChildTaskInput, ApprovalMessage, StepRef, TaskActionRef) inserts `job.snapshots.files.template_input(input)`; `ActionBody` inserts `files.template_prepared(prepared, schema)` (`schema` None → unchanged). The `ApprovalMessage { rendered_input }` override is inserted unchanged (approval inputs cannot be `file`, Task 2). `webhook_ref` has no job and is unchanged. Entries: `Settlement::advance` and `dispatch::init` set `snapshots.files = job_files(&load_attachments(pool, job_id).await?, CANONICAL_INPUTS_ROOT)` right after `latest_snapshots` (an error propagates; recovery re-advances). Update `Snapshots` doc: per entry, per job.

- [ ] **Step 1: Write the failing tests** (`render_context.rs` unit, one per § 5.3 row): `step_input_scope_renders_path`, `action_body_scope_renders_path_for_file_fields_only`, `agent_prompt_scope_renders_canonical_path`, `condition_scope_absent_optional_field_stays_absent`, `child_task_input_scope_renders_path`, `step_ref_and_task_action_ref_scope_render_path`, `collision_case_two_files_same_name` (task fields `a`,`b` → two uploads named `report.csv`; action field `a` given `{{ input.b }}`: S1 renders `…/file_B/report.csv` for `{{ input.b }}`, S2/S3 renders the same for `{{ input.a }}` after A3 → B, and `{{ input.a }}` in S1 renders `…/file_A/…`). Integration (`input_file_creation_test.rs`): `when_condition_sees_file_given` (`when: "{{ input.data_file }}"` true with a file, step skipped without).
- [ ] **Step 2: Run** `cargo test -p stroem-server --lib render_context::` → FAIL. **Step 3: Implement.** **Step 4: Run** lib + `cargo test -p stroem-server --test integration orchestrator_test:: cascade_apply_test::` → PASS.
- [ ] **Step 5: Commit** `feat(server): render file inputs as paths`.

---

### Task 11: Claim protocol and worker download endpoint

**Files:**
- Modify: `crates/stroem-server/src/web/worker_api/jobs.rs` (`ClaimRequest` :54, `ClaimResponse` :67 + `no_work` + 9 literals, `claim_job` :819), `rendering.rs` (`render_step_input`, `merge_missing_action_fields` :308, `PrepareContext`)
- Create: `crates/stroem-server/src/web/worker_api/input_files.rs`; Modify `web/worker_api/mod.rs` (route ABOVE the auth layer at :124)
- Create: `crates/stroem-server/tests/input_file_claim_test.rs`

**Interfaces — Produces (wire protocol, shared with Task 18):**
```rust
// ClaimRequest
#[serde(default)] pub inputs_dir: Option<String>,          // absolute, else 400
// ClaimResponse
#[serde(default, skip_serializing_if = "Vec::is_empty")] pub input_files: Vec<ClaimedInputFile>,
// rendering.rs
pub fn merge_missing_action_fields(input_val: &mut Value, job_input: Option<&Value>, action_schema: &HashMap<String, InputFieldDef>, files: &JobFiles);
pub fn inputs_root(action_type: &str, runner: Option<&str>, inputs_dir: Option<&str>) -> Option<String>; // docker|pod type or runner → "/inputs"; local → inputs_dir
pub const OLD_WORKER_INPUTS: &str = "this step has file inputs, which the worker does not support; upgrade the worker";
```
`GET /worker/jobs/{job_id}/input-files/{file_id}`: `JobInputFileRepo::get_attached` → 404 unless attached; no archive → 503 `file storage is not configured` + `Retry-After: 5`; blob read error → 503 + `Retry-After: 5`; missing blob → 404; else `download_body` with `Content-Length` = `size_bytes`, `Content-Type: application/octet-stream`.
Claim, after the job row loads: `rows = load_attachments(..)` (error → `fail_claimed_step` "could not load the job's input files"); agent step → files ignored, `input_files` empty, root canonical; else if `!rows.is_empty()` and `inputs_dir` is None → `fail_claimed_step(OLD_WORKER_INPUTS)` (not withheld, ordinary claim failure, retry applies); `snapshots.files = job_files(&rows, &root)`; `render_step_input` uses `render_input_typed_with_files(.., &snapshots.files)`; `prepare_step_action_input` → new `merge_missing_action_fields` (task file field → id into a `file` action field, `files.path_of(id)` into any other); `Scope::ActionBody { prepared_input, action_schema: input_schema.as_ref() }`; `input_files = claimed_files(&rows)`.

- [ ] **Step 1: Write the failing tests** (`input_file_claim_test.rs`; claim via the worker router with `inputs_dir: "/w/in"`):
  - `local_step_renders_worker_root` (`script: cat "{{ input.data_file }}"` → rendered spec contains `/w/in/file_…/report.csv`; `input_files` = one entry with size + sha).
  - `docker_and_pod_steps_render_inputs_root` (runner docker, type docker, runner pod, type pod → `/inputs/file_…/…`).
  - `step_input_path_into_string_action_field` and `file_action_field_receives_id_and_renders_path`.
  - `merge_missing_by_target_type` (unset action field `data_file: file` → id in persisted step input; unset `data_file: string` → path).
  - `collision_case_claim_lists_two_files_with_distinct_paths` (rev-4 case end to end).
  - `same_upload_in_two_fields_is_one_entry` (Review Focus 2).
  - `step_without_using_file_still_receives_it` (U9).
  - `old_worker_without_inputs_dir_fails_step` (error_message = `OLD_WORKER_INPUTS`; a job WITHOUT files claims fine without `inputs_dir`).
  - `agent_step_gets_no_input_files`.
  - `relative_inputs_dir_is_400`.
  - `foreign_path_in_file_target_fails_claim_value_free` (another root, another job's id → step failed with `Input field 'data': the file cannot be used`).
  - `worker_download_streams_attached_file` (200, body, `content-length`), `worker_download_unattached_is_404`, `worker_download_requires_worker_token` (401), `worker_download_without_archive_is_503_with_retry_after`.
- [ ] **Step 2: Run** `cargo test -p stroem-server --test integration input_file_claim_test::` → FAIL. **Step 3: Implement** (also update the 9 `ClaimResponse` literals). **Step 4: Run** → PASS; then `cargo test -p stroem-server --lib web::worker_api` and the whole integration binary → PASS.
- [ ] **Step 5: Commit** `feat(server): claim input files and worker download`.

---

### Task 12: `type: task` children (S6 reverse lookup + A2 via parent)

**Files:** Modify `src/settlement/dispatch.rs:470-1003` (`handle_task_steps_pass`: bucket C render at :692 → `render_input_typed_with_files(.., &snapshots.files)`; child creation :933 → `FileGrant::TaskChild`). Test: `tests/input_file_creation_test.rs`.

- [ ] **Step 1: Write the failing tests:** `task_child_same_workspace_gets_file` (parent `input: {data_file: "{{ input.data_file }}"}` → child `job_input_file` row with the parent's id, child field name may differ), `task_child_cross_workspace_gets_file` (child in workspace `other`; its job detail lists the file; owner-side render rules unchanged), `task_child_string_field_receives_canonical_path`, `task_child_foreign_id_fails_step_value_free` (literal id of an upload not attached to the parent → step failed with `Input field '…': the file cannot be used`, job failed by cascade), `agent_tool_child_gets_no_files` (A2 not granted: a tool call naming the parent's file id → creation 400).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** + `cargo test -p stroem-server --test integration orchestrator_test::` → PASS.
- [ ] **Step 5: Commit** `feat(server): give a job's files to type: task children`.

---

### Task 13: Job detail `input_files[]` and redaction

**Files:** Modify `src/web/api/jobs.rs:250-602` (`JobDetailResponse`), `src/redaction.rs:127-170,593-612`; tests in `tests/input_file_creation_test.rs` and `redaction.rs` unit tests.

**Interfaces — Produces:**
```rust
#[derive(Debug, Serialize)] pub struct InputFileRef { pub field: String, pub file_id: String, pub name: String, pub size_bytes: i64, pub content_type: String }
// JobDetailResponse: pub input_files: Vec<InputFileRef>   (always present, [] when none; ordered by field)
pub(crate) const INPUT_FILE_IDENTIFIER_KEYS: &[&str] = &["field", "file_id"];
```
`walk_job_content`: `key == "input_files"` → each element `walk_object_except(el, INPUT_FILE_IDENTIFIER_KEYS, f)`, like `steps`.

- [ ] **Step 1: Write the failing tests:** `job_detail_lists_input_files` (fields, original name, size, content type); unit `redaction_keeps_input_file_identifiers_with_secret_file` (secret value `"file"` → `field`, `file_id` intact; `name` containing `file` masked); unit `mask_all_keeps_input_file_identifiers`; `mask_all_masks_input_file_name_and_content_type`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** + `cargo test -p stroem-server --test integration read_path_audit_test:: pinned_api_fields_test::` → PASS.
- [ ] **Step 5: Commit** `feat(server): list input files on the job page API`.

---

### Task 14: Retention and the orphan sweep (§ 4.7)

**Files:** Modify `src/recovery.rs:473-603` (`retention_cleanup`); `src/metrics.rs` optional none. Test: `tests/input_file_retention_test.rs` (+ `tests/main.rs`).

After the job-deletion loop, in the same leader-gated tick: `UploadTokenRepo::delete_expired`; then `InputFileRepo::sweep_candidates(pool, upload_ttl_secs, 1000)`; per file: `recovery_beat.beat()`, `mark_deleting` (None → skip), `input_blob.delete(storage_key)` (error → warn + keep row with `deleting_at` set), `InputFileRepo::delete`. Without an `input_blob` backend the file sweep is skipped with one `debug!` per tick (no blob can exist; rows stay); token cleanup still runs.

- [ ] **Step 1: Write the failing tests:** `retention_deletes_job_attachments_keeps_file_until_ttl`, `sweep_deletes_unattached_expired_upload_blob_and_row`, `sweep_keeps_young_or_attached_upload`, `sweep_retries_blob_delete_after_failure` (archive wrapper failing once → row has `deleting_at`, next sweep removes it), `sweep_removes_expired_upload_tokens`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** + `cargo test -p stroem-server --test integration artifact_retention_test::` → PASS.
- [ ] **Step 5: Commit** `feat(server): sweep unused input files`.

---

### Task 15: MCP inline files, `create_upload_url`, `/api/uploads/{token}`

**Files:** Modify `src/mcp/tools.rs` (`execute_task` :493, new tool, `get_task` description), `src/web/api/input_files.rs` (token handler), `src/web/api/mod.rs` (route in the `public` router, `DefaultBodyLimit::max(max_file_bytes)`). Tests: `tests/mcp_test.rs`, `tests/input_file_upload_test.rs`.

**Interfaces:**
```rust
#[derive(Deserialize, JsonSchema)] pub struct CreateUploadUrlParams { pub workspace: String, pub task_name: String, pub file_name: String }
// tool result: {"upload_url": "{base_url}/api/uploads/{token}", "method": "POST", "max_bytes": u64, "expires_at": RFC3339}
pub(crate) fn decode_inline(field: &str, value: &serde_json::Value, cap: u64) -> Result<Option<(String /*name*/, Vec<u8>)>, InlineError>; // Some for {"name","content"} | {"name","content_base64"}
```
`execute_task`: for each task `file` field whose value is an object → `decode_inline` (over cap → `invalid_params` with `Input field '{f}': inline content is larger than {cap} bytes`; bad shape/base64 → `invalid_params` `Input field '{f}': the file cannot be used`) → `store_upload(origin "mcp_inline", user = caller)` → replace with the id → create with `FileGrant::direct`. `create_upload_url`: Run on the task (Deny → not found, View → acl error), task has a `file` field, `safe_name(file_name)` valid, `auth.base_url` set (else the fixed tool error) → token = 32 random bytes hex, store SHA-256, TTL `upload_url_ttl_secs`. `POST /api/uploads/{token}` (no Bearer): `UploadTokenRepo::consume(sha256(token))` → None → 404 `Upload URL not found`; else `store_upload(origin "mcp_url", user = token user, original_name = token name)` → 201 (§ 4.2 response).

- [ ] **Step 1: Write the failing tests:** `mcp_execute_inline_text_file` (content → job with attachment, `origin = mcp_inline`), `mcp_execute_inline_base64_file`, `mcp_execute_inline_over_cap_names_field_and_cap`, `mcp_execute_inline_bad_base64_is_invalid_params`, `mcp_create_upload_url_and_upload_once` (POST without Bearer → 201; second POST → 404; then `execute_task` with the id → job), `mcp_upload_url_expired_is_404`, `mcp_create_upload_url_without_base_url_is_tool_error` (no `auth` section → exact text), `mcp_create_upload_url_view_only_is_denied`, `mcp_get_task_shows_file_type`.
- [ ] **Step 2: Run** `cargo test -p stroem-server --test integration mcp_test::mcp_ input_file_upload_test::` → FAIL. **Step 3: Implement** (tool descriptions explain inline vs URL). **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(mcp): send files inline or through an upload URL`.

---

### Task 16: `RunConfig.inputs` and the Docker mount

**Files:** Modify `crates/stroem-runner/src/traits.rs:31-83`, `src/docker.rs:123-205` (+ tests), every `RunConfig {` literal (scripted: 24 docker.rs, 26 kubernetes.rs, 3 shell.rs, `crates/stroem-cli/src/local/run.rs:661`, `crates/stroem-worker/src/executor.rs:488`).

**Interfaces — Produces:**
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInputs { pub job_id: String, pub host_dir: Option<String>, pub files: Vec<stroem_common::file_field::ClaimedInputFile> }
// RunConfig: pub inputs: Option<RunInputs>,   // None = no file inputs
```
Docker: when `inputs.host_dir` is Some, push `"{host_dir}:/inputs:ro"` in BOTH modes.

- [ ] **Step 1: Scripted literal update** (`inputs: None,` after each `workspace_revision:` inside a `RunConfig` literal); `cargo check -p stroem-runner -p stroem-worker -p stroem-cli --all-targets`.
- [ ] **Step 2: Write the failing tests:** `with_workspace_binds_inputs_read_only`, `no_workspace_binds_inputs_read_only`, `no_inputs_no_bind`; `#[ignore] docker_step_reads_input_file` (real daemon: host temp dir with `file_x/r.csv` → `cat /inputs/file_x/r.csv` output, and a write fails).
- [ ] **Step 3: Run** `cargo test -p stroem-runner docker::` → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS; run the ignored test once: `cargo test -p stroem-runner docker::tests::docker_step_reads_input_file -- --ignored` → PASS (record in the commit body).
- [ ] **Step 6: Commit** `feat(runner): mount input files in Docker steps`.

---

### Task 17: Kube init container `stroem-inputs-init`

**Files:** Modify `crates/stroem-runner/src/kubernetes.rs` (const script, `inject_inputs_init(&self, pod_json: &mut Value, config: &RunConfig)` called from `build_pod_spec` :421 for both modes BEFORE `merge_json`), tests in the same module.

**Interfaces — Produces:**
```rust
pub(crate) const INPUTS_INIT_CONTAINER: &str = "stroem-inputs-init";
pub(crate) const INPUTS_VOLUME: &str = "stroem-inputs";
pub(crate) const INPUTS_INIT_SCRIPT: &str = "…";   // constant; reads env only
fn input_file_url(&self, job_id: &str, file_id: &str) -> String;   // {server_url}/worker/jobs/{job}/input-files/{file}
fn inputs_init_env(&self, inputs: &RunInputs) -> Vec<Value>;       // STROEM_WORKER_TOKEN, STROEM_INPUTS_ROOT=/inputs, STROEM_INPUT_COUNT, STROEM_INPUT_{i}_{ID,NAME,URL,SIZE,SHA256}
```
Script (POSIX `sh`, `set -eu`): loop `i` from 0 to `STROEM_INPUT_COUNT-1`; read the five values with `eval "id=\${STROEM_INPUT_${i}_ID}"` (only `$i`, a number, is spliced; values are never parsed as code); `mkdir -p "$STROEM_INPUTS_ROOT/$id"`; `curl -sSf --retry 11 --retry-delay 5 -H "Authorization: Bearer $STROEM_WORKER_TOKEN" -o "$dir/$name" "$url"`; size check `[ "$(wc -c < "$f" | tr -d ' ')" = "$size" ]` and digest check `[ "$(sha256sum "$f" | cut -d ' ' -f 1)" = "$sha" ]`, each failing with a one-line stderr message naming the file id and `exit 1`. Volume `{"name":"stroem-inputs","emptyDir":{}}`; init mount rw at `/inputs`; `step` mount `{"name":"stroem-inputs","mountPath":"/inputs","readOnly":true}`. Added only when `inputs.files` is non-empty. The init container is appended to `initContainers` (created for NoWorkspace).

- [ ] **Step 1: Write the failing tests:**
  - Spec: `inputs_init_added_with_workspace` (2 init containers, order workspace-init then stroem-inputs-init, volume, ro mount), `inputs_init_added_no_workspace`, `no_inputs_no_init_container` (existing exact-count tests still pass), `manifest_override_can_rewrite_inputs_init` (override by name merges), `inputs_init_env_is_safe` (every value matches `^[A-Za-z0-9._:/=?&%-]*$`, script text equals the constant).
  - Executed on the host (skip with a message when `curl`/`sha256sum`/`sh` missing): helper `run_inputs_script(root, env) -> ExitStatus`; `inputs_script_downloads_and_verifies_from_empty_root` (2 files, wiremock, header `Authorization: Bearer test-token` matched), `inputs_script_size_mismatch_fails`, `inputs_script_digest_mismatch_fails`, `inputs_script_404_fails`, `inputs_script_retries_503_then_succeeds`.
  - Executed in the real image (skip with a message when `docker info` fails; NOT `#[ignore]`, so CI runs it): `inputs_script_in_real_curl_image` — wiremock bound on `0.0.0.0` (`MockServer::builder().listener(std::net::TcpListener::bind("0.0.0.0:0")?)`), URLs use `http://host.docker.internal:{port}`; `docker run --rm --add-host host.docker.internal:host-gateway -e STROEM_INPUTS_ROOT=/tmp/inputs -e … --entrypoint sh {DEFAULT_INIT_IMAGE} -c "$SCRIPT && cat /tmp/inputs/*/*"`. `/tmp/inputs` does not exist at start (empty root, created by the script's `mkdir -p`); no bind mount; the image's default non-root user, never `--user root` (a pod `emptyDir` is writable by it too). Variants: success (output = both files), size mismatch, digest mismatch, 404 — each failure exits non-zero.
- [ ] **Step 2: Run** `cargo test -p stroem-runner kubernetes::` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS, and confirm in the output that the real-image test RAN (not skipped).
- [ ] **Step 5: Commit** `feat(runner): download input files in a pod init container`.

---

### Task 18: Worker — claim, download, place, clean up

**Files:** Modify `crates/stroem-worker/src/client.rs` (`ClaimRequest` :150, `ClaimResponse` :101, `ClaimedStep` :22 + mapping :311-343 + 10 literals, new download fn), `src/poller.rs` (before `report_step_start` :274), `src/executor.rs` (`build_run_config` :348 / `execute_step`), `Cargo.toml` (`sha2.workspace = true`, `hex = "0.4"`).

**Interfaces — Produces:**
```rust
// ClaimedStep
#[serde(default)] pub input_files: Vec<ClaimedInputFile>,
#[serde(default)] pub inputs_dir: Option<String>,     // the dir this claim request announced
// client.rs
pub(crate) const INPUT_503_ATTEMPTS: u32 = 12; pub(crate) const INPUT_503_DELAY: Duration = Duration::from_secs(5);
pub async fn download_input_file(&self, job_id: Uuid, file: &ClaimedInputFile, dest: &Path) -> Result<()>;  // chunk() loop to a temp file in dest's dir, sha256 + size verify, rename; 503 → InputUnavailable marker
pub(crate) async fn download_input_file_with_retry(client: &ServerClient, job_id: Uuid, file: &ClaimedInputFile, dest: &Path, attempts: u32, delay: Duration) -> Result<()>;
// poller.rs
pub(crate) struct InputsDirGuard(PathBuf);   // Drop: chmod dirs 0755 + files 0644 recursively, then remove_dir_all (best effort, warn)
pub(crate) fn needs_worker_download(step: &ClaimedStep) -> bool;  // files non-empty and runner is not kube (type pod / runner pod)
```
`claim_step`: each request sends `inputs_dir = temp_dir()/stroem-inputs-{uuid simple}` (absolute). After a claim with files and `needs_worker_download`: create `{dir}/{file_id}/` (then `0555`), download each to `{dir}/{file_id}/{safe_name}` (`0444`), guard owns `dir`; a failure → `push_error_log` + `report_step_complete(-1, "Failed to download input files: …")` and return, BEFORE `report_step_start`. A claim with files but no `inputs_dir` (cannot happen) → same failure. `build_run_config`: `inputs = Some(RunInputs { job_id, host_dir: Some(dir) for docker / None for kube & local, files })` when files are non-empty.

- [ ] **Step 1: Write the failing tests** (wiremock): `claim_request_sends_absolute_inputs_dir` (two consecutive claim requests announce two different dirs; none is created before a claim returns files), `claimed_step_parses_input_files`, `download_input_file_verifies_size_and_sha` (ok; size mismatch → Err, no file left; digest mismatch → Err), `download_input_file_retries_503_then_ok`, `download_input_file_gives_up_after_attempts` (attempts 2, delay 1 ms), `inputs_dir_guard_removes_read_only_tree`, `needs_worker_download_cases` (local, docker, `type: docker` → true; pod, `type: pod`, agent → false), `run_config_carries_host_dir_for_docker_only`, `poller_input_download_failure_fails_step_before_start` (start never reported; complete reported with exit -1).
- [ ] **Step 2: Run** `cargo test -p stroem-worker` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(worker): place input files for local, Docker and pod steps`.

---

### Task 19: CLI — `stroem-api trigger --file` and `stroem run`

**Files:** Modify `crates/stroem-cli/src/remote/mod.rs:18-27,140-146`, `src/remote/trigger.rs`, `crates/stroem-cli/Cargo.toml` (`reqwest = { workspace = true, features = ["stream"] }`), `src/local/run.rs` (input parse :53, render :510-518), `src/local/mod.rs` help text.

**Interfaces:**
```rust
// Commands::Trigger gains: #[arg(long = "file", value_name = "FIELD=PATH")] files: Vec<String>
pub fn parse_file_arg(s: &str) -> Result<(String, PathBuf)>;          // "field=path", both non-empty
pub fn upload_url(server: &str, ws: &str, task: &str, name: &str) -> String;  // …/files?name=<urlencoded basename>
```
Trigger: upload each file (streamed `reqwest::Body::wrap_stream(ReaderStream::new(File))`, `Content-Type: application/octet-stream`), insert `field → file_id` into the input object (a key in both `--input` and `--file` → error), then execute. `stroem run`: for each task `file` field with a value: must be a regular file (else error naming the field); `local_id = new_file_id()`; copy to `{tmp}/stroem-run-inputs-{uuid}/{local_id}/{safe_name}` (`0444`, dirs `0555`); job input value = `local_id`; `JobFiles::new(root, …)`; S1 render via `render_input_typed_with_files`; the action body context gets `template_input` / `template_prepared` exactly like the server (use the same `JobFiles` helpers); the temp root is removed at the end (guard like the worker's).

- [ ] **Step 1: Write the failing tests:** `parse_file_arg_cases`, `upload_url_encodes_name`, `trigger_uploads_then_executes` (wiremock: POST …/files?name=r.csv → 201 `{file_id}`, then execute body `{"input":{"data_file":"file_…"}}`), `trigger_file_and_input_same_key_is_error`; local: `run_file_field_renders_local_path_and_reads_file` (task with `script: cat "{{ input.data_file }}"` → output contains the file content), `run_file_field_not_a_file_is_error`, `run_file_field_read_only`.
- [ ] **Step 2: Run** `cargo test -p stroem-cli` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(cli): file inputs for stroem-api trigger and stroem run`.

---

### Task 20: UI — Run form, Re-run prefill, job page (parallel with Rust tasks; no cargo)

**Files:**
- Modify: `ui/src/components/task/constants.ts` (`"file"`), `ui/src/components/task/input-field-row.tsx` (file branch first, before `json`), `ui/src/lib/execute-input.ts`, `ui/src/pages/task-detail.tsx` (defaults + `inputFiles` state + Run disabled), `ui/src/pages/job-detail.tsx` (Re-run `Link` state `inputFiles: job.input_files`, card), `ui/src/lib/types.ts` (`InputFileRef`, `JobDetail.input_files?`), `ui/src/components/artifact-list.tsx` (export `triggerArtifactAction`, `SAFE_INLINE`, `humanSize`)
- Create: `ui/src/lib/upload.ts`, `ui/src/lib/file-field.ts`, `ui/src/components/task/file-input-field.tsx`, `ui/src/components/input-file-list.tsx`, tests `ui/src/lib/__tests__/upload.test.ts`, `ui/src/lib/__tests__/file-field.test.ts`, `ui/src/components/task/file-input-field.test.tsx`, `ui/src/components/input-file-list.test.tsx`, additions to `ui/src/lib/__tests__/execute-input.test.ts`, `ui/src/pages/__tests__/job-detail.test.tsx`
- Create: `workspace/.workflows/file-input.yaml` (task `file-demo`: input `data_file: {type: file, required: true}`; one `type: script` local step `sha256sum "{{ input.data_file }}" && cat "{{ input.data_file }}"`), `ui/e2e/file-inputs.spec.ts`

**Interfaces:**
```ts
// lib/types.ts
export interface InputFileRef { field: string; file_id: string; name: string; size_bytes: number; content_type: string; }
// lib/file-field.ts
export type FileFieldState =
  | { kind: "file"; mode: "empty" }
  | { kind: "file"; mode: "uploading"; name: string; size: number; progress: number }
  | { kind: "file"; mode: "failed"; name: string; error: string }
  | { kind: "file"; mode: "ready"; fileId: string; name: string; size: number; fromSource: boolean };
export function isFileFieldState(v: unknown): v is FileFieldState;
export function initialFileFieldState(source?: InputFileRef): FileFieldState;   // ready+fromSource when given
export function fileFieldBlocksRun(v: unknown): boolean;                        // uploading | failed
// lib/upload.ts
export function uploadInputFile(ws: string, task: string, file: File, opts?: { sourceJobId?: string; onProgress?: (fraction: number) => void; signal?: AbortSignal }): Promise<{ file_id: string; name: string; size_bytes: number; sha256: string; content_type: string; expires_at: string }>;
```
`uploadInputFile` uses `XMLHttpRequest` (progress), `Authorization: Bearer getAccessToken()` (after `tryRestoreSession()` when none), one retry after `tryRestoreSession()` on 401, `Content-Type: file.type || application/octet-stream`, URL `/api/workspaces/{ws}/tasks/{task}/files?name=…[&source_job_id=…]` with `task.id`; errors `new ApiError(status, body.error || statusText)`. `buildExecutePayload`: `ready` → `input[key] = fileId`; `empty` + required → `ExecuteFormError(key, "a file is required")`; `empty` optional → omitted. Run button disabled when any value `fileFieldBlocksRun`. Re-run: task-detail reads `location.state.inputFiles` (array) and seeds `ready, fromSource` for matching fields — NEVER from `rawInput`; Replace uploads with `sourceJobId`. Job page: "Input files" `<section>` like `ArtifactList` (heading `Input files`, row: name, `humanSize`, content type, field; Download/Open button via `triggerArtifactAction("/api/jobs/{id}/input-files/{encodeURIComponent(field)}", name, inline)`), hidden when empty.

- [ ] **Step 1: Write the failing tests:** `upload.test.ts` (FakeXhr stub: sends Bearer + raw body, reports progress, 401 → refresh + retry once, 413 → ApiError with server text, `source_job_id` in URL); `file-field.test.ts`; `file-input-field.test.tsx` (pick file → uploading with progress → ready shows name + size; Remove → empty; failed shows error + Run blocked; re-run source shows "From the previous run" + Replace); `execute-input.test.ts` (ready → id, empty required → error, empty optional → omitted); `input-file-list.test.tsx` (renders rows, hidden when empty, download calls `apiFetchRaw` with the field-encoded URL); `job-detail.test.tsx` (Re-run link state carries `inputFiles`).
- [ ] **Step 2: Run** `cd ui && bun run test` → FAIL. **Step 3: Implement.** **Step 4: Run** `bun run test`, `bun run lint`, `bun run build` → PASS.
- [ ] **Step 5: Playwright spec** `file-inputs.spec.ts`: login → `/workspaces/default/tasks/file-demo` → `setInputFiles` on `#input-data_file` → wait for ready → Run Task → job page shows "Input files" card with the name → Download triggers a response with the file bytes → Re-run → field shows the source file → Run → new job completes. Run against the compose stack when available; otherwise record "not run" for the controller.
- [ ] **Step 6: Commit** `feat(ui): file inputs in the run form, re-run and job page`.

---

### Task 21: E2E section

**Files:** Modify `tests/e2e.sh` (new `# --- 22. File inputs: upload, execute, read, download, re-run ---` before Summary); fixture from Task 20 (`file-demo` in the default workspace).

- [ ] **Step 1:** Section: create a 3-line temp file; `POST /api/workspaces/default/tasks/file-demo/files?name=report.csv` (raw body) → `file_id`; execute → poll completed; step log contains the file's `sha256sum`; `GET /api/jobs/{id}/input-files/data_file` equals the file; job detail `input_files[0].name == "report.csv"`; Re-run (execute with `source_job_id` and the same id) → completed. A docker step is out of scope here (compose worker has no Docker daemon; Task 16 ignored test covers it).
- [ ] **Step 2:** `bash -n tests/e2e.sh`; run `./tests/e2e.sh` when ≥ 15 GiB free, else record "written, not run" for the PR.
- [ ] **Step 3: Commit** `test(e2e): file inputs end to end`.

---

### Task 22: Documentation

**Files:**
- Create: `docs/src/content/docs/guides/file-inputs.md` (YAML, UI, REST, CLI, MCP inline + URL, runners incl. pod init container + `for_each` downloads, limits, safe-name rule, U9 every step gets every file, cross-workspace consequence (§ 4.6), Helm DinD limitation, rollout order, retention/TTL, troubleshooting: pod init failures show exit 1 only)
- Create: `docs/src/content/docs/operations/migration-054.md` (tables only, rollout order)
- Modify: `docs/src/content/docs/guides/input-and-output.md` (type list), `guides/action-types.md`, `guides/runners.md` (`/inputs`), `guides/mcp.md`, `guides/rerun-and-restart.md`, `guides/artifacts.md` (inline list correction if wrong), `reference/api.md` (upload, download, `/api/uploads/{token}`, job detail `input_files`), `reference/worker-api.md` (claim fields, download route), `reference/cli.md`, `reference/workflow-yaml.md`, config reference page (`input_files:`), `operations/metrics.md` (two metrics), `docs/scripts/generate-llms-txt.ts` (`sections` + file-inputs), sidebar if it is manual (`docs/astro.config.mjs`)
- Modify: `CLAUDE.md` (new `### File Inputs` Key Patterns section: § 4.4 A1/A2/A3 + `FileGrant`, § 5.1 ID→path only in `build_entries` via `Snapshots.files`, § 6.4 constant init script + executed tests, § 9 streaming + alloc cases; fix § Artifacts: transfers did not stream before this change — now `get_stream` streams but artifact UPLOAD still buffers (`put`); inline allow-list is png/jpeg/gif/webp/text/plain, not PDF/Markdown; Phase line), `CONTEXT.md` (Input file, Upload, File ID, Safe name, Inputs root, Attachment, Upload URL), `docs/internal/stroem-v2-plan.md` (status), `docs/internal/TODO.md` (mark done items; keep § 15 list)

- [ ] **Step 1:** Write the pages (base-less internal links, CLAUDE.md § Documentation). **Step 2:** `cd docs && bun run build` and `bun run test` → PASS. **Step 3: Commit** `docs: file inputs guide, references and rollout note`.

---

### Task 23: Full verification (controller)

- [ ] `cargo fmt --check --all`
- [ ] `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `DOCKER_HOST=… TESTCONTAINERS_RYUK_DISABLED=true CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo test --workspace --no-fail-fast`
- [ ] `cargo test -p stroem-runner docker::tests::docker_step_reads_input_file -- --ignored`
- [ ] `cd ui && bun run lint && bun run build && bun run test`
- [ ] Playwright `file-inputs.spec.ts` and `tests/e2e.sh` if disk allows; otherwise say so in the PR.
- [ ] grep the tests tree for any old error text changed by this work; re-run the whole integration binary after any response-shape change.
