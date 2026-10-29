# File inputs (`type: file`)

Status: draft, revision 1 (2026-10-29). Branch `feat/file-inputs`.

## Revision history

- **Revision 1** (2026-10-29) — first draft, from the brainstorming
  conversation of 2026-10-26 / 2026-10-29 and three read-only code recons.
- **Revision 2** (2026-10-29) — after the builder check. Author's calls:
  `file` field-name rule checked at validation, creation and claim (§ 4.1,
  F19); sweep-vs-creation lock protocol replaced (a single `UPDATE … WHERE
  NOT EXISTS` raced, § 4.7); `merge_missing_action_fields` copies by target
  type (§ 5.1); `create_upload_url` needs `auth.base_url` (§ 7.5, F18
  corrected). Open for the user: which files a step receives (§ 5.4).

## 1. Goal

A person or a program gives a task a file, and the script of a step gets a
local path to that file, for example as a command-line argument:

```yaml
tasks:
  import-report:
    input:
      data_file:
        type: file
        required: true
    flow:
      import:
        action: import
        input:
          path: "{{ input.data_file }}"

actions:
  import:
    type: script
    runner: pod
    script: python import.py "{{ input.path }}"
```

`{{ input.data_file }}` renders to a path such as
`/inputs/data_file/report.csv`. The file is read-only in the step.

Today the only way into a step is a string-shaped input value rendered into a
template; artifacts move files the other way (step → server). This feature is
the reverse direction of artifacts.

## 2. Decisions made by the user

These came from the user in the brainstorming conversation. They are not open.

| # | Decision |
|---|---|
| U1 | A file is uploaded and the script gets a **path** to it. The path is the only template value; file name, size and content type are not exposed to templates (for now). |
| U2 | Files come from the **UI Run form**, the **REST API** and **MCP**. |
| U3 | Files are small-to-medium: **< 100 MiB** (config default 100 MiB). |
| U4 | Every runner works: `runner: local`, `runner: docker`, `type: docker`, **`runner: pod`, `type: pod`**. |
| U5 | MCP supports **both** inline content (small files) **and** a one-time **upload URL** (agents with a shell). |
| U6 | A flow step can give the job's file to a **`type: task` child** or to an **action with a `file` field**, **also across workspaces**. |
| U7 | The pod `/state` repair found during recon is a **separate task**, not part of this change (§ 15). |
| U8 | The user accepted the lifecycle and the client list played back in the conversation: two-call model (upload → execute), Re-run / Restart reuse the source file, unused uploads expire, the job page lists input files with a download link, `stroem-api trigger --file`, `stroem run` takes a local path, and the first version excludes webhooks, scheduler triggers and event sources as file sources, multiple files per field, and secret masking of file content. |

## 3. Facts (recon, at `42b4b1e3`)

Storage

- F1. `BlobArchive` (`crates/stroem-server/src/blob_storage.rs:58`) has S3 and
  Local backends. **No backend overrides `put_stream` / `get_stream`**: the
  trait defaults collect the whole body into memory (`:64`, `:84`). Local and
  S3 override only `open` / `read_range`.
- F2. Artifact uploads (`web/worker_api/artifacts.rs:118`) and the manual state
  upload (`web/api/state_upload.rs:329`) take the raw request body as `Bytes`,
  fully buffered, not multipart. axum's `multipart` feature is not enabled
  (`crates/stroem-server/Cargo.toml`). Body limits are per route via
  `DefaultBodyLimit`; every other route has axum's 2 MiB default.
- F3. Artifact blobs are deleted two-phase (blob, then rows) inside the
  leader-gated retention sweep (`recovery.rs:528-603`); `job_artifact.job_id`
  is `ON DELETE RESTRICT`.
- F4. The production server ran with a 512 MiB memory limit in September 2026
  (both replicas OOMKilled on a big-log page). A few concurrent 100 MiB
  uploads buffered in memory would repeat that.

Runners

- F5. The worker downloads everything it gives a step to its own disk
  (`stroem-worker/src/poller.rs:347-480`); downloads read the whole body with
  `.bytes()` (`client.rs:465`, `:642`). No worker code streams to disk.
- F6. ShellRunner gets host paths through env vars; DockerRunner bind-mounts
  host temp dirs (`docker.rs:129-143`); `type: docker` (NoWorkspace) gets only
  `/artifacts`.
