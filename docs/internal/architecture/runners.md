# Actions, runners and step claiming

Action types, the Shell/Docker/Kube runners, and how workers claim steps by capability and tag. `AGENTS.md` indexes this file. A `§ <Title>` reference names a `###` section in any file of `docs/internal/architecture/`.

### Workflow YAML structure
See `docs/internal/stroem-v2-plan.md` Section 2 for the full YAML format.

### Action Types and Runners
- **`agent`**: LLM call as a workflow step, worker-side dispatch. Supports structured output via `output` (converted to JSON Schema).
- **`docker` / `pod`** (container actions): Runs user's prepared image as-is, no workspace mounting. Uses `cmd` field for entrypoint/command override.
- **`script`**: `type: script` + `runner: local|docker|pod` — scripts with workspace files. Languages: `shell` (default), `python`, `javascript`, `typescript`, `go`. Uses `script` (inline) or `source` (file path) fields. Optional `dependencies`, `interpreter`, and `args` (CLI arguments, Tera-templated) fields.
- **`task`**: References another task, server creates a child job (see § Task Actions)
- **`approval`**: Pauses execution for human approve/reject (see § Approval Gates)
- `type: script` + `image` is **rejected** by validation. Use `type: docker` or `type: script` + `runner: docker` instead.
- **Toolchain preferences**: `uv > python3 > python`, `bun > node` (JS), `bun > deno` (TS), `bash > sh`
- **Pod manifest overrides**: `type: pod` and `runner: pod` support a `manifest` field deep-merged into the generated pod spec. See `docs/src/content/docs/guides/action-types.md`.

### Runner Architecture
- `RunnerMode` enum: `WithWorkspace` (script actions) or `NoWorkspace` (docker/pod actions)
- `StepExecutor::select_runner()` dispatches on `(action_type, runner_field)`:
  - `("script", "local")` → ShellRunner, `("script", "docker")` or `("docker", _)` → DockerRunner, `("script", "pod")` or `("pod", _)` → KubeRunner
- **DockerRunner**: `WithWorkspace` bind-mounts at `/workspace:ro`; `NoWorkspace` runs standalone. Before its first request it negotiates the Docker API version down to the daemon's (`ensure_api_version`, once per runner; a failure is retried on the next step, never fatal) — bollard otherwise sends its own newest version (1.53 in 0.21) and an older daemon rejects every call.
- **KubeRunner**: `WithWorkspace` uses init container + workspace volume; `NoWorkspace` runs directly. The init container fetches the tarball itself, so it must ask for `?revision=` = `RunConfig.workspace_revision` (the claim's `revision`, set by the worker's `build_run_config`) — without it the server serves the LIVE workspace (prod 2026-10-12: `ref:` steps ran the default branch's files). It downloads to a file with `curl --retry` (12 attempts, 5 s apart, like the worker's `PINNED_503_ATTEMPTS`), then `tar` — never `curl | tar`, whose status is tar's and whose retries would append to the stream (`workspace_init_script`). A manifest override of `initContainers[name=workspace-init].command` replaces all of this and is not validated (TODO.md).
- **Pod names and labels**: `KubeRunner::pod_name` (≤ 63 chars, a Strøm policy; trailing dash trimmed; an FNV-1a hash suffix when truncated, which lowers but does not rule out collisions) and `KubeRunner::step_labels` (job-id/step/task values through `sanitize_label_value`). A `for_each` instance is named `step[i]`, and `[`/`]` in a label value make the API server reject the pod (prod 2026-09-24). The raw step name goes in the `stroem.io/step-name` annotation, set AFTER the manifest merge so Strøm's value wins. Manifest `metadata` overrides are not validated and can still replace the default labels. Nothing in Strøm selects pods by label.
- **Startup scripts**: `docker/entrypoint.sh` sources `*.sh` from `/etc/stroem/startup.d/`. DockerRunner bind-mounts this; KubeRunner uses ConfigMap via `runner_startup_configmap`.

### Capabilities, Tags, and Step Claiming
Post-042 (`042_worker_exclusive.sql`): two routing axes with affinity semantics on `tags`, plus an optional exclusive flag for reserved workers.
- **`capabilities`** on worker — runners supported. Any of `"script"`, `"docker"`, `"kubernetes"`, `"agent"`. Required. Matched against step's `required_ability` (derived from action type + runner).
- **`tags`** on worker — free-form **affinity labels**. A step's `required_tags` MUST be a subset of these for the worker to claim it. Empty = no affinity axis.
- **`exclusive`** on worker — bool (default `false`). When `true`, the worker also refuses any step whose `required_tags` don't cover its own `tags` — the "reserved worker" pattern.
- Claim SQL: `worker.capabilities @> to_jsonb(required_ability) AND required_tags::jsonb <@ worker.tags AND (NOT worker.exclusive OR worker.tags <@ required_tags::jsonb)`, plus `action_type NOT IN ('task','approval','event_source')` — the server-dispatched kinds. **`agent` must not be in that list** (worker-side since a8aa1c6; ed67c76 regressed it until 044). The unmatched-step sweep keeps the claim's `NOT IN` list (plus `action_type <> 'deferred'`, templated refs). Measured 2026-10-16: neither query uses the GIN index `idx_job_step_ready_claim` (GIN `jsonb_ops` cannot serve `<@`); the claim uses `idx_job_step_ready_ability` and the unmatched sweep `idx_job_step_ready_at` (both partial `status = 'ready'`) — pinned by stroem-db `templated_refs_test::claim_and_unmatched_sweep_still_use_a_ready_partial_index` (asserts a partial ready index named `idx_job_step_ready_*` and no seq scan; TODO.md). A claim request carrying a capability outside `KNOWN_ABILITIES` is a 400 (a deferred step's `required_ability` is `deferred`).
- User pattern "reserve worker X for step Y": worker `tags: ["some-label"], exclusive: true` + action `tags: ["some-label"]`. Untagged steps never leak onto worker X.
- Behaviour flip from 041: an untagged step (`required_tags: []`) NO LONGER reaches a tagged non-exclusive worker's *tagged* peers — a step needs `required_tags: ["claude"]` to be routed to a `tags: ["claude"]` worker. Under 041 a permissive (empty-tag) worker could still snap up any step; that leak is gone.
- Recovery's unmatched-step sweep mirrors the claim SQL (`get_unmatched_ready_steps`).
