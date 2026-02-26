# Strøm v2 -- Project Plan

## Context

Strøm is a unified workflow/task orchestration platform replacing RunDeck, Windmill, CircleCI, GitHub Actions, and Kubernetes CronJobs. This is a complete rewrite of the original Strøm project. Backend in Rust, frontend in React + shadcn/ui. GitOps-based workflow definitions in YAML, PostgreSQL as the single backend.

---

## 1. Cargo Workspace Structure

```
stroem/
├── Cargo.toml                    # Workspace root
├── Cargo.lock
├── CLAUDE.md
├── README.md
├── docker-compose.yml
├── Dockerfile.server
├── Dockerfile.worker
├── crates/
│   ├── stroem-common/            # Shared types, models, DAG walker, templating
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── models/
│   │       │   ├── mod.rs
│   │       │   ├── workflow.rs   # ActionDef, TaskDef, FlowStep, TriggerDef
│   │       │   ├── job.rs        # Job, JobStep, JobStatus
│   │       │   └── auth.rs       # User, ApiKey
│   │       ├── dag.rs            # DAG walker (resolve dependencies, find ready steps)
│   │       ├── template.rs       # Tera-based parameter rendering
│   │       └── validation.rs     # YAML schema validation
│   │
│   ├── stroem-db/                # Database layer (sqlx, migrations, repositories)
│   │   ├── Cargo.toml
│   │   ├── migrations/
│   │   │   └── 001_initial.sql
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── pool.rs           # PgPool creation
│   │       └── repos/
│   │           ├── mod.rs
│   │           ├── job.rs        # JobRepo: create, claim, update, query
│   │           ├── job_step.rs   # JobStepRepo: create, update, query
│   │           ├── user.rs       # UserRepo
│   │           └── api_key.rs    # ApiKeyRepo
│   │
│   ├── stroem-runner/            # Execution backends (shell, docker, k8s)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── traits.rs         # Runner trait + RunnerDispatch (routes by type+image)
│   │       ├── shell.rs          # ShellRunner (shell without image, in-worker)
│   │       ├── docker.rs         # DockerRunner (shell+image & docker, via docker run)
│   │       └── kubernetes.rs     # KubeRunner (shell+image, docker & pod, via k8s Pod)
│   │
│   ├── stroem-server/            # API server + scheduler + orchestrator
│   │   ├── Cargo.toml
│   │   ├── static/               # Embedded UI build output
│   │   └── src/
│   │       ├── main.rs
│   │       ├── config.rs         # ServerConfig (YAML)
│   │       ├── state.rs          # AppState (shared across handlers)
│   │       ├── web/
│   │       │   ├── mod.rs        # Router assembly + static file serving
│   │       │   ├── api/          # Public REST API
│   │       │   │   ├── mod.rs
│   │       │   │   ├── tasks.rs  # List/get/execute tasks
│   │       │   │   ├── jobs.rs   # List/get jobs, stream logs
│   │       │   │   └── dashboard.rs
│   │       │   ├── worker_api/   # Worker-facing API
│   │       │   │   ├── mod.rs
│   │       │   │   ├── jobs.rs   # Claim job, report result, push logs
│   │       │   │   └── workspace.rs  # Download workspace tarball
│   │       │   └── auth/         # Login, OIDC callback, refresh, api keys
│   │       │       └── mod.rs
│   │       ├── ws/               # WebSocket log streaming
│   │       │   └── mod.rs
│   │       ├── workspace/        # Git/folder workspace sync
│   │       │   ├── mod.rs
│   │       │   ├── git.rs
│   │       │   └── folder.rs
│   │       ├── scheduler.rs      # Cron trigger evaluation
│   │       ├── orchestrator.rs   # DAG step dispatcher (distributed mode)
│   │       └── log_storage.rs    # Local disk + S3 upload
│   │
│   ├── stroem-worker/            # Worker process
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── config.rs         # WorkerConfig
│   │       ├── poller.rs         # HTTP poll loop for jobs
│   │       ├── executor.rs       # Step execution + log collection
│   │       └── local_dag.rs      # DAG walker for mode:local tasks
│   │
│   └── stroem-cli/               # CLI tool
│       ├── Cargo.toml
│       └── src/
│           └── main.rs           # validate, trigger, status commands
│
└── ui/                           # React frontend (separate npm project)
    ├── package.json
    ├── vite.config.ts
    ├── tsconfig.json
    └── src/
        ├── main.tsx
        ├── App.tsx
        ├── components/           # shadcn/ui components
        ├── pages/
        │   ├── Dashboard.tsx
        │   ├── Tasks.tsx
        │   ├── TaskDetail.tsx
        │   ├── Jobs.tsx
        │   ├── JobDetail.tsx      # With live log streaming
        │   └── Login.tsx
        ├── hooks/
        │   ├── useApi.ts
        │   └── useWebSocket.ts
        └── lib/
            └── api.ts            # API client
```