- F7. KubeRunner (WithWorkspace) has ONE init container, `workspace-init`
  (`kubernetes.rs:219`), running `workspace_init_script` (`:44`): `curl -sSf
  --retry 11 --retry-delay 5` with `Authorization: Bearer
  $STROEM_WORKER_TOKEN` to `{server_url}/worker/workspace/{ws}.tar.gz?revision=…`,
  then `tar`. The worker token is a plain env value in the pod spec (known,
  TODO.md). `type: pod` (NoWorkspace) has **no** init container
  (`kubernetes.rs:249-310`).
- F8. Nothing but the workspace reaches a pod today. The `/state` volumes of a
  pod are empty `emptyDir`s that nothing fills (`kubernetes.rs:179-208`,
  comment `:195-199`), although `guides/task-state.md:124-127` says otherwise
  (U7, § 15). Kube artifacts are not supported (`executor.rs:182`).
- F9. Pod manifest overrides merge arrays by `name` (`merge_json`,
  `kubernetes.rs:601`, `is_named_array` `:632`): a user override can rewrite
  any named volume or init container Strøm adds.
- F10. Helm chart: the `dind` sidecar mounts only `dind-storage` and
  `dind-certs` (`helm/stroem/templates/worker-deployment.yaml:151-170`); the
  worker's `workspace-cache` and `/tmp` are not shared with it. Docker bind
  mounts resolve on the sidecar's filesystem. Not verified at runtime (§ 6.3).

Inputs and templates

- F11. `InputFieldDef.field_type` is a `String`
  (`models/workflow.rs:87`); `PRIMITIVE_TYPES` (`template.rs:716`) is the one
  switch between a primitive type and a connection-type reference. An unknown
  type IS a connection type.
- F12. The server does not type-check submitted values and does not enforce
  `required` on the normal creation path (deliberate, `template.rs:690`); only
  `CreationMode::Restart` checks required fields (`job_creator.rs:585-603`).
- F13. Templates render on the server, never on workers. The claim
  (`web/worker_api/jobs.rs:819`) renders the step `input:` (S1,
  `rendering.rs:90`), prepares the action input (`prepare_step_action_input`
  `:127`; `merge_missing_action_fields` `:311` copies job-input values into
  unset same-named action fields) and renders the action body (S2/S3,
  `Scope::ActionBody`). The worker never reads `step.input`.
- F14. `render_context::build` is the single owner of the template context
  (`render_context.rs:181` `Scope`).
- F15. `ClaimRequest` (`jobs.rs:55`) carries `worker_id`, `capabilities`,
  `tags`, `exclusive` — nothing about the worker's file system.
- F16. Lineage: Re-run / Restart replay the source's `raw_input`
  (`source_job_id`); a task retry copies the failed job's resolved `input`
  (`settlement/retry.rs:139`, `retry_of_job_id`, linked in the creation
  transaction); a `type: task` child gets the parent step's merged input
  (`settlement/dispatch.rs:937`, `parent_job_id`); hook jobs carry
  `source_job_id`.
- F17. Upload identity today: `resolve_source_id` (`state_upload.rs:283`)
  yields `user:{email}` / `api_key:{prefix}` / `None` (auth off). API keys
  belong to a user (`008_api_keys.sql:3`, `user_id NOT NULL`).
- F18. MCP: `execute_task` (`mcp/tools.rs:493`) takes `input` as a JSON object.
  `auth.base_url` is required when MCP is enabled **and** an `auth` section
  exists (`config.rs:876`, inside `if let Some(ref auth)`). With no `auth`
  section MCP is open (`mcp/auth.rs` returns no user) and there is no
  `base_url`.
- F19. Input field names have no character rule (`validation.rs`), and the
  server does not run workspace validation at load (F12).

## 4. Model and lifecycle

### 4.1 Declaring a `file` field

`type: file` is a new primitive type: add it to `PRIMITIVE_TYPES` and
`RESERVED_TYPE_NAMES` (`template.rs:713-722`). It is allowed in a **task**
`input` and in an **action** `input`.

Validation (`stroem validate`; the server still does not run workspace
validation at load — F12 / CLAUDE.md):

- rejected with `default`, `options`, `multiple`, `secret`;
- rejected as a connection-type property type (`validation.rs:792` list);
- rejected in an approval action's input (`validate_approval_action`);
- the **name** of a `file` field (task or action) must match
  `^[A-Za-z0-9_][A-Za-z0-9_-]*$` (F19: no rule exists today). The name
  becomes a directory on the worker host, an env value in the pod init
  container and part of the rendered path, so `../x` or `a b` must never get
  there. Because the server does not validate at load, job creation checks
  the same rule for every `file` field of the task, and claim checks it for
  the `file` fields of the step's persisted `action_spec.input`; a failure is
  a value-free 400 / claim failure `input field name is not valid for a file
  field`. No existing workspace has `file` fields, so nothing breaks;
