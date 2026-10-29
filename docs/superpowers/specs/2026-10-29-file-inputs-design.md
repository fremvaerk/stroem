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
- **Revision 3** (2026-10-29) — the user decided U9: every runner step gets
  every file of its job; consequences recorded in § 4.6 and § 5.4; agent
  steps get none (author's call).
- **Revision 4** (2026-10-29) — Codex spec review round 1 (6 findings, all
  verified against the code and fixed): (1) the path is now
  `{root}/{file_id}/{safe_name}` — a field-keyed path let a task field and an
  action field of the same name name one path for two files (§ 5.1, § 5.3,
  § 5.4, § 6.2); the revision-2 field-name rule is removed, since field names
  no longer reach a path; (2) `input_files[]` keeps `field` / `file_id` as
  redaction identifiers and Re-run reads IDs only from there (§ 7.2);
  (3) `FileInputError` is a typed 400, classified before the `"is not
  available"` → 500 check, and the message no longer uses that phrase (§ 4.4,
  § 8); (4) the pod init script creates `/inputs/$ID` before the download
  (§ 6.4); (5) the pod init script verifies size and SHA-256 (§ 6.4);
  (6) `stroem run` stays local-script-only (§ 7.4).
- **Revision 5** (2026-10-29) — Codex spec review round 2 (round-1 findings
  all confirmed fixed; 3 new findings, all verified and fixed): (1) a pure
  trigger preflight runs after target resolution and BEFORE the scheduler's
  concurrency policy, so a required `file` field makes the fire `MISSED`
  without a skipped row or a cancellation (§ 4.5); A1 is now limited to
  creations a caller starts directly (§ 4.4); (2) uploads accept
  `source_job_id` and reuse the Re-run authorisation and pinned-source
  resolution, so a pinned Re-run can replace a file (§ 4.2, § 7.2); (3) retry
  A2 checks the job that just failed (`CreationMode::Retry.failed_job_id`),
  not the chain root (§ 4.4, § 4.6); the § 4.6 closure sentence now says
  what is true (a file follows only existing input-copy edges; content is
  never redacted).

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
`/inputs/file_7f3a91c2…/report.csv` (§ 5.1). The file is read-only in the
step.

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
| U9 | **Every runner step gets every file of its job** (chosen over "only the files the step uses" and "all, except cross-workspace actions"), with the consequences in § 5.4. |

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
- no rule on the field NAME is needed (F19): since revision 4 a field name
  never reaches a file system path, a pod env value or a script — paths are
  built from the file ID (§ 5.1);
- `stroem validate` **warns** when a task with a `required` `file` field is the
  target of a scheduler trigger, an event-source trigger or a webhook (none of
  them can supply a file, § 4.5).

### 4.2 Upload

`POST /api/workspaces/{ws}/tasks/{task}/files?name=<file name>[&source_job_id=<id>]`

- Body: the raw file bytes (F2 pattern, no multipart). `Content-Type` header is
  stored as sent (default `application/octet-stream`).
- Auth/ACL, which task schema decides — the same two paths as execute:
  - **without `source_job_id`**: **Run** on the LIVE task (the
    `check_run_permission` mapping: View → 403, Deny → 404); the live task
    must declare at least one `file` field, else 400.
  - **with `source_job_id`** (a replacement file for a Re-run; Restart takes
    no new input, `RestartJobRequest` at `web/api/jobs.rs:653`, and only
    replays the source's files through A2):
    the Re-run authorisation sequence, reused as is — the source must be a
    top-level job of this `{ws}` and `{task}` (`is_top_level_job`, else 400);
    `check_job_acl` on the SOURCE job (Deny → 404); a pinned source's ref is
    re-resolved with `pinned_source::resolve_source_pin`
    (`web/api/pinned_source.rs:44`) and the selected task — at that commit
    for a pinned source, live otherwise — gets `require_task_run`
    (`web/api/tasks.rs:594`: Deny → 404, View → 403) and must declare a
    `file` field, else 400. So a pinned Re-run whose task exists only at its
    ref, or whose `file` field exists only there, can replace a file
    (`web/api/tasks.rs:472-505` is the execute-side precedent).

  The row records `(workspace, task_name)` either way, which is all A1
  (§ 4.4) matches on; the execute call re-resolves and re-checks on its own.
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
  **A1 is available only to creations a caller starts directly**: execute
  (UI, REST API, `stroem-api`), MCP `execute_task`, Re-run and Restart.
  Scheduler, webhook, event-source, hook, `type: task`, agent-tool and retry
  creations never resolve a value through A1 — a trigger or a hook cannot
  hold an upload (U8), even a NULL-owner one with auth off.
- **A2 — lineage.** The ID is attached (`job_input_file`) to the job this
  creation derives from, named by the creation itself, never re-derived from
  a column of the new row: `source_job_id` (Re-run, Restart); for a task
  retry, the job that **just failed** — `CreationMode::Retry.failed_job_id`,
  whose input the retry copies (`settlement/retry.rs:139`, `:144-147`) — NOT
  `retry_of_job_id`, which names the chain's root (`:123`); `parent_job_id`
  (`type: task` child). The field name may differ (a child's field). No
  uploader or TTL check: the source job's viewers can already download it,
  and the creating caller passed that job's ACL (a retry has no caller; its
  input is the failed job's own).

Anything else — unknown ID, other user, other task, expired, being deleted, a
non-string value — fails creation with a value-free 400:
`Input field 'data_file': the file cannot be used` (never the ID, name or
path; § 8). It is a typed, value-free leaf error (`FileInputError`) that
`classify_execute_error` downcasts to `BadRequest` BEFORE its phrase tiers —
the generic `"is not available"` check (`web/api/mod.rs:457-460`) would
otherwise turn it into a 500, which is also why the sentence avoids that
phrase. The attachment rows are written in the **creation transaction**
(for a retry, the same transaction as `JobRepo::link_retry_tx`), taking
`FOR SHARE` on each `input_file` row and re-checking `deleting_at IS NULL`
on the locked row (§ 4.7 gives the sweep's side of this protocol).

### 4.5 Required file fields (behaviour difference, decision)

Unlike every other type (F12), a `required` `file` field with no value fails
creation in **every** creation mode: 400 with the word "required" in the
outermost message (`classify_execute_error` → 400); a webhook answers 400; an
event-source emit is logged and dropped. Reason: a step whose file is missing
can only fail later at claim with a less useful message, and U8 accepted
"MISSED" for the scheduler case.

**Trigger preflight (before any concurrency policy).** The scheduler applies
its concurrency policy BEFORE it creates the job: `skip` writes a skipped row
and `cancel_previous` cancels the running jobs (`scheduler.rs:434-455`,
`:485-496`), and only then `create_target_job` runs (`:530-542`). A
creation-time failure would therefore come after those side effects. So a
pure check, `file_inputs::trigger_preflight(target task, trigger input)`,
runs right after `trigger_target::resolve_trigger_target`
(`scheduler.rs:418-430`, whose failure branch already logs `MISSED`) and
before the policy, on the resolved target's task schema (live or pinned): it fails when
the target task has a `required` `file` field, or when the trigger's `input:`
gives a value to any `file` field (a trigger cannot hold an upload, § 4.4
A1). A failure takes the existing resolution-failure branch: `Trigger '…'
MISSED: …`, no skipped row, no cancellation, not replayed. The webhook calls
the same preflight after its own target resolution (→ 400 via
`classify_execute_error`, before any job row). Creation-time validation stays
as the backstop. Event sources have no concurrency policy (only
`max_in_flight` backpressure), so the creation-time check suffices there.

### 4.6 Lineage edges

| Edge | Column | File follows? | How |
|---|---|---|---|
| Re-run | `source_job_id` | yes | `raw_input` keeps the ID; A2 |
| Restart | `source_job_id` | yes | `raw_input` replayed; A2 |
| Task retry | `CreationMode::Retry.failed_job_id` (not `retry_of_job_id`, the root) | yes | the failed job's resolved `input` copied; A2 against that job, in the `link_retry_tx` transaction |
| `type: task` child (same or other workspace, U6) | `parent_job_id` | yes | § 5.3 reverse lookup → ID → A2 |
| Agent task-tool child | `parent_job_id` | no (v1) | § 7.6 |
| Hook job | `source_job_id` | no (v1) | `hook.input` shows the stored IDs; a hook action / `type: task` hook whose input has a required `file` field fails creation (logged to the source job) |

Every edge that carries a file is an edge along which the job INPUT is
already copied today (`raw_input` for Re-run / Restart, the failed job's
`input` for a retry, the rendered step input for a child): a file ID travels
exactly where its input value already travels, and no new copy path between
jobs is added. File CONTENT is never redacted (U8), so the redaction closure
(CLAUDE.md § Git refs → Redaction) is not extended; the `input_files[]`
names and content types a job shows are redacted with that job's own set
(§ 7.2).

**Cross-workspace consequences (U6, U9, recorded on purpose):**

- a file given to a child job in workspace T can be downloaded by anyone
  with View on that child job (job-row ACL: T's task folder). Giving a file
  to another workspace's task means giving it to that task's viewers;
- a step that runs another workspace's action receives **all** files of the
  job (§ 5.4), so that workspace's script can read them. Using another
  workspace's action in a task with `file` fields means trusting that
  action with those files.

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
  `{root}/{file_id}/{safe_name}`. The path is a function of the FILE only,
  never of the field that holds it (revision 4): the job input (S1) and the
  prepared action input (S2/S3) are different contexts
  (`render_context.rs:285-297`), so a field-keyed path let task field `a` and
  action field `a` name the same path for two different files. With the ID in
  the path, one file has one path in every context of a step, and two files
  never share one. `file_id` is `file_` + 32 hex, so the path segment is safe
  by construction.
- A `file`-typed **target** field (an action field, or a child task's field)
  accepts either a file ID or a path `{root}/{file_id}/{safe_name}` whose
  `root` is the root of that render and whose `file_id` is attached to
  **this** job (A3). The lookup is by ID, so it is unambiguous. Any other
  value → `Input field 'x': the file cannot be used` (`FileInputError`,
  value-free, § 8).

So `input: { path: "{{ input.data_file }}" }` gives a `string` action field the
path, and `input: { data: "{{ input.data_file }}" }` gives a `file` action field
the same file; in the action body `{{ input.data }}` is the same path.

`merge_missing_action_fields` (F13) copies a job-input value into a
same-named, unset action field without looking at its type. For a task-level
`file` field it copies by **target type** (decision): into a `file` action
field, the stored form (the ID — a valid A3 value); into any other action
field, the template form (the path `{claim root}/{file_id}/{safe_name}`),
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
| S1 step `input:` at claim | `StepInput` | `{claim root}/{file_id}/{name}` |
| S2/S3 action body + image at claim | `ActionBody` | `{claim root}/{file_id}/{name}` |
| S4 agent prompt | `AgentPrompt` | `/inputs/{file_id}/{name}` (the agent has no file access; informational) |
| S5 `when` / `for_each` | `Condition` | `/inputs/{file_id}/{name}`; absent optional field → absent as today (`when: "{{ input.data_file }}"` = "a file was given") |
| S6 `type: task` step input | `ChildTaskInput` | `/inputs/{file_id}/{name}` → looked up by ID for the child task's `file` field (§ 5.1) |
| S6 approval message | `ApprovalMessage` | `/inputs/{file_id}/{name}` |
| Templated refs | `StepRef`, `TaskActionRef` | `/inputs/{file_id}/{name}` |
| Hook input | — (payload, not context) | stored ID |

### 5.4 What the claim tells the worker

`ClaimResponse` gains `input_files: [{ file_id, safe_name, size_bytes,
sha256 }]`: every distinct file the step's S1 and S2/S3 contexts expose
(task-level and prepared-action files, de-duplicated by ID — one file is one
entry and one path, whatever fields hold it). The worker places exactly
these at `{root}/{file_id}/{safe_name}`; the server rendered exactly these
paths.

**Every runner step gets every file of its job (U9, the user's decision
after the builder check).** The S1 context holds the whole job input, so
every task-level file is a pair of every claimed step, whether or not the
step uses it. Consequences, accepted:

- a **cross-workspace action** step (owner ≠ `job.workspace`) receives all
  of the caller job's files, not only the ones given to its `file` fields;
  the owner's script can list `/inputs` and read them (§ 4.6);
- every `for_each` instance downloads every file (one pod init container
  per instance pod); the guide says so;
- § 6.5 (old worker) applies to every runner step of a job that has a file.

Agent steps (`type: agent`) have no runner and no file system for the step;
they receive no `input_files` and never fail § 6.5 (author's call).

## 6. Delivery to runners

### 6.1 Worker download endpoint

`GET /worker/jobs/{job_id}/input-files/{file_id}` — worker token; 404 unless
the file is attached to that job (every file a step can expose is, § 4.4,
§ 5.1 A3); streams the blob
(§ 9) with `Content-Length`. Used by the worker (local / docker) and by the pod
init container.

The worker streams each download to disk and verifies size and SHA-256; a
mismatch or a failed download fails the step before the runner starts (like a
workspace download failure). Retry: a `503` gets the same bounded retry as
pinned tarballs (`download_pinned_with_retry` shape).

### 6.2 `runner: local`

The worker creates `inputs_dir/{file_id}/{safe_name}` (read-only file mode
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
- The init script is a **constant** text run with `set -eu`. It loops over
  `STROEM_INPUT_COUNT` and, for each `i`, reads `STROEM_INPUT_{i}_ID`,
  `_NAME`, `_URL`, `_SIZE` and `_SHA256` from the environment, then:
  1. `mkdir -p "/inputs/$ID"` (the `emptyDir` starts empty and curl creates
     no parent directories);
  2. `curl -sSf --retry 11 --retry-delay 5 -H "Authorization: Bearer
     $STROEM_WORKER_TOKEN" -o "/inputs/$ID/$NAME" "$URL"` (the
     `workspace_init_script` retry policy; download to a file, never a pipe);
  3. checks the byte count (`wc -c`) equals `$SIZE` and the `sha256sum` of
     the file equals `$SHA256`.

  Any failing step exits non-zero, so the init container fails, the pod
  never starts its main container and the step fails (the existing pod-failure
  path). The pod's copy is downloaded on the pod's node, which the worker
  cannot see (`kubernetes.rs:195-199`), so this check is the ONLY integrity
  check of that copy; it is the pod-side equivalent of § 6.1. `wc` and
  `sha256sum` must exist in the configured curl image (busybox provides both
  in the default `curlimages/curl`); the real-image test proves it.
  No user-controlled text is ever interpolated into the script, and every env
  value is safe by construction: IDs are `file_` + hex, names follow § 4.2,
  size and digest are server-computed.
- Per the user's standing rule for generated shell: the emitted script is
  **executed** in tests — against a wiremock server, in the real curl image,
  starting from an EMPTY `/inputs` — including a size mismatch, a digest
  mismatch and a 404, each of which must fail the script; not only asserted
  as text.
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
  source's ID (A2), taken from `input_files[]` by field — **never** from
  `raw_input` / `input`, whose strings the redaction walk may mask (a secret
  that is a substring of an ID, or `MaskAll`); replacing uploads a new file
  with `?source_job_id=` (§ 4.2, so a pinned source's task is the one checked)
  and sends its ID (A1). Restart takes no input (`RestartJobRequest`); it
  replays the source's files (A2).
- Job detail API gains `input_files: [{ field, file_id, name, size_bytes,
  content_type }]`; the job page shows an "Input files" card with download
  links.
- Download: `GET /api/jobs/{id}/input-files/{field}` — `check_job_acl(&job)`
  (View; Deny → 404), never a live task lookup; headers as for artifacts
  (`nosniff`, sanitized `Content-Disposition`, the artifact inline allow-list,
  everything else attachment).
- Redaction: the walker redacts every string of a job-detail key that is not
  an identifier key (`redaction.rs:593-610`), and `MaskAll` replaces every
  string (`redaction.rs:553-555`); a secret that is a substring of a
  `file_…` ID would also be span-masked inside it. So `input_files[]` is
  walked like `steps[]`: each entry keeps its identifier keys `field` and
  `file_id` intact (a new `INPUT_FILE_IDENTIFIER_KEYS`, applied in
  `walk_job_content` next to `STEP_IDENTIFIER_KEYS`), and `name` and
  `content_type` are redacted content. `size_bytes` is a number (never
  masked). Re-run prefill and download links read only those two
  identifiers. Test: a secret equal to `file` and a `MaskAll` job both leave
  `field` / `file_id` intact.

### 7.3 REST API

Two calls: upload (§ 4.2), then execute with `{"input": {"data_file":
"file_…"}}`. `GET /api/workspaces/{ws}/tasks/{name}` shows `type: file`.

### 7.4 CLI

- `stroem-api trigger … --file data_file=./report.csv` (repeatable): uploads
  each file, then executes with the IDs.
- `stroem run` (local, no server): the value of a `file` field is a local
  path; the CLI checks it is a regular file and copies it to a temp
  `{root}/{local id}/{safe_name}` (read-only) so paths have the server's
  shape. `stroem run` runs only `type: script` with `runner: local`
  (`stroem-cli/src/local/run.rs:168-199`, `ShellRunner` only at `:211`) and
  that scope is unchanged — no Docker or pod support is added here.

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
| A1/A2/A3 failure | `Input field '{f}': the file cannot be used` (`FileInputError`) |
| Required file missing | `Input field '{f}': a file is required` |
| Bad name | `invalid file name` (upload, 400) |
| Too large | `file is larger than {max} bytes` (upload, 413) |
| MCP inline over cap | `Input field '{f}': inline content is larger than {cap} bytes` |
| Old worker | § 6.5 fixed text |
| No archive | `file storage is not configured` (503) |

**Classification.** The creation-time file errors (A1/A2 failure, required
file missing, MCP inline over cap) are one typed leaf, `FileInputError`
(fixed kind + field name, no `source`), which `classify_execute_error`
(`web/api/mod.rs`) downcasts to `BadRequest` BEFORE the `"is not available"`
check at `:457-460` and the phrase tiers — like `ReplayFieldsError` at
`:454-456`. Webhook target errors already go through that classifier
(CLAUDE.md § Git refs → Triggers), so a webhook answers 400; the scheduler
records `MISSED` and the event source logs and drops, whatever the class.
Regression tests: unknown, expired, other-user and other-task
IDs each answer 400 on execute.

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
| `settlement/retry.rs::create_retry_job` | attachment in the `link_retry_tx` transaction (A2 against `failed_job_id`) |
| `trigger_target.rs` / `scheduler.rs` / `web/hooks.rs` | `trigger_preflight` after target resolution, before the concurrency policy (§ 4.5) |
| `settlement/dispatch.rs::handle_task_steps_pass` | bucket C reverse lookup; child attachment (A2 via parent) |
| `settlement/hooks.rs` | required-file failure path (§ 4.6) |
| `web/worker_api/jobs.rs` claim | `inputs_dir`, `input_files` in the response, § 6.5 |
| `web/worker_api/rendering.rs` | A3 resolution in `prepare_step_action_input` |
| new `web/api/input_files.rs` | upload (live and `source_job_id` modes, § 4.2), user download, `/api/uploads/{token}` |
| new `web/worker_api/input_files.rs` | worker download |
| `web/api/jobs.rs` job detail | `input_files[]` |
| `redaction.rs` | `INPUT_FILE_IDENTIFIER_KEYS` (`field`, `file_id`) walked in `walk_job_content` (§ 7.2) |
| `web/api/mod.rs::classify_execute_error` | `FileInputError` → 400 before the `"is not available"` check (§ 8) |
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
  path, other job's path, path with another root), validation rules (§ 4.1),
  MCP inline decode + cap, `FileInputError` classification (§ 8), redaction
  of `input_files[]` with a secret equal to `file` and under `MaskAll`
  (§ 7.2);
- the revision-4 collision case: task fields `a` and `b` hold two different
  uploads both named `report.csv`, and action field `a` receives
  `{{ input.b }}` — S1 and S2/S3 render two different paths, the claim lists
  two files, and each path holds its own content;
- stroem-db integration: migration 054 shape, attach/lineage queries, the
  orphan sweep race (creation vs sweep) with both orders;
- server integration: upload ACL (Run / View / Deny, auth off), body limit,
  upload with `source_job_id` for a pinned source whose task exists only at
  its ref (accepted) and for a source the caller may not view (404), A1
  (other user, other task, expired, and a trigger / hook creation that names
  a valid upload — rejected), A2 for Re-run / Restart / retry / child (same
  and other workspace), the retry chain after a schema change: a second
  retry after the TTL expired still accepts the file attached to the FIRST
  retry (A2 against `failed_job_id`, not the root), required check per
  creation mode, the trigger preflight with `concurrency: skip` and
  `cancel_previous` (MISSED, no skipped row, nothing cancelled), webhook
  preflight 400, claim response files, old-worker failure, job detail +
  download ACL on the job row, retention + sweep, upload-URL token single use
  and expiry, metrics;
- runner: Docker mount in both modes; Kube spec (init container, volume,
  read-only mount, both modes, manifest-merge interaction) and the **executed**
  init script against wiremock and the real curl image, from an empty
  `/inputs`, with size-mismatch, digest-mismatch and 404 cases (§ 6.4);
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