---

## 2. Workspace Layout & Action Resolution

### Multi-workspace model

The server manages multiple **workspaces**, each a separate source (folder or git repo) that contributes its own tasks, triggers, and actions. Tasks are namespaced by workspace name to avoid collisions.

```yaml
# server-config.yaml (see Section 6 for full config)
workspaces:
  platform:                          # workspace name -> task namespace
    type: folder
    path: ./workspaces/platform
  data-team:
    type: git
    url: https://github.com/org/data-workflows.git
    ref: main
  monitoring:
    type: git
    url: https://github.com/org/monitoring-workflows.git
    ref: v2.1.0
```

Task names are qualified: `platform/deploy-api`, `data-team/etl-daily`, `monitoring/check-health`. The API, UI, and CLI all use the `{workspace}/{task}` format. Actions are workspace-scoped -- a task can only reference actions from its own workspace or from libraries declared in that workspace's `.stroem.yaml`.

**Phase 1 (MVP):** Single workspace (named `default`), tasks use unqualified names. The multi-workspace config is additive -- existing single-workspace setups keep working.

### Workspace structure

Each workspace follows the same layout:

```
workspace/
├── .stroem.yaml               # workspace config (libraries, globals)
├── .workflows/                # workflow definitions (YAML)
│   ├── deploy.yaml
│   └── monitoring.yaml
├── actions/                   # local action scripts
│   ├── deploy.sh
│   ├── notify.py
│   └── build-image.sh
└── bin/                       # binaries (optional)
    └── my-tool
```

### Action types: `shell`, `docker`, `pod`

Actions declare their execution type. This determines which runner handles them and how they behave across different environments (bare metal, Docker, Kubernetes).

#### `type: shell` -- run a command

```yaml
actions:
  # Inline command -- runs in worker process (fast, no isolation)
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }}"

  # Script file -- path relative to workspace root
  deploy:
    type: shell
    script: actions/deploy.sh
    input:
      env: { type: string }

  # With image -- runs in a separate container (isolated, CI/CD use case)
  build:
    type: shell
    image: node:20-alpine             # presence of image = separate container/pod
    cmd: "npm ci && npm run build"
    resources:
      cpu: "2"
      memory: "4Gi"
```

The `image` field controls execution mode:
- **Without `image`**: command runs directly in the worker process. Fast (~0ms overhead). Good for lightweight ops: health checks, notifications, glue scripts.
- **With `image`**: command runs in a separate container. Worker creates a Docker container (via `docker run`) or k8s Pod, mounts the workspace, runs `cmd` via `sh -c`. This is the CI/CD mode -- each step gets its own toolchain and resource limits.

#### `type: docker` -- run a prebuilt image

```yaml
actions:
  # CronJob replacement -- prebuilt image runs as-is
  backup-db:
    type: docker
    image: company/db-backup:v2.1
    command: ["--compress", "--target", "s3://bucket/backups"]
    env:
      DB_HOST: "{{ input.db_host }}"
      DB_PASSWORD: "{{ secret.db_password }}"
    resources:
      cpu: "500m"
      memory: "512Mi"
    input:
      db_host: { type: string, required: true }

  # One-off tool
  lint:
    type: docker
    image: golangci/golangci-lint:v1.59
    command: ["golangci-lint", "run", "./..."]
    workdir: /src                     # mount workspace at this path
```

The image runs with its own ENTRYPOINT/CMD. Workspace is NOT mounted by default (opt-in via `workdir`). Always runs in a separate container/pod.

#### `type: pod` -- full k8s pod spec (Phase 4)

```yaml
actions:
  gpu-training:
    type: pod
    image: company/ml-training:latest
    command: ["python", "train.py"]
    pod:
      tolerations:
        - key: "nvidia.com/gpu"
          operator: "Exists"
      node_selector:
        gpu: "a100"
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: training-data
      init_containers:
        - name: download-model
          image: company/model-downloader:v1
```

The escape hatch for workloads that need specific k8s scheduling, GPUs, sidecars, init containers, custom volumes. Only works on k8s workers.

#### Execution mode summary

A single workflow can mix all action types. The server dispatches each step only to workers that support the action's type (see Section 5: Runner Dispatch).

```
┌─────────────────────────┬──────────────────────┬────────────────────────────┐
│ Action                  │ Non-k8s worker       │ K8s worker                 │
├─────────────────────────┼──────────────────────┼────────────────────────────┤
│ shell (no image)        │ ShellRunner          │ ShellRunner (in worker)    │
│ shell (with image)      │ DockerRunner         │ KubeRunner (pod + sh -c)   │
│ docker                  │ DockerRunner         │ KubeRunner (pod)           │
│ pod                     │ ERROR (k8s only)     │ KubeRunner (full pod spec) │
└─────────────────────────┴──────────────────────┴────────────────────────────┘
```