- `stroem validate` **warns** when a task with a `required` `file` field is the
  target of a scheduler trigger, an event-source trigger or a webhook (none of
  them can supply a file, § 4.5).

### 4.2 Upload

`POST /api/workspaces/{ws}/tasks/{task}/files?name=<file name>`

- Body: the raw file bytes (F2 pattern, no multipart). `Content-Type` header is
  stored as sent (default `application/octet-stream`).
- Auth/ACL: **Run** on the task (the `check_run_permission` mapping: View →
  403, Deny → 404). The task must exist in the live workspace and declare at
  least one `file` field, else 400.
- Body limit: `DefaultBodyLimit = input_files.max_file_bytes`.
- The handler **streams** the body to storage (§ 9) while computing size and
  SHA-256. Over the limit → 413, nothing kept.
- Response `201`: `{ file_id, name, size_bytes, sha256, content_type,
  expires_at }`.

**File ID**: `file_` + a UUID v4 in simple form (32 hex). Opaque; never a secret.

**File name** (decision): the request `name` is the *original name* and is
kept for display and download. The step sees a **safe name** derived from it:
the basename only; every character outside `[A-Za-z0-9._-]` becomes `_`; a
leading `.` or `-` becomes `_` (a leading `-` would read as a command-line
option); at most 128 bytes, keeping the extension. Empty, `.` or `..` after
that → 400. Reason: the path is rendered straight into `script:` / `cmd:` /
`args:`; a name such as `x;rm -rf ~.csv` must not become shell syntax in the
author's script, and the pod init script must stay injection-free (§ 6.4).

### 4.3 Storage and tables (migration `054_input_files.sql`)

New tables only; no index on a large existing table, so no `CONCURRENTLY`
pre-run.

- `input_file` — `file_id` PK, `workspace`, `task_name`,
  `uploaded_by_user_id` (UUID NULL, FK user `ON DELETE SET NULL`),
  `origin` (`api` | `mcp_inline` | `mcp_url`), `original_name`, `safe_name`,
  `content_type`, `size_bytes`, `sha256`, `storage_key`, `created_at`,
  `deleting_at` (NULL; set by the sweep, § 4.7).
- `job_input_file` — `job_id` (FK job, `ON DELETE RESTRICT`), `field`,
  `file_id` (FK input_file, `ON DELETE RESTRICT`), PK `(job_id, field)`. One
  row per **task-level** file field of a job. A file can be attached to many
  jobs (Re-run, Restart, retry, children).
- `input_file_upload_token` — `token_sha256` PK, `user_id`, `workspace`,
  `task_name`, `original_name`, `expires_at`, `used_at` (§ 7.5).

Blob key: `{input_files.prefix}{workspace}/{file_id}` (default prefix
`inputs/`). Not per job: the blob is shared by every job that attaches it.
Backend: `input_files.archive` if set, else the artifact archive, else the log
archive (the `main.rs:283-293` fallback chain). No archive configured → the
upload route answers 503 and `type: file` jobs cannot be created; startup logs
one warning.

### 4.4 Accepting a value at job creation

The value of a task-level `file` field in `input` is a file ID string (MCP may
also send an inline object, § 7.5). At creation (`create_job_for_task_inner`)
every task-level `file` field with a value is resolved by exactly one of:

- **A1 — own upload.** The ID names an `input_file` with the job's workspace
  and task, `deleting_at IS NULL`, `created_at` within
  `input_files.upload_ttl_secs`, and `uploaded_by_user_id` = the caller's user
  (an API key and an MCP OAuth token resolve to their user, F17). With auth
  off there are no identities: `uploaded_by_user_id` is NULL and any caller
  matches a NULL-owner upload (everything is open in that mode anyway).
  Uploading once and executing several times within the TTL is allowed.
