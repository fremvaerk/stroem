---
title: Runners
description: Configuring Docker and Kubernetes container runners
---

Container runners are optional features that allow steps to execute inside Docker containers or Kubernetes pods instead of directly on the worker host.

## Runner modes

| Runner | How it works |
|--------|-------------|
| `local` (default) | Step runs directly on the worker host |
| `docker` | Step runs in a Docker container with workspace at `/workspace` |
| `pod` | Step runs as a Kubernetes pod with workspace via init container |

## Docker runner

### Prerequisites

1. Build the worker with the `docker` feature:
   ```bash
   cargo build -p stroem-worker --features docker
   ```
2. Add `docker: {}` to the worker config
3. The worker needs Docker daemon access (local socket, or DinD sidecar in K8s)

### Worker config

```yaml
tags:
  - script
  - docker
docker: {}
runner_image: "ghcr.io/fremvaerk/stroem-runner:latest"
```

## Kubernetes runner

### Prerequisites

1. Build the worker with the `kubernetes` feature:
   ```bash
   cargo build -p stroem-worker --features kubernetes
   ```
2. Add a `kubernetes:` section to the worker config
3. The worker needs in-cluster credentials or a kubeconfig with permissions to create/get/delete pods
4. The server must be reachable from inside the pod (the init container downloads workspace tarballs)

### Worker config

```yaml
tags:
  - script
  - kubernetes
kubernetes:
  namespace: stroem-jobs
  init_image: curlimages/curl:latest  # optional, default
```

## Combined config

Both features can be enabled simultaneously:

```yaml
server_url: "http://stroem-server:8080"
worker_token: "your-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: /var/stroem/workspace-cache
tags:
  - script
  - docker
  - kubernetes
runner_image: "ghcr.io/fremvaerk/stroem-runner:latest"
docker: {}
kubernetes:
  namespace: stroem-jobs
```

## Worker tags

Tags control which steps a worker can claim. Each step automatically computes `required_tags` based on its action type and runner configuration:

| Action | Runner | Required tags |
|--------|--------|--------------|
| `script` | `local` (default) | `["script"]` |
| `script` | `docker` | `["docker"]` |
| `script` | `pod` | `["kubernetes"]` |
| `docker` | — | `["docker"]` |
| `pod` | — | `["kubernetes"]` |
| `task` | — | `[]` (server-dispatched) |

### Step tag matching

A worker claims a step only when **all** of the step's `required_tags` are present in the worker's `tags` list. This ensures specialized workloads route to capable workers.

**Worker configuration:**
```yaml
tags:
  - script      # can run local scripts
  - docker      # can run docker steps
  - kubernetes  # can run pod steps
  - gpu         # custom tag for GPU-enabled work
```

**Step with custom tags:**
```yaml
flow:
  train-model:
    action: train-gpu
    tags: ["gpu"]        # adds to required_tags
```

A step with `type: script, runner: docker, tags: ["gpu"]` requires `["docker", "gpu"]` — only workers with both tags can claim it.

### Unmatched steps

If no active worker has all required tags for a step, the step remains ready but unclaimed. After `unmatched_step_timeout_secs` (default 30 seconds, configurable in server recovery settings), the step fails with error: `"No active worker with required tags to run this step"`.

**Example scenario:** A step requires `["kubernetes"]` but all workers have `["script", "docker"]`. After 30 seconds, the step fails. To fix: add a worker with Kubernetes runner enabled, or remove the `runner: pod` requirement.

## Pre-installed tools

The official runner image (`ghcr.io/fremvaerk/stroem-runner`) and worker image ship with these tools pre-installed:

### System tools

| Tool | Description |
|------|-------------|
| `bash` | Default shell |
| `curl` | HTTP client |
| `git` | Version control |
| `jq` | JSON processor |
| `yq` | YAML/JSON/XML processor |
| `tar` / `gzip` | Archive and compression |
| `unzip` | ZIP extraction |
| `ssh` | OpenSSH client |

### Secret management

| Tool | Description |
|------|-------------|
| `sops` | Mozilla SOPS — encrypted file editing |
| `vals` | Helmfile vals — multi-backend secret resolution |

### Language runtimes

| Tool | Description |
|------|-------------|
| `uv` | Python package manager and runner |
| `bun` | JavaScript/TypeScript runtime |

**Example — Python with dependencies:**

```yaml
actions:
  analyze:
    type: script
    runner: docker
    script: |
      uv pip install pandas requests --system
      uv run python /workspace/scripts/analyze.py
```

**Example — TypeScript:**

```yaml
actions:
  generate-report:
    type: script
    runner: docker
    script: |
      cd /workspace
      bun install
      bun run scripts/report.ts
```

## Helm chart configuration

When deploying via Helm, use these values to configure runners:

```bash
# Enable Kubernetes runner
helm install stroem ./helm/stroem \
  --set worker.kubernetes.enabled=true \
  --set worker.kubernetes.namespace=stroem-jobs

# Enable Docker runner via DinD sidecar
helm install stroem ./helm/stroem \
  --set worker.dind.enabled=true
```

See [Helm / Kubernetes](/deployment/helm/) for full deployment details.