### Action libraries (external Git repos)

Libraries are **action-only** toolboxes. They provide reusable action definitions and scripts, but NOT tasks or triggers. Each workspace declares its own libraries in `.stroem.yaml`:

```yaml
# .stroem.yaml
libraries:
  common:
    git: https://github.com/org/stroem-action-library.git
    ref: v1.2.0          # tag, branch, or commit SHA (branches float to latest)
  internal:
    git: git@github.com:org/internal-actions.git
    ref: main            # branch ref allowed, logged with warning
```

Library repo layout:

```
stroem-action-library/
├── .workflows/            # ONLY action definitions (tasks and triggers are IGNORED)
│   └── actions.yaml       # action defs: slack-notify, pagerduty-alert, etc.
└── actions/               # scripts referenced by action definitions
    ├── slack-notify.sh
    └── pagerduty.py
```

Library actions are referenced with a `library_name/action_name` prefix:

```yaml
tasks:
  deploy:
    flow:
      notify:
        action: common/slack-notify   # from "common" library
      deploy:
        action: deploy                # local action (no prefix)
```

No implicit resolution -- unprefixed names always resolve to the local workspace. Name collisions between libraries are impossible because the prefix is mandatory.

### Library sync & caching

1. Server reads `.stroem.yaml` from each workspace
2. Server clones/fetches libraries to a managed cache dir (`/var/stroem/libraries/{name}/{ref}/`)
3. **Pinned refs** (tags, SHAs): clone once, never update
4. **Branch refs**: pull on workspace reload, log a warning ("floating ref")
5. Server parses library `.workflows/` files, loads **only action definitions**, ignores tasks/triggers
6. Libraries are shared across workspaces -- if `platform` and `data-team` both reference `common@v1.2.0`, it's cloned once

### Path resolution at runtime

1. Server builds a workspace tarball per workspace, including libraries under `_libraries/{name}/`
2. Worker extracts tarball to `/tmp/stroem/{workspace}/{job_id}/`
3. Runner sets working directory to workspace root
4. `cmd` paths are relative to workspace root
5. `script` paths are resolved relative to workspace root (local) or `_libraries/{name}/` (library)

---

## 3. Database Schema

File: `crates/stroem-db/migrations/001_initial.sql`

```sql
-- Workers
CREATE TABLE worker (
    worker_id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    capabilities JSONB NOT NULL DEFAULT '["shell"]',  -- e.g. ["shell", "docker", "kubernetes"]
    last_heartbeat TIMESTAMPTZ,
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'inactive'))
);

-- Jobs (the queue)
CREATE TABLE job (
    job_id UUID PRIMARY KEY,
    workspace TEXT NOT NULL DEFAULT 'default',
    task_name TEXT NOT NULL,           -- unqualified name (workspace provides the namespace)
    mode TEXT NOT NULL DEFAULT 'distributed' CHECK (mode IN ('local', 'distributed')),
    input JSONB,
    output JSONB,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled')),
    source_type TEXT NOT NULL CHECK (source_type IN ('trigger', 'user', 'api', 'webhook')),
    source_id TEXT,
    worker_id UUID REFERENCES worker(worker_id),
    revision TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    log_path TEXT                   -- S3 key or local path
);

CREATE INDEX idx_job_status ON job(status);
CREATE INDEX idx_job_created ON job(created_at);
CREATE INDEX idx_job_task ON job(workspace, task_name);

-- Job Steps (for distributed mode, each step is dispatchable)
CREATE TABLE job_step (
    job_id UUID NOT NULL REFERENCES job(job_id) ON DELETE CASCADE,
    step_name TEXT NOT NULL,
    action_name TEXT NOT NULL,
    action_type TEXT NOT NULL DEFAULT 'shell' CHECK (action_type IN ('shell', 'docker', 'pod')),
    action_image TEXT,                -- container image (for shell+image, docker, pod types)
    action_spec JSONB,                -- full action definition (cmd, command, env, resources, pod spec)
    input JSONB,
    output JSONB,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'ready', 'running', 'completed', 'failed', 'skipped')),
    worker_id UUID REFERENCES worker(worker_id),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    error_message TEXT,
    PRIMARY KEY (job_id, step_name)
);

CREATE INDEX idx_job_step_status ON job_step(status);

-- Users
CREATE TABLE "user" (
    user_id UUID PRIMARY KEY,
    name TEXT,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Auth links (OIDC providers)
CREATE TABLE user_auth_link (
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL,
    external_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, provider_id)
);

-- Refresh tokens
CREATE TABLE refresh_token (
    token_hash TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- API keys
CREATE TABLE api_key (
    key_hash TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,           -- first 8 chars for identification
    scopes JSONB,                   -- optional: restrict to specific tasks
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ
);
```

