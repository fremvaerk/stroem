---
title: Helm / Kubernetes
description: Deploy Strøm on Kubernetes with the Helm chart
---

A Helm chart is provided for deploying Strøm on Kubernetes with proper secret management, RBAC, and optional container runners.

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

## Upgrade / Uninstall

```bash
helm upgrade stroem oci://ghcr.io/fremvaerk/charts/stroem --version 1.1.0
helm uninstall stroem
```

## Architecture

The chart uses a **config pass-through** pattern:

- `server.config` and `worker.config` contain the full YAML config passed as-is to ConfigMaps
- Secrets are injected via `extraSecretEnv` as `STROEM__` environment variables that override config values at runtime
- No init containers or sed templating — clean and debuggable

### Environment variable overrides

| Env Var | Overrides YAML key |
|---------|--------------------|
| `STROEM__DB__URL` | `db.url` |
| `STROEM__WORKER_TOKEN` | `worker_token` |
| `STROEM__AUTH__JWT_SECRET` | `auth.jwt_secret` |
| `STROEM__LISTEN` | `listen` |

## Configuration reference

### Server values

| Key | Description | Default |
|-----|-------------|---------|
| `server.image.repository` | Server image repository | `stroem-server` |
| `server.image.tag` | Server image tag | `latest` |
| `server.replicas` | Number of server replicas | `1` |
| `server.service.type` | Kubernetes service type | `ClusterIP` |
| `server.service.port` | Service port | `8080` |
| `server.config` | Full `server-config.yaml` content | See `values.yaml` |
| `server.extraSecretEnv` | Secret env vars for overrides | `{}` |
| `server.extraEnv` | Plain env vars | `{}` |
| `server.resources` | CPU/memory resource limits | `{}` |

### Worker values

| Key | Description | Default |
|-----|-------------|---------|
| `worker.image.repository` | Worker image repository | `stroem-worker` |
| `worker.image.tag` | Worker image tag | `latest` |
| `worker.replicas` | Number of worker replicas | `2` |
| `worker.config` | Full `worker-config.yaml` content | See `values.yaml` |
| `worker.extraSecretEnv` | Secret env vars for overrides | `{}` |
| `worker.extraEnv` | Plain env vars | `{}` |
| `worker.dind.enabled` | Enable Docker-in-Docker sidecar | `false` |
| `worker.dind.image` | DinD image | `docker:27-dind` |
| `worker.resources` | CPU/memory resource limits | `{}` |

`server_url` and `worker_name` are automatically injected — `server_url` is derived from the Helm release name, and `worker_name` is set to the pod name.

### Infrastructure values

| Key | Description | Default |
|-----|-------------|---------|
| `rbac.create` | Create RBAC resources | `true` |
| `serviceAccount.create` | Create service account | `true` |
| `serviceAccount.name` | Service account name override | `""` |

### Ingress values

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
      url: "placeholder"
    log_storage:
      local_dir: /var/stroem/logs
    workspaces:
      default:
        type: folder
        path: /workspace
    worker_token: "placeholder"
  extraSecretEnv:
    STROEM__DB__URL: "postgres://real-user:real-pass@rds:5432/stroem"
    STROEM__WORKER_TOKEN: "production-secret-token"

worker:
  extraSecretEnv:
    STROEM__WORKER_TOKEN: "production-secret-token"
```

### With authentication

```yaml
server:
  config:
    auth:
      jwt_secret: "placeholder"
      refresh_secret: "placeholder"
      initial_user:
        email: admin@example.com
        password: "placeholder"
  extraSecretEnv:
    STROEM__AUTH__JWT_SECRET: "real-jwt-secret"
    STROEM__AUTH__REFRESH_SECRET: "real-refresh-secret"
    STROEM__AUTH__INITIAL_USER__PASSWORD: "real-admin-password"
```

### With Docker-in-Docker

```yaml
worker:
  config:
    tags:
      - shell
      - docker
    docker: {}
  dind:
    enabled: true
    resources:
      limits:
        cpu: "2"
        memory: 4Gi
```

### With Kubernetes runner

```yaml
worker:
  config:
    tags:
      - shell
      - kubernetes
    kubernetes:
      namespace: stroem-jobs
rbac:
  create: true
```

### With ingress

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

### CLI install

```bash
# Basic install
helm install stroem ./helm/stroem \
  --set database.url="postgres://stroem:pw@postgres:5432/stroem" \
  --set workerToken="my-secure-token"

# With Kubernetes runner
helm install stroem ./helm/stroem \
  --set worker.kubernetes.enabled=true \
  --set worker.kubernetes.namespace=stroem-jobs

# With Docker runner
helm install stroem ./helm/stroem \
  --set worker.dind.enabled=true
```
