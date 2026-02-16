# Stroem Helm Chart

Helm chart for deploying [Stroem](https://github.com/fremvaerk/stroem) workflow orchestration platform on Kubernetes.

## Prerequisites

- Kubernetes 1.26+
- Helm 3.8+
- PostgreSQL database (external)

## Install from OCI Registry

```bash
# Install latest version
helm install stroem oci://ghcr.io/fremvaerk/charts/stroem

# Install specific version
helm install stroem oci://ghcr.io/fremvaerk/charts/stroem --version 1.0.0

# Install with values file
helm install stroem oci://ghcr.io/fremvaerk/charts/stroem -f values.yaml
```

## Upgrade

```bash
helm upgrade stroem oci://ghcr.io/fremvaerk/charts/stroem --version 1.1.0
```

## Uninstall

```bash
helm uninstall stroem
```

## Architecture

The chart uses a **config pass-through** pattern:

- `server.config` and `worker.config` contain the **full** YAML config passed as-is to ConfigMaps
- Secrets are injected via `extraSecretEnv` as `STROEM__` environment variables that override config values at runtime
- No init containers or sed templating — clean and debuggable

### How env var overrides work

The server and worker binaries support `STROEM__` prefixed environment variables that override any YAML config value. The separator is `__` (double underscore) for nested keys:

| Env Var | Overrides YAML key |
|---------|--------------------|
| `STROEM__DB__URL` | `db.url` |
| `STROEM__WORKER_TOKEN` | `worker_token` |
| `STROEM__AUTH__JWT_SECRET` | `auth.jwt_secret` |
| `STROEM__LISTEN` | `listen` |

## Configuration

### Server

| Key | Description | Default |
|-----|-------------|---------|
| `server.image.repository` | Server image repository | `stroem-server` |
| `server.image.tag` | Server image tag | `latest` |
| `server.replicas` | Number of server replicas | `1` |
| `server.service.type` | Kubernetes service type | `ClusterIP` |
| `server.service.port` | Service port | `8080` |
| `server.config` | Full `server-config.yaml` content (pass-through) | See `values.yaml` |
| `server.extraSecretEnv` | Secret env vars for `STROEM__` overrides | `{}` |
| `server.extraEnv` | Plain env vars | `{}` |
| `server.resources` | CPU/memory resource limits | `{}` |

### Worker

| Key | Description | Default |
|-----|-------------|---------|
| `worker.image.repository` | Worker image repository | `stroem-worker` |
| `worker.image.tag` | Worker image tag | `latest` |
| `worker.replicas` | Number of worker replicas | `2` |
| `worker.config` | Full `worker-config.yaml` content (pass-through) | See `values.yaml` |
| `worker.extraSecretEnv` | Secret env vars for `STROEM__` overrides | `{}` |
| `worker.extraEnv` | Plain env vars | `{}` |
| `worker.dind.enabled` | Enable Docker-in-Docker sidecar | `false` |
| `worker.dind.image` | DinD image | `docker:27-dind` |
| `worker.resources` | CPU/memory resource limits | `{}` |

**Note:** `server_url` and `worker_name` are automatically injected via env vars — `server_url` is derived from the Helm release name, and `worker_name` is set to the pod name for uniqueness.

### Infrastructure

| Key | Description | Default |
|-----|-------------|---------|
| `rbac.create` | Create RBAC resources | `true` |
| `serviceAccount.create` | Create service account | `true` |
| `serviceAccount.name` | Service account name override | `""` |

### Ingress

| Key | Description | Default |
|-----|-------------|---------|
| `ingress.enabled` | Enable ingress | `false` |
| `ingress.className` | Ingress class name | `""` |
| `ingress.annotations` | Ingress annotations | `{}` |
| `ingress.hosts` | Ingress host rules | See `values.yaml` |
| `ingress.tls` | TLS configuration | `[]` |

## Examples

### Minimal (dev)

```yaml
server:
  config:
    listen: "0.0.0.0:8080"
    db:
      url: "postgres://stroem:stroem@postgres:5432/stroem"
    log_storage:
      local_dir: /var/stroem/logs
    workspaces:
      default:
        type: folder
        path: /workspace
    worker_token: "dev-token"
```

### Production (secrets via env overrides)

```yaml
server:
  config:
    listen: "0.0.0.0:8080"
    db:
      url: "placeholder"  # overridden by extraSecretEnv
    log_storage:
      local_dir: /var/stroem/logs
    workspaces:
      default:
        type: folder
        path: /workspace
    worker_token: "placeholder"  # overridden by extraSecretEnv
  extraSecretEnv:
    STROEM__DB__URL: "postgres://real-user:real-pass@rds:5432/stroem"
    STROEM__WORKER_TOKEN: "production-secret-token"

worker:
  extraSecretEnv:
    STROEM__WORKER_TOKEN: "production-secret-token"
```

### With Authentication

```yaml
server:
  config:
    listen: "0.0.0.0:8080"
    db:
      url: "placeholder"
    log_storage:
      local_dir: /var/stroem/logs
    workspaces:
      default:
        type: folder
        path: /workspace
    worker_token: "placeholder"
    auth:
      jwt_secret: "placeholder"
      refresh_secret: "placeholder"
      initial_user:
        email: admin@example.com
        password: "placeholder"
  extraSecretEnv:
    STROEM__DB__URL: "postgres://user:pass@db:5432/stroem"
    STROEM__WORKER_TOKEN: "secret-token"
    STROEM__AUTH__JWT_SECRET: "real-jwt-secret"
    STROEM__AUTH__REFRESH_SECRET: "real-refresh-secret"
    STROEM__AUTH__INITIAL_USER__PASSWORD: "real-admin-password"

worker:
  extraSecretEnv:
    STROEM__WORKER_TOKEN: "secret-token"
```

### With Docker-in-Docker

```yaml
worker:
  config:
    server_url: "http://stroem-server:8080"
    worker_token: "placeholder"
    worker_name: "k8s-worker"
    max_concurrent: 4
    poll_interval_secs: 2
    workspace_cache_dir: /var/stroem/workspace-cache
    capabilities:
      - shell
      - docker
  dind:
    enabled: true
    resources:
      limits:
        cpu: "2"
        memory: 4Gi
```

### With KubeRunner

```yaml
worker:
  config:
    server_url: "http://stroem-server:8080"
    worker_token: "placeholder"
    worker_name: "k8s-worker"
    max_concurrent: 4
    poll_interval_secs: 2
    workspace_cache_dir: /var/stroem/workspace-cache
    capabilities:
      - shell
      - kubernetes
    kubernetes:
      namespace: stroem-jobs
rbac:
  create: true
```

### With Ingress

```yaml
ingress:
  enabled: true
  className: nginx
  hosts:
    - host: stroem.example.com
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: stroem-tls
      hosts:
        - stroem.example.com
```