---

## 4. API Design

### Public API (authenticated via JWT or API key)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/workspaces` | List all workspaces |
| GET | `/api/workspaces/{ws}/tasks` | List tasks in workspace |
| GET | `/api/workspaces/{ws}/tasks/{name}` | Get task detail (input schema, flow) |
| POST | `/api/workspaces/{ws}/tasks/{name}/execute` | Trigger task execution |
| GET | `/api/jobs` | List jobs (paginated, filterable by workspace) |
| GET | `/api/jobs/{id}` | Get job detail with steps |
| GET | `/api/jobs/{id}/logs` | Get job logs (from disk or S3) |
| WS | `/api/jobs/{id}/logs/stream` | WebSocket live log stream |
| GET | `/api/dashboard/status` | System status metrics |
| GET | `/api/dashboard/metrics` | Job metrics (counts, durations) |
| POST | `/api/auth/login` | Email/password login |
| GET | `/api/auth/oidc/{provider}` | OIDC redirect |
| GET | `/api/auth/oidc/{provider}/callback` | OIDC callback |
| POST | `/api/auth/refresh` | Refresh JWT |
| POST | `/api/auth/logout` | Revoke refresh token |

### Worker API (authenticated via bearer token)

| Method | Path | Description |
|--------|------|-------------|
| POST | `/worker/register` | Register worker with capabilities, get worker_id |
| POST | `/worker/heartbeat` | Worker heartbeat |
| POST | `/worker/jobs/claim` | Claim next available job/step |
| POST | `/worker/jobs/{id}/steps/{step}/start` | Mark step as running |
| POST | `/worker/jobs/{id}/steps/{step}/complete` | Report step result |
| POST | `/worker/jobs/{id}/logs` | Push log chunk |
| POST | `/worker/jobs/{id}/complete` | Mark local-mode job as done |
| GET | `/worker/workspace/{ws}.tar.gz` | Download workspace tarball for a workspace |

### Webhook API (authenticated via signature or token)

| Method | Path | Description |
|--------|------|-------------|
| POST | `/hooks/{trigger_name}` | Trigger a webhook-based task |

---

## 5. Key Architectural Components

### Worker capabilities & runner dispatch

Workers advertise their capabilities on registration:

```
POST /worker/register
{
  "name": "worker-1",
  "capabilities": ["shell", "docker"]
}
```

Capabilities are derived from the worker's configured runners:
- `shell` runner -> advertises `["shell"]`
- `docker` runner -> advertises `["shell", "docker"]` (docker can run shell+image too)
- `kubernetes` runner -> advertises `["shell", "docker", "pod"]` (k8s can run everything)

Each job step carries `action_type` and `action_image` (derived from the action definition). The claim endpoint filters by capability match:

```sql
-- Claim query filters by capability match
SELECT ... FROM job_step
WHERE status = 'ready'
  AND action_type = ANY($1)  -- $1 = worker's capabilities array
ORDER BY ...
FOR UPDATE SKIP LOCKED
LIMIT 1;
```

Runner dispatch on the worker side -- the worker inspects both `action_type` and `action_image` to select the runner:

```
action_type=shell, image=NULL    -> ShellRunner (in-worker process, fast)
action_type=shell, image=present -> DockerRunner or KubeRunner (separate container)
action_type=docker               -> DockerRunner or KubeRunner (always separate container)
action_type=pod                  -> KubeRunner only (full pod spec)
```

For workers with both `docker` and `kubernetes` runners configured, the `kubernetes` runner takes priority for container-based actions.

### Kubernetes runner (controller pattern)

The k8s worker runs as a Deployment and acts as a **controller** -- it doesn't execute workloads itself, it orchestrates Pods:

```
Server -> Worker claims step -> Worker creates k8s Job/Pod
                              -> Worker watches pod logs (k8s API)
                              -> Worker streams logs back to server
                              -> Pod exits -> Worker reports result
```

How each action type maps to a k8s Pod:

| Action | Pod behavior |
|--------|-------------|
| `shell` (no image) | Runs directly in worker pod via ShellRunner. No pod created. |
| `shell` (with image) | Creates Pod with specified image, runs `cmd` via `sh -c`, workspace mounted. |
| `docker` | Creates Pod with specified image, runs `command` as container args. Workspace opt-in via `workdir`. |
| `pod` | Creates Pod with full spec from action definition (tolerations, node selectors, volumes, init containers, etc). |