- **A2 — lineage.** The ID is attached (`job_input_file`) to the job this
  creation derives from: `source_job_id` (Re-run, Restart), `retry_of_job_id`
  (task retry), `parent_job_id` (`type: task` child). The field name may differ
  (a child's field). No uploader or TTL check: the source job's viewers can
  already download it, and the creating caller passed that job's ACL.

Anything else — unknown ID, other user, other task, expired, being deleted, a
non-string value — fails creation with a value-free 400:
`Input field 'data_file': the file is not available` (never the ID, name or
path; § 8). The attachment rows are written in the **creation transaction**
(for a retry, the same transaction as `JobRepo::link_retry_tx`), taking
`FOR SHARE` on each `input_file` row and re-checking `deleting_at IS NULL`
on the locked row (§ 4.7 gives the sweep's side of this protocol).

### 4.5 Required file fields (behaviour difference, decision)

Unlike every other type (F12), a `required` `file` field with no value fails
creation in **every** creation mode: 400 with the word "required" in the
outermost message (`classify_execute_error` → 400); a scheduler fire becomes
`Trigger '…' MISSED: …` with no side effects (§ Git refs trigger rules); a
webhook answers 400; an event-source emit is logged and dropped. Reason: a
step whose file is missing can only fail later at claim with a less useful
message, and U8 accepted "MISSED" for the scheduler case.

### 4.6 Lineage edges

| Edge | Column | File follows? | How |
|---|---|---|---|
| Re-run | `source_job_id` | yes | `raw_input` keeps the ID; A2 |
| Restart | `source_job_id` | yes | `raw_input` replayed; A2 |
| Task retry | `retry_of_job_id` | yes | resolved `input` copied; A2, in the `link_retry_tx` transaction |
| `type: task` child (same or other workspace, U6) | `parent_job_id` | yes | § 5.3 reverse lookup → ID → A2 |
| Agent task-tool child | `parent_job_id` | no (v1) | § 7.6 |
| Hook job | `source_job_id` | no (v1) | `hook.input` shows the stored IDs; a hook action / `type: task` hook whose input has a required `file` field fails creation (logged to the source job) |

Every edge that carries a file is already an edge of the redaction closure
(CLAUDE.md § Git refs → Redaction), so no content leaves the closure that a
viewer of the closure could not already reach.

**Cross-workspace consequence (U6, recorded on purpose):** a file given to a
child job in workspace T can be downloaded by anyone with View on that child
job (job-row ACL: T's task folder). Giving a file to another workspace's task
means giving it to that task's viewers.

### 4.7 Retention and the orphan sweep

- Job retention (`recovery.rs` retention): delete the job's `job_input_file`
  rows in the same transaction as `JobRepo::delete` (after artifacts, as now).
  Never delete a blob here.
- **Orphan sweep** (same leader-gated retention tick, one heartbeat per file):
  an `input_file` with **no** `job_input_file` row and `created_at` older than
  `upload_ttl_secs` is deleted in three steps: (1) in ONE transaction:
  `SELECT … FOR UPDATE` on the `input_file` row, then — in a SEPARATE
  statement, so it reads with a fresh READ COMMITTED snapshot taken after the
  lock was granted — check that no `job_input_file` row exists, and only then
  set `deleting_at`; (2) delete the blob; (3) delete the row.
  A single `UPDATE … WHERE NOT EXISTS (attachment)` is **not** enough: its
  `NOT EXISTS` is evaluated on the snapshot taken before it waits for a
  creation's `FOR SHARE` lock, and PostgreSQL re-checks the condition only
  when the target row itself changed — the attachment insert does not change
  it, so the sweep would mark (and delete) a file a job just attached.
  With `FOR UPDATE` first, a concurrent creation either committed its
  attachment before the lock was granted (the second statement sees it; the
  sweep stops) or blocks on its `FOR SHARE` until the sweep commits, then
  reads the new row version with `deleting_at` set and fails with the
  value-free 400 (§ 4.4). A blob-delete error leaves the row with
  `deleting_at` set; the next sweep retries step 2. Tested with both
  interleavings (§ 13).
- Expired upload tokens (`expires_at` past) are deleted by the same sweep.

## 5. Template values

### 5.1 The rule

- The **stored** value (in `job.input`, `raw_input`, `job_step.input`, hook
  payloads, API responses) is the **file ID**.
- The **template** value of a `file` field is a **path**:
  `{root}/{field}/{safe_name}`, where `field` is the key under which the value
  appears in that context (task field for `input` of the job; action field for
  the prepared action input).
- A `file`-typed **target** field (an action field, or a child task's field)
  accepts either a file ID or a path of the form `{root}/{field}/{safe_name}`
  that names a file attached to **this** job (A3). Inside one job the
  lookup is unambiguous: one file per field, field names are unique keys, and
  the lookup uses the same `root` the render used. Any other value →
  `Input field 'x': the file is not available` (value-free).

So `input: { path: "{{ input.data_file }}" }` gives a `string` action field the
path, and `input: { data: "{{ input.data_file }}" }` gives a `file` action field
the same file (and in the action body `{{ input.data }}` is
`{root}/data/{safe_name}`).

`merge_missing_action_fields` (F13) copies a job-input value into a
same-named, unset action field without looking at its type. For a task-level
`file` field it copies by **target type** (decision): into a `file` action
field, the stored form (the ID — a valid A3 value); into any other action
field, the template form (the path `{claim root}/{task field}/{safe_name}`),
so a script never receives a bare file ID. The path is then in the step's
prepared input like any other string.

The ID → path mapping happens in `render_context::build` (F14), driven by:
the job's `job_input_file` rows for `input` of the job (not the live task
schema, which may have changed since creation); the step's persisted
`action_spec.input` (D10 of the json spec) for the prepared action input. No
caller patches the context afterwards. The `file`-target rule lives in
`template::render_input_typed`, next to the `json` rule.

### 5.2 The inputs root

- Container runners (`runner: docker`, `type: docker`, `runner: pod`,
  `type: pod`): `root = /inputs`.
- `runner: local`: the worker sends `inputs_dir` (an absolute path it will
  create, fresh per claim attempt, e.g. `{tmp}/stroem-inputs-{uuid}`) in
  `ClaimRequest` (new optional field). `root = inputs_dir`.
- Contexts not rendered for a worker (S4 agent prompt, S5 `when` / `for_each`,
  S6 child-task input and approval message, templated refs): canonical
  `root = /inputs`.

`inputs_dir` present in the claim request means "this worker supports file
inputs" (§ 12).

### 5.3 Render table

| Site | `Scope` | `input.<file field>` renders to |
|---|---|---|
| S1 step `input:` at claim | `StepInput` | `{claim root}/{task field}/{name}` |
| S2/S3 action body + image at claim | `ActionBody` | `{claim root}/{action field}/{name}` |
| S4 agent prompt | `AgentPrompt` | `/inputs/{task field}/{name}` (the agent has no file access; informational) |
| S5 `when` / `for_each` | `Condition` | `/inputs/{task field}/{name}`; absent optional field → absent as today (`when: "{{ input.data_file }}"` = "a file was given") |
| S6 `type: task` step input | `ChildTaskInput` | `/inputs/{task field}/{name}` → reverse-looked-up by the child task's `file` field (§ 5.1) |
| S6 approval message | `ApprovalMessage` | `/inputs/{task field}/{name}` |
| Templated refs | `StepRef`, `TaskActionRef` | `/inputs/{task field}/{name}` |
| Hook input | — (payload, not context) | stored ID |

### 5.4 What the claim tells the worker

`ClaimResponse` gains `input_files: [{ field, file_id, safe_name, size_bytes,
sha256 }]`: every `(field, file)` pair the step's S1 and S2/S3 contexts expose
(task-level pairs and prepared-action pairs; the same file under two fields is
two pairs, placed twice). The worker places exactly these; the server rendered
exactly these paths.

## 6. Delivery to runners

### 6.1 Worker download endpoint

`GET /worker/jobs/{job_id}/input-files/{file_id}` — worker token; 404 unless
the file is attached to that job (or to its lineage source for a prepared
action pair — in practice always the job itself, § 4.4); streams the blob
(§ 9) with `Content-Length`. Used by the worker (local / docker) and by the pod
init container.

The worker streams each download to disk and verifies size and SHA-256; a
mismatch or a failed download fails the step before the runner starts (like a
workspace download failure). Retry: a `503` gets the same bounded retry as
pinned tarballs (`download_pinned_with_retry` shape).

### 6.2 `runner: local`

The worker creates `inputs_dir/{field}/{safe_name}` (read-only file mode
`0444`, dirs `0555`) and deletes `inputs_dir` after the step, like the state
temp dirs. If the claim returned `input_files` but the worker sent no
`inputs_dir` (cannot happen for a current worker), it fails the step.

### 6.3 Docker (`runner: docker`, `type: docker`)

The worker downloads into a host temp dir and bind-mounts it at
`/inputs:ro`, in both WithWorkspace and NoWorkspace modes.

**Pre-implementation check (named, blocking for this runner's docs):** verify
F10 in a Helm DinD deployment. If the sidecar cannot see the worker's `/tmp`,
`runner: docker` state, artifacts, the workspace bind mount — and now inputs —
are all affected. A chart fix (a shared volume mounted at the same path in the
worker and the sidecar) is **out of scope**; record the result in TODO.md and
document the limitation in the file-inputs guide if confirmed.

### 6.4 Pods (`runner: pod`, `type: pod`)

- A new init container **`stroem-inputs-init`**, separate from
  `workspace-init`, added only when `input_files` is not empty, in both
  WithWorkspace and NoWorkspace modes. Image: the same curl image as
  `workspace-init`.
- A new `emptyDir` volume **`stroem-inputs`**, mounted read-write in the init
  container and **read-only at `/inputs`** in the main container.
- The init script is a **constant** text: it loops over `STROEM_INPUT_COUNT`
  and, for each `i`, reads `STROEM_INPUT_{i}_FIELD`, `STROEM_INPUT_{i}_NAME`,
  `STROEM_INPUT_{i}_URL` from the environment and runs `curl -sSf --retry 11
  --retry-delay 5 -H "Authorization: Bearer $STROEM_WORKER_TOKEN" -o
  "/inputs/$F/$N" "$U"` (the `workspace_init_script` retry policy; download to
  a file, never a pipe). No user-controlled text is ever interpolated into the
  script. Independently of that, the values in the env are safe by
  construction: file names by § 4.2, field names by the § 4.1 name rule
  (checked at creation and at claim, since the server does not validate at
  load).
- Per the user's standing rule for generated shell: the emitted script is
  **executed** in tests against a wiremock server and in the real curl image,
  not only asserted as text.
- The worker token reaches this init container the same way it reaches
  `workspace-init` (plain env, known exposure, TODO.md) — accepted, not
  widened.
- Both new names are part of the manifest-override surface (F9): a user
  override can rewrite them. Not validated, like `workspace-init`
  (documented).
- The script shape is meant to be reusable by the separate `/state` repair
  (U7).

### 6.5 Old workers

If a claimed step has `input_files` and the claim request has no `inputs_dir`,
the claim fails the step through `fail_claimed_step` with the fixed message
`this step has file inputs, which the worker does not support; upgrade the
worker` (ordinary claim-failure route, `retry:` applies). No release-to-ready
path: a fleet of old workers would loop.

## 7. Clients

### 7.1 UI — Run form

- `type: file` → a file picker. On select, the form uploads immediately (with
  a progress bar; `fetch` has no upload progress, so `XMLHttpRequest`), shows
  the original name and size, and offers Remove / Replace. Run is disabled
  while an upload runs or failed.
- The submitted value is the file ID.
- `ui/src/components/task/constants.ts` gains `file` as a primitive (else it
  is treated as a connection type, F11).

### 7.2 UI — Re-run prefill and job page

- Re-run: the field shows the **source file's** original name and size (from
  the source job's `input_files[]`) and a Replace button. Keeping it sends the
  source's ID (A2); replacing uploads a new file (A1).
- Job detail API gains `input_files: [{ field, file_id, name, size_bytes,
  content_type }]`; the job page shows an "Input files" card with download
  links.
- Download: `GET /api/jobs/{id}/input-files/{field}` — `check_job_acl(&job)`
  (View; Deny → 404), never a live task lookup; headers as for artifacts
  (`nosniff`, sanitized `Content-Disposition`, the artifact inline allow-list,
  everything else attachment).
- Redaction: `input_files[]` goes through the normal job-detail redaction walk
  (names are content; IDs are random and never match a secret). No new skip
  key.

### 7.3 REST API

Two calls: upload (§ 4.2), then execute with `{"input": {"data_file":
"file_…"}}`. `GET /api/workspaces/{ws}/tasks/{name}` shows `type: file`.

### 7.4 CLI

- `stroem-api trigger … --file data_file=./report.csv` (repeatable): uploads
  each file, then executes with the IDs.
- `stroem run` (local, no server): the value of a `file` field is a local
  path; the CLI checks it is a regular file, copies it to a temp
  `{root}/{field}/{safe_name}` (read-only) so paths match the server's shape,
  and mounts that dir at `/inputs` for docker steps. `stroem run` cannot run
  pod steps today; unchanged.

### 7.5 MCP

- **Inline**: in `execute_task`'s `input`, a `file` field may be an object
  `{"name": "report.csv", "content": "<utf-8 text>"}` or `{"name": …,
  "content_base64": "…"}`. Decoded size ≤ `input_files.mcp_inline_max_bytes`
  (default 1 MiB; base64 of 1 MiB ≈ 1.37 MiB fits the `/mcp` route's 2 MiB
  default body limit). The server stores it as an A1 upload by the caller
  (`origin = mcp_inline`) inside the same request, then creates the job.
  Over the cap → tool error naming the field and the cap.
- **Upload URL**: a new tool `create_upload_url { workspace, task_name,
  file_name }` (Run on the task) returns `{ upload_url, method: "POST",
  max_bytes, expires_at }`. `upload_url =
  {auth.base_url}/api/uploads/{token}`. Without `auth.base_url` (only
  possible with no `auth` section, F18) the tool answers a fixed tool error
  `upload URLs need auth.base_url; send the file inline or use the REST
  upload` — no URL is ever built from request headers (the same spoofing
  reason `config.rs:870-875` gives for the OAuth issuer). So a token always
  has a user (`user_id NOT NULL`). `token` = 32 random bytes, stored as SHA-256 only, TTL
  `input_files.upload_url_ttl_secs` (default 900), **single use**. `POST`
  with the raw body and **no** Bearer header (the agent's shell does not have
  the MCP token); the token is the capability. The response is the § 4.2
  response; `uploaded_by_user_id` = the token's user, `origin = mcp_url`.
  Then the agent calls `execute_task` with the `file_id` (A1).
- `get_task` shows `type: file`; the tool descriptions say how to send files.

### 7.6 Agent task tools, webhooks, triggers, event sources (not in v1)

- Agent task tools (`stroem-agent/src/tools.rs:137` `map_field_type`): an
  optional `file` field is left out of the tool schema; a task with a required
  `file` field cannot be an agent tool (`stroem validate` error; at runtime
  the tool call fails creation per § 4.5).
- Webhooks, scheduler triggers, event sources: cannot supply a file; § 4.5
  decides the required case; an optional field is simply absent.

## 8. Errors

All new messages follow CLAUDE.md § Secrets in logs: they name the **input
field** and a fixed kind, never the ID, the name, the path or a rendered
value.

| Situation | Message |
|---|---|
| A1/A2/A3 failure | `Input field '{f}': the file is not available` |
| Required file missing | `Input field '{f}': a file is required` |
| Bad name | `invalid file name` (upload, 400) |
| Too large | `file is larger than {max} bytes` (upload, 413) |
| MCP inline over cap | `Input field '{f}': inline content is larger than {cap} bytes` |
| Old worker | § 6.5 fixed text |
| No archive | `file storage is not configured` (503) |

The reverse lookup (§ 5.1) is a post-render check, not a new render path; it
reports only the field name and so stays within the tera-2 spec § 3.4 rules.
Cross-workspace owner-side withholding (CLAUDE.md § Cross-Workspace
References) is unchanged: a file field error on the owner side is withheld by
the same origin rule as any other.

## 9. Streaming and memory

Decision: no path holds a whole file in memory.

- `LocalBlobArchive` and `S3BlobArchive` get real `put_stream` / `get_stream`
  overrides. S3 upload uses a multipart upload with a bounded part buffer
  (8 MiB default; the request may be chunked, so the length is not known in
  advance). Local writes to a temp file, fsync, rename (as `put` does now).
- The upload handler, the worker download endpoint and the user download
  endpoint stream end to end; the worker streams to disk.
- **Enforcement**: cases in `stroem-server/tests/log_peak_alloc_test.rs` (the
  existing counting-allocator binary, `OWN_BINARY` allow-list) for upload and
  both downloads of a 50 MiB file: measured peak ≤ a stated bound (part
  buffer × 2 + fixed overhead), the pattern the user approved for log tails.
  If that binary's name becomes misleading, renaming it is the builder's call
  (update the guard's allow-list).

## 10. Configuration

```yaml
input_files:
  max_file_bytes: 104857600      # 100 MiB (U3)
  upload_ttl_secs: 86400         # unattached uploads
  upload_url_ttl_secs: 900       # MCP upload URLs
  mcp_inline_max_bytes: 1048576  # decoded
  prefix: "inputs/"
  archive: { … }                 # optional; falls back as in § 4.3
```

Env overrides follow `STROEM__INPUT_FILES__…`. Caps validated in
`ServerConfig::validate` (positive; `mcp_inline_max_bytes` ≤ 1.4 MiB so the
base64 form fits the `/mcp` 2 MiB body limit).

## 11. Touch points

| Area | Change |
|---|---|
| `template.rs` `PRIMITIVE_TYPES`, `RESERVED_TYPE_NAMES` | add `file` |
| `validation.rs` | § 4.1 rules; connection-type property list stays without `file` |
| `template::render_input_typed` | `file`-target rule (§ 5.1) |
| `template::merge_defaults` / `merge_action_defaults` | no default possible; no branch needed beyond rejecting at validation |
| `template::resolve_connection_inputs*`, `resolve_rerun_sentinels` | nothing (primitive) — confirm with tests |
| `render_context::build` | ID → path mapping, root per § 5.2 |
| `job_creator::create_job_for_task_inner` | § 4.4 acceptance, attachment rows, § 4.5 required check, MCP inline storage |
| `settlement/retry.rs::create_retry_job` | attachment in the `link_retry_tx` transaction (A2) |
| `settlement/dispatch.rs::handle_task_steps_pass` | bucket C reverse lookup; child attachment (A2 via parent) |
| `settlement/hooks.rs` | required-file failure path (§ 4.6) |
| `web/worker_api/jobs.rs` claim | `inputs_dir`, `input_files` in the response, § 6.5 |
| `web/worker_api/rendering.rs` | A3 resolution in `prepare_step_action_input` |
| new `web/api/input_files.rs` | upload, user download, `/api/uploads/{token}` |
| new `web/worker_api/input_files.rs` | worker download |
| `web/api/jobs.rs` job detail | `input_files[]` |
| `blob_storage.rs` | real streaming (§ 9) |
| `recovery.rs` retention | § 4.7 |
| `stroem-db` | migration 054, `InputFileRepo`, `JobInputFileRepo`, token repo |
| `stroem-worker` | `inputs_dir`, streaming download + verify, placement, cleanup |
| `stroem-runner` | Docker `/inputs:ro`; Kube `stroem-inputs-init` + volume; Shell nothing |
| `stroem-cli` | `stroem run` § 7.4, `stroem validate` § 4.1, `stroem-api trigger --file` |
| `mcp/tools.rs` | inline objects, `create_upload_url` |
| `stroem-agent/src/tools.rs` | § 7.6 |
| UI | `constants.ts`, file field widget, Re-run prefill, job page card, API client |
| `metrics.rs` | at least `stroem_input_file_uploads_total{origin,result}` and `stroem_input_file_bytes_total` (+ test, ops doc) |

## 12. Rollout and compatibility

- An old server replica treats `type: file` as a **connection type** (F11) and
  answers 400 on such a task. So: **every server replica first, then every
  worker, then YAML with `type: file`.** Same pattern as the templated-refs
  and `ref:` rollout notes.
- Migration 054 adds tables only.
- No existing behaviour changes for tasks without `file` fields. The § 4.5
  required check applies only to `file` fields.

## 13. Testing

Per CLAUDE.md § Mandatory Test Coverage, at least:

- unit: safe-name derivation (table of hostile names), ID/path classification,
  render-table rows (§ 5.3) per `Scope`, reverse lookup (ID, path, foreign
  path, other job's path), validation rules (§ 4.1), MCP inline decode + cap;
- stroem-db integration: migration 054 shape, attach/lineage queries, the
  orphan sweep race (creation vs sweep) with both orders;
- server integration: upload ACL (Run / View / Deny, auth off), body limit,
  A1 (other user, other task, expired), A2 for Re-run / Restart / retry /
  child (same and other workspace), required check per creation mode incl.
  scheduler MISSED, claim response pairs, old-worker failure, job detail +
  download ACL on the job row, retention + sweep, upload-URL token single use
  and expiry, metrics;
- runner: Docker mount in both modes; Kube spec (init container, volume,
  read-only mount, both modes, manifest-merge interaction) and the **executed**
  init script against wiremock and the real curl image;
- allocation: § 9;
- UI: Vitest for the field widget state and Re-run prefill; Playwright upload
  → run → download;
- E2E (`tests/e2e.sh`): a new section — upload, execute, a local step and a
  docker step that read the file and print its SHA-256, Re-run without upload.

## 14. Documentation

- New guide `docs/src/content/docs/guides/file-inputs.md` (YAML, UI, API,
  CLI, MCP, runners, limits, the safe-name rule, the cross-workspace
  consequence, rollout order); config reference; API reference; MCP page;
  `llms.txt` generator `sections`.
- CLAUDE.md: a `### File Inputs` section (rules of § 4.4, § 5.1, § 6.4, § 9);
  correct the § Artifacts claims that are wrong today (F1: artifact transfers
  do not stream; inline disposition allow-list is png/jpeg/gif/webp/text/plain,
  not PDF/Markdown).
- `docs/internal/stroem-v2-plan.md` status; `docs/internal/TODO.md` entries
  (§ 15).

## 15. Non-goals and follow-ups (TODO.md)

- Pod `/state` and `/global-state` are always empty; the task-state guide says
  otherwise (U7, separate task; can reuse § 6.4's init container shape).
- Helm DinD volume sharing (F10), result of the § 6.3 check.
- Files from webhooks (multipart), scheduler triggers, event sources, hooks,
  agent task tools.
- More than one file per field; file name / size / content type in templates.
- Using an artifact of one job as the file input of another.
- Secret masking of file content.
- Worker token as a plain env value in pod specs (existing).
- Kube artifacts (existing).

## 16. Left to the builder

- Exact Rust module and repo names, DB column types and index choices beyond
  § 4.3.
- S3 multipart details (part size within the § 9 bound), the reqwest
  streaming code.
- UI layout of the file field and the "Input files" card.
- Metric names beyond the two in § 11.
- Whether `stroem run` copies or hard-links the file (§ 7.4), as long as the
  path shape and read-only mode match.
