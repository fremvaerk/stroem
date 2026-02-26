---
title: Startup Scripts
description: Customizing worker and runner environments at startup
---

Worker and runner containers support startup scripts via the `/etc/stroem/startup.d/` convention. Any `*.sh` files in that directory are `source`d (not executed in a subshell) before the main process starts, so environment variables set in startup scripts are available to the process.

## Use cases

- Install extra tools or packages at runtime
- Configure cloud CLI credentials (AWS, GCP, Azure)
- Import custom CA certificates
- Set up SSH keys or other authentication

## How it works

Both worker and runner Docker images include an entrypoint script that:

1. Sources all `*.sh` files in `/etc/stroem/startup.d/` (sorted alphabetically)
2. Runs the main process via `exec "$@"`

If the directory doesn't exist or is empty, the entrypoint proceeds directly to the main process.

## Helm configuration

The Helm chart provides three `startupScript` values:

```yaml
# Shared startup script — runs in both worker and runner containers
startupScript: |
  echo "Installing CA cert..."
  cp /mnt/certs/ca.pem /usr/local/share/ca-certificates/
  update-ca-certificates

# Worker-specific startup script — runs after the shared script
worker:
  startupScript: |
    echo "Configuring AWS..."
    aws configure set region us-east-1

# Runner-specific startup script — runs after the shared script
runner:
  startupScript: |
    echo "Installing Node.js..."
    curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
    apt-get install -y nodejs
```

The shared `startupScript` becomes `00-common.sh`, worker-specific becomes `10-worker.sh`, and runner-specific becomes `10-runner.sh`. Alphabetical ordering ensures the shared script always runs first.

Worker pods mount the worker startup ConfigMap. Runner containers (Docker via DinD or Kubernetes pods) mount the runner startup ConfigMap.

## Manual setup (without Helm)

Place shell scripts in `/etc/stroem/startup.d/` on the Docker host or bind-mount them into containers:

```bash
# Create a startup script
echo 'export MY_VAR=hello' > /etc/stroem/startup.d/setup.sh
chmod +x /etc/stroem/startup.d/setup.sh

# The worker/runner entrypoint will source it automatically
```

For Kubernetes without Helm, create a ConfigMap and mount it at `/etc/stroem/startup.d/`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: stroem-runner-startup
data:
  00-setup.sh: |
    export MY_TOOL_VERSION=1.2.3
```

Then set `runner_startup_configmap: stroem-runner-startup` in the worker's Kubernetes config to inject the ConfigMap into runner pods automatically.