The `resources` field from the action definition maps to k8s resource requests/limits.

### Shared workspace for k8s step pods (CI/CD)

When steps run in separate Pods, they need to share build artifacts (e.g., checkout writes source, build reads it). The k8s worker handles this:

1. Worker claims a job, creates an ephemeral PVC (`stroem-job-{job_id}`)
2. Downloads workspace tarball into the PVC (via init container or first step)
3. Each step Pod mounts the same PVC at the workspace path
4. Steps share files naturally (checkout writes, build reads, deploy reads)
5. When job completes, worker deletes the PVC

### Orchestrator (distributed mode)

When a job is created for a `mode: distributed` task:
1. Server creates `job` row + `job_step` rows for all steps (status: `pending`, `action_type` from action def)
2. Steps with no `depends_on` are set to `ready`
3. Workers claim `ready` steps via `POST /worker/jobs/claim` -- server filters by capability match
4. When a step completes, the orchestrator checks if downstream steps have all dependencies met -> marks them `ready`
5. When all steps complete -> job is marked `completed`

### Local DAG executor (local mode)

When a job is created for a `mode: local` task:
1. Server creates `job` row (no step rows yet)
2. Worker claims the whole job
3. Worker downloads workspace tarball, creates temp directory
4. Worker walks the DAG locally, executing steps sequentially/in-parallel
5. Worker reports step results as it goes
6. Worker marks job complete when all steps done

### Log Flow (HTTP chunked POST)

Worker buffers log lines and POSTs chunks to server every ~1s. Chosen over gRPC/WebSocket for robustness: works through proxies, no persistent connections, simple retry. Can upgrade to gRPC streaming later if log volume demands it.

```
Runner stdout/stderr
  -> Worker captures line-by-line, buffers in memory
  -> Worker POSTs chunk to POST /worker/jobs/{id}/logs every ~1s
  -> Server appends to /var/stroem/logs/{job_id}.log
  -> Server broadcasts to WebSocket subscribers for that job
  -> On job completion: Server uploads log file to S3, stores S3 key in DB
  -> On log request: serve from local file if exists, else fetch from S3
```

---

## 6. Configuration

### Server config (`server-config.yaml`)

```yaml
listen: "0.0.0.0:8080"
public_url: "http://localhost:8080"

db:
  url: "postgres://stroem:stroem@localhost:5432/stroem"

log_storage:
  local_dir: /var/stroem/logs
  s3:                               # optional
    bucket: stroem-logs
    region: eu-west-1
    prefix: logs/

# Multiple workspaces -- each contributes tasks/triggers under its namespace
workspaces:
  default:                            # MVP: single workspace named "default"
    type: folder
    path: ./workspace
  # Additional workspaces (Phase 3+):
  # data-team:
  #   type: git
  #   url: https://github.com/org/data-workflows.git
  #   ref: main
  #   poll_interval: 60s
  #   auth:
  #     type: ssh_key | token
  # monitoring:
  #   type: git
  #   url: https://github.com/org/monitoring-workflows.git
  #   ref: v2.1.0

library_cache_dir: /var/stroem/libraries  # shared cache for library clones

auth:
  jwt_secret: "change-me"
  refresh_secret: "change-me-too"
  providers:
    internal:
      type: internal
    # company-sso:
    #   type: oidc
    #   issuer_url: https://auth.example.com
    #   client_id: stroem
    #   client_secret: secret
  initial_user:
    email: admin@stroem.local
    password: admin

worker_token: "shared-worker-secret"
```

### Worker config (`worker-config.yaml`)

Workers can support multiple runner types simultaneously. Capabilities are derived from configured runners.

```yaml
server_url: "http://localhost:8080"
worker_token: "shared-worker-secret"
worker_name: "worker-1"
max_concurrent: 4
poll_interval: 2s
workspace_dir: /tmp/stroem-workspace

# Multiple runners -- worker advertises capabilities based on what's configured
runners:
  - type: shell                     # always available, runs in worker process

  # Docker runner (uncomment to enable)
  # Adds capabilities: ["shell", "docker"] (handles shell+image via docker run)
  # - type: docker
  #   socket: /var/run/docker.sock

  # Kubernetes runner -- creates Pods for container-based actions
  # Adds capabilities: ["shell", "docker", "pod"]
  # - type: kubernetes
  #   namespace: stroem-jobs
  #   service_account: stroem-runner
  #   default_resources:
  #     cpu: "250m"
  #     memory: "256Mi"
  #   workspace_storage_class: standard  # for ephemeral PVCs
  #   workspace_storage_size: 5Gi
```

**Capability derivation examples:**

