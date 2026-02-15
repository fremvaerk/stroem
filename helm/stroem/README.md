# Stroem Helm Chart

Helm chart for deploying [Stroem](https://github.com/fremvaerk/stroem) workflow orchestration platform on Kubernetes.

## Prerequisites

- Kubernetes 1.26+
- Helm 3.8+
- PostgreSQL database (external)

## Install from OCI Registry

```bash
# Install latest version
helm install stroem oci://ghcr.io/fremvaerk/charts/stroem-helm

# Install specific version
helm install stroem oci://ghcr.io/fremvaerk/charts/stroem-helm --version 1.0.0

# Install with custom values
helm install stroem oci://ghcr.io/fremvaerk/charts/stroem-helm \
  --set database.url="postgres://user:pass@db:5432/stroem" \
  --set workerToken="my-secret-token"

# Install with values file
helm install stroem oci://ghcr.io/fremvaerk/charts/stroem-helm -f values.yaml
```

## Upgrade

```bash
helm upgrade stroem oci://ghcr.io/fremvaerk/charts/stroem-helm --version 1.1.0
```

## Uninstall

```bash
helm uninstall stroem
```

## Configuration

### Server

| Key | Description | Default |
|-----|-------------|---------|
| `server.image.repository` | Server image repository | `stroem-server` |
| `server.image.tag` | Server image tag | `latest` |
| `server.replicas` | Number of server replicas | `1` |
| `server.service.type` | Kubernetes service type | `ClusterIP` |
| `server.service.port` | Service port | `8080` |
| `server.config.logStorageDir` | Log storage directory | `/var/stroem/logs` |
| `server.config.workspaces` | Workspace definitions | `{default: {type: folder, path: /workspace}}` |
| `server.resources` | CPU/memory resource limits | `{}` |

### Worker

| Key | Description | Default |
|-----|-------------|---------|
| `worker.image.repository` | Worker image repository | `stroem-worker` |
| `worker.image.tag` | Worker image tag | `latest` |
| `worker.replicas` | Number of worker replicas | `2` |
| `worker.config.maxConcurrent` | Max concurrent jobs per worker | `4` |
| `worker.config.pollIntervalSecs` | Server poll interval | `2` |
| `worker.config.capabilities` | Worker capabilities | `["shell"]` |
| `worker.dind.enabled` | Enable Docker-in-Docker sidecar | `false` |
| `worker.dind.image` | DinD image | `docker:27-dind` |
| `worker.kubernetes.enabled` | Enable KubeRunner | `false` |
| `worker.kubernetes.namespace` | Namespace for job pods | `stroem-jobs` |
| `worker.resources` | CPU/memory resource limits | `{}` |

### Authentication

| Key | Description | Default |
|-----|-------------|---------|
| `auth.enabled` | Enable JWT authentication | `false` |
| `auth.jwtSecret` | JWT signing secret | `""` |
| `auth.refreshSecret` | Refresh token signing secret | `""` |
| `auth.baseUrl` | Base URL for OIDC redirects | `""` |
| `auth.initialUser.username` | Initial admin username | `admin` |
| `auth.initialUser.password` | Initial admin password | `""` |
| `auth.providers` | OIDC provider configurations | `[]` |

### Infrastructure

| Key | Description | Default |
|-----|-------------|---------|
| `database.url` | PostgreSQL connection URL | `postgres://stroem:stroem@postgres:5432/stroem` |
| `workerToken` | Shared auth token for workers | `change-me-in-production` |
| `rbac.create` | Create RBAC resources | `true` |
| `serviceAccount.create` | Create service account | `true` |
| `serviceAccount.name` | Service account name override | `""` |

### Ingress

| Key | Description | Default |
|-----|-------------|---------|
| `ingress.enabled` | Enable ingress | `false` |
| `ingress.className` | Ingress class name | `""` |
| `ingress.annotations` | Ingress annotations | `{}` |
| `ingress.hosts` | Ingress host rules | `[{host: stroem.example.com, paths: [{path: /, pathType: Prefix}]}]` |
| `ingress.tls` | TLS configuration | `[]` |

## Examples

### Minimal

```yaml
database:
  url: "postgres://stroem:stroem@postgres:5432/stroem"
workerToken: "my-secret-token"
```

### With Authentication

```yaml
database:
  url: "postgres://stroem:stroem@postgres:5432/stroem"
workerToken: "my-secret-token"
auth:
  enabled: true
  jwtSecret: "generate-a-random-secret"
  refreshSecret: "generate-another-random-secret"
  initialUser:
    username: admin
    password: "admin-password"
```

### With OIDC

```yaml
database:
  url: "postgres://stroem:stroem@postgres:5432/stroem"
workerToken: "my-secret-token"
auth:
  enabled: true
  jwtSecret: "generate-a-random-secret"
  refreshSecret: "generate-another-random-secret"
  baseUrl: "https://stroem.example.com"
  providers:
    - name: google
      displayName: Google
      providerType: oidc
      issuerUrl: https://accounts.google.com
      clientId: "your-client-id"
      clientSecret: "your-client-secret"
```

### With Docker-in-Docker

```yaml
database:
  url: "postgres://stroem:stroem@postgres:5432/stroem"
workerToken: "my-secret-token"
worker:
  config:
    capabilities: ["shell", "docker"]
  dind:
    enabled: true
    resources:
      limits:
        cpu: "2"
        memory: 4Gi
```

### With KubeRunner

```yaml
database:
  url: "postgres://stroem:stroem@postgres:5432/stroem"
workerToken: "my-secret-token"
worker:
  config:
    capabilities: ["shell", "kubernetes"]
  kubernetes:
    enabled: true
    namespace: stroem-jobs
rbac:
  create: true
```

### With Ingress

```yaml
database:
  url: "postgres://stroem:stroem@postgres:5432/stroem"
workerToken: "my-secret-token"
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