| Configured runners | Advertised capabilities | Notes |
|-------------------|------------------------|-------|
| `[shell]` | `["shell"]` | MVP default. Only in-worker shell commands. |
| `[shell, docker]` | `["shell", "docker"]` | Shell in-worker + Docker containers. shell+image runs via `docker run`. |
| `[shell, kubernetes]` | `["shell", "docker", "pod"]` | Shell in-worker + k8s Pods for everything else. |
| `[kubernetes]` | `["docker", "pod"]` | Pure k8s worker. No in-worker shell (all commands run in Pods). |

---

## 7. Docker Compose (Development)

```yaml
services:
  postgres:
    image: postgres:17
    environment:
      POSTGRES_USER: stroem
      POSTGRES_PASSWORD: stroem
      POSTGRES_DB: stroem
    ports:
      - "5432:5432"
    volumes:
      - pgdata:/var/lib/postgresql/data

  server:
    build:
      context: .
      dockerfile: Dockerfile.server
    ports:
      - "8080:8080"
    volumes:
      - ./workspace:/workspace
      - logs:/var/stroem/logs
    depends_on:
      - postgres
    environment:
      STROEM_CONFIG: /etc/stroem/server-config.yaml

  worker:
    build:
      context: .
      dockerfile: Dockerfile.worker
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock  # for docker runner
    depends_on:
      - server
    environment:
      STROEM_CONFIG: /etc/stroem/worker-config.yaml

volumes:
  pgdata:
  logs:
```

---

## 8. Phased Build Order

### Phase 1 -- MVP (this is what we build now)

Goal: End-to-end execution of a simple workflow via CLI/API.

**Step 1 -- Workspace + common types**
- Init Cargo workspace with all crate stubs (empty lib.rs/main.rs that compile)
- `stroem-common`: Workflow YAML models (`ActionDef`, `TaskDef`, `FlowStep`, `TriggerDef`, `WorkflowConfig`) with serde deserialization via `serde_yaml`
- `stroem-common`: DAG walker (`fn ready_steps(steps, completed) -> Vec<StepName>`)
- `stroem-common`: Tera template renderer (`fn render_input(template_map, context) -> Result<Value>`)
- Unit tests for YAML parsing, DAG resolution, template rendering

**Step 2 -- Database layer**
- `stroem-db`: PgPool creation from config URL
- `stroem-db`: Migration `001_initial.sql` (job, job_step, worker tables -- no auth tables yet in MVP)
- `stroem-db`: `JobRepo` -- `create_job`, `create_steps`, `claim_ready_step` (SELECT FOR UPDATE SKIP LOCKED), `complete_step`, `get_job`, `list_jobs`
- `stroem-db`: `JobStepRepo` -- `update_status`, `get_steps_for_job`, `get_ready_steps`
- Integration tests against a real Postgres (docker)

**Step 3 -- Runner**
- `stroem-runner`: `Runner` trait with `async fn execute(action, env, workdir) -> RunResult`
- `stroem-runner`: `RunnerDispatch` -- routes steps to the correct runner by `action_type`
- `stroem-runner`: `ShellRunner` -- spawn process, capture stdout/stderr line-by-line, parse `OUTPUT: {json}` from stdout
- Unit tests with simple shell commands

**Step 4 -- Server core**
- `stroem-server`: Config loading (serde deserialize from YAML)
- `stroem-server`: DB pool + run migrations on startup
- `stroem-server`: Single workspace folder loader (scan `default` workspace's `.workflows/` dir, parse YAML, store in-memory)
- `stroem-server`: AppState struct (workspaces map, repos, config)
- MVP uses single workspace named `default` -- tasks are unqualified in API responses but `workspace` column is populated in DB for forward compatibility

**Step 5 -- Server API**
- Public API: `GET /api/workspaces/{ws}/tasks`, `GET /api/workspaces/{ws}/tasks/{name}`, `POST /api/workspaces/{ws}/tasks/{name}/execute`
- Public API: `GET /api/jobs`, `GET /api/jobs/{id}`, `GET /api/jobs/{id}/logs`
- Worker API: `POST /worker/register` (with capabilities), `POST /worker/jobs/claim` (filtered by capability), `POST /worker/jobs/{id}/steps/{step}/start`, `POST /worker/jobs/{id}/steps/{step}/complete`, `POST /worker/jobs/{id}/logs`, `POST /worker/jobs/{id}/complete`
- Worker auth: simple bearer token check (from config)
- No user auth in MVP -- add in Phase 2

**Step 6 -- Orchestrator**
- When step completes -> check all dependents -> if all deps met -> mark step `ready`
- When all steps complete -> mark job `completed`/`failed`
- Triggered by the `POST /worker/jobs/{id}/steps/{step}/complete` handler

**Step 7 -- Worker**
- Config loading, worker registration on startup
- Poll loop: `POST /worker/jobs/claim` every N seconds
- On claim: resolve inputs via Tera templates, execute via ShellRunner
- Buffer stdout/stderr, POST log chunks to server every ~1s
- Report step result on completion

**Step 8 -- Log storage + docker-compose**
- Server: append log chunks to `/var/stroem/logs/{job_id}.log`
- Server: serve logs from local file via `GET /api/jobs/{id}/logs`
- docker-compose: postgres + server + worker + sample workspace volume
- Sample workflow YAML for testing

Deliverable: `docker-compose up` -> curl to trigger a multi-step workflow -> steps execute on worker -> logs retrievable via API.

### Phase 2 -- UI + Auth

1. ~~**React project setup**: Vite + React + shadcn/ui + TailwindCSS~~ **DONE** (Phase 2b) -- Vite + React 19 + TypeScript + Tailwind v4 + shadcn/ui in `ui/` directory
2. ~~**Pages**: Login, Dashboard, Tasks list, Task detail, Jobs list, Job detail~~ **DONE** (Phase 2b) -- All 6 pages with auth-aware routing, auto-refresh, status badges
3. ~~**WebSocket log streaming**: Server endpoint + React hook~~ **DONE** (Phase 2a) -- Server-side WebSocket endpoint with backfill + live streaming via `tokio::sync::broadcast`
4. ~~**Auth**: JWT + refresh tokens, internal provider (email/password)~~ **DONE** (Phase 2a) -- JWT access tokens (15min), refresh token rotation (30 day, SHA256 hashed), argon2id password hashing, optional auth config, initial user seeding
5. ~~**UI embedding**: Build UI -> copy to `stroem-server/static/` -> `rust_embed`~~ **DONE** (Phase 2b) -- rust-embed with SPA fallback, 3-stage Docker build (bun + rust + runtime)
6. ~~**Auto-generated run form**: Parse task input schema -> render form in UI~~ **DONE** (Phase 2b) -- Dialog auto-generates form fields from task input schema (string/number/boolean)

### Phase 3 -- Multi-workspace, Runners & Production Readiness

1. ~~**Multi-workspace support**~~: **DONE** -- Multiple workspace sources (folder + git), each namespaced. `GET /api/workspaces` endpoint. Workspace-scoped tarballs.
2. ~~**Git workspace source**~~: **DONE** -- Clone, poll, sync on changes. Branch tracking with configurable poll interval.
3. **Action libraries**: Clone library repos, cache by ref, parse action-only definitions, include in workspace tarballs under `_libraries/`.
4. ~~**Schedule triggers**~~: **DONE** -- Cron-based trigger evaluation loop on the server. Smart sleep scheduler, config hot-reload, clean shutdown.
5. ~~**DockerRunner**~~: **DONE** -- (via bollard) Type 1 (container as-is) and Type 2 (shell in runner with workspace mounted).
6. ~~**KubeRunner**~~: **DONE** -- (via kube-rs) Type 1 (pod as-is) and Type 2 (shell in runner with init container + workspace volume).
7. ~~**OIDC auth provider**~~: **DONE** -- Authorization Code + PKCE flow, JIT user provisioning, state cookie.
8. ~~**S3 log upload**~~: **DONE** -- Background upload on job completion, read fallback chain (local → S3).
9. **API keys**: Create, list, revoke, authenticate
10. **Webhook triggers**: `POST /hooks/{name}` endpoint
11. ~~**on_error / on_success hooks**~~: **DONE** -- Task-level hooks with Tera-rendered input, recursion guard, supports `type: task` hook actions.
12. ~~**Sub-task execution (`type: task` action)**~~: **DONE** -- Server-side dispatch, recursive child jobs, depth limit (10), parent propagation.
13. ~~**Worker heartbeat + stale job recovery**~~: **DONE** -- Background sweeper, stale worker detection, step failure + job orchestration.
14. ~~**stroem-cli**~~: **DONE** -- Commands: `validate`, `trigger`, `status`, `logs`, `tasks`, `jobs`, `workspaces`. Workspace-scoped with `--workspace` flag.
15. ~~**Helm chart**~~: **DONE** -- Full chart at `helm/stroem/` (OCI: `charts/stroem`), server + worker deployments, RBAC for KubeRunner, optional DinD sidecar, ingress, secrets, configmaps.
16. ~~**Default features + release workflow**~~: **DONE** -- All cargo features (docker, kubernetes, s3) enabled by default. Dedicated release workflow on `v*` tags (Docker images + Helm chart + GitHub Release). Dev builds on main only.

### Phase 4 -- Advanced Features

1. **Local mode** execution (Worker-side DAG walker, shared workspace)
2. **`type: pod` actions** -- full k8s pod spec (tolerations, node selectors, volumes, init containers, sidecars)
3. **RBAC**: Roles, permissions, workspace-scoped access control
4. **Secret resolution** via `vals` CLI
5. **Resource types** (custom resource definitions in YAML)
6. **DAG visualization** -- Interactive workflow graph on task detail and job detail pages. Auto-layout of steps as nodes with dependency edges, virtual start/finish nodes. Clickable steps to inspect inputs, outputs, and logs. On job detail, nodes reflect live step status (pending/running/completed/failed). Library candidates: dagre, elkjs, reactflow.

---

## 9. Verification

### Phase 1 testing

1. **Unit tests**: DAG walker (cycle detection, topological sort, ready step resolution), Tera template rendering, YAML parsing
2. **Integration tests**: JobRepo claim concurrency (spawn multiple tasks claiming from same queue), step state transitions
3. **End-to-end**: docker-compose up -> create workspace with sample workflow -> `curl POST /api/workspaces/default/tasks/hello-world/execute` -> verify job completes -> `curl GET /api/jobs/{id}` shows success -> `curl GET /api/jobs/{id}/logs` returns output

### Sample CI/CD pipeline (`workspace/.workflows/ci.yaml`)

Demonstrates mixed action types: in-worker shell (fast glue), shell+image (isolated CI steps), and docker (prebuilt tool).

```yaml
actions:
  checkout:
    type: shell                        # in-worker, fast, no container overhead
    cmd: "git clone --depth=1 {{ input.repo }} /workspace/src"
    input:
      repo: { type: string, required: true }

  install:
    type: shell
    image: node:20-alpine              # separate container -- CI/CD step
    cmd: "cd /workspace/src && npm ci"
    resources: { cpu: "1", memory: "2Gi" }

  test:
    type: shell
    image: node:20-alpine              # separate container -- CI/CD step
    cmd: "cd /workspace/src && npm test"
    resources: { cpu: "2", memory: "4Gi" }

  build-image:
    type: docker                       # prebuilt tool, runs as-is
    image: gcr.io/kaniko-project/executor:latest
    command: ["--context=/workspace/src", "--destination=company/app:{{ input.tag }}"]

  deploy:
    type: shell
    image: bitnami/kubectl:1.30        # separate container -- CI/CD step
    cmd: "kubectl set image deployment/app app=company/app:{{ input.tag }}"
    input:
      tag: { type: string, required: true }

tasks:
  ci-pipeline:
    mode: distributed
    input:
      repo: { type: string }
      tag: { type: string }
    flow:
      checkout:
        action: checkout
        input: { repo: "{{ input.repo }}" }
      install:
        action: install
        depends_on: [checkout]
      test:
        action: test
        depends_on: [install]
      build:
        action: build-image
        depends_on: [test]
        input: { tag: "{{ input.tag }}" }
      deploy:
        action: deploy
        depends_on: [build]
        input: { tag: "{{ input.tag }}" }
```

### Sample docker workflow (`workspace/.workflows/backup.yaml`)

```yaml
actions:
  backup:
    type: docker
    image: company/db-backup:v2.1
    command: ["--compress", "--output", "/data/backup.sql"]
    env:
      DB_HOST: "{{ input.db_host }}"
    resources:
      cpu: "500m"
      memory: "512Mi"
    input:
      db_host: { type: string, required: true }

  notify:
    type: shell
    cmd: "echo 'Backup completed for {{ input.db_host }}'"
    input:
      db_host: { type: string, required: true }

tasks:
  nightly-backup:
    mode: distributed
    input:
      db_host: { type: string, default: "db.internal" }
    flow:
      run-backup:
        action: backup
        input:
          db_host: "{{ input.db_host }}"
      send-notification:
        action: notify
        depends_on: [run-backup]
        input:
          db_host: "{{ input.db_host }}"

triggers:
  nightly:
    type: scheduler
    cron: "0 0 2 * * *"
    task: nightly-backup
    enabled: true
```

### Sample test workflow (`workspace/.workflows/hello.yaml`)

```yaml
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }}"
    input:
      name: { type: string, required: true }
    output:
      properties:
        greeting: { type: string }

  shout:
    type: shell
    cmd: "echo {{ input.message }} | tr '[:lower:]' '[:upper:]'"
    input:
      message: { type: string, required: true }

tasks:
  hello-world:
    mode: distributed
    input:
      name: { type: string, default: "World" }
    flow:
      say-hello:
        action: greet
        input:
          name: "{{ input.name }}"
      shout-it:
        action: shout
        depends_on: [say-hello]
        input:
          message: "{{ say-hello.output.greeting }}"

triggers:
  every-minute:
    type: scheduler
    cron: "0 * * * * *"
    task: hello-world
    input:
      name: "Cron"
    enabled: false
```
