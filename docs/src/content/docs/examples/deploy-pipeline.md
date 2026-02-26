---
title: Deploy Pipeline
description: A three-step deployment workflow with health check, deploy, and notification
---

This example creates a deployment pipeline with a health check, deployment step, and a notification.

## Workflow file

Create `workspace/.workflows/deploy.yaml`:

```yaml
actions:
  check-status:
    type: shell
    cmd: "echo 'Checking system status...' && sleep 1 && echo 'OUTPUT: {\"healthy\": true}'"

  deploy:
    type: shell
    script: actions/deploy.sh
    input:
      env: { type: string, default: "staging" }

  notify:
    type: shell
    cmd: "echo 'Notification: Deployment to {{ input.env }} completed with status={{ input.status }}'"
    input:
      env: { type: string }
      status: { type: string }

tasks:
  deploy-pipeline:
    mode: distributed
    input:
      env: { type: string, default: "staging" }
    flow:
      health-check:
        action: check-status
      deploy-app:
        action: deploy
        depends_on: [health-check]
        input:
          env: "{{ input.env }}"
      send-notification:
        action: notify
        depends_on: [deploy-app]
        input:
          env: "{{ input.env }}"
          status: "success"
```

## Deploy script

Create `workspace/actions/deploy.sh`:

```bash
#!/bin/bash
set -euo pipefail

echo "Deploying to environment: ${DEPLOY_ENV:-staging}"
echo "Version: ${VERSION:-latest}"
sleep 1
echo "Deployment complete!"
echo "OUTPUT: {\"status\": \"deployed\", \"env\": \"${DEPLOY_ENV:-staging}\"}"
```

## What this does

1. **`health-check`** verifies the system is healthy before deploying
2. **`deploy-app`** runs the deployment script (depends on health check)
3. **`send-notification`** sends a notification after deployment (depends on deploy)

The steps execute sequentially: `health-check` → `deploy-app` → `send-notification`.

## Key concepts demonstrated

- **Script files** referenced via `script:` (relative to workspace root)
- **Linear dependencies** create a sequential pipeline
- **Input propagation** passes task input down to step actions
- **Multiple actions** in a single workflow file

## Running it

```bash
# Deploy to staging (default)
stroem trigger deploy-pipeline

# Deploy to production
stroem trigger deploy-pipeline --input '{"env": "production"}'
```

## Adding hooks

Enhance the pipeline with success/failure notifications:

```yaml
tasks:
  deploy-pipeline:
    flow:
      health-check:
        action: check-status
      deploy-app:
        action: deploy
        depends_on: [health-check]
        input:
          env: "{{ input.env }}"
      send-notification:
        action: notify
        depends_on: [deploy-app]
        input:
          env: "{{ input.env }}"
          status: "success"
    on_error:
      - action: notify
        input:
          env: "{{ hook.workspace }}"
          status: "FAILED: {{ hook.error_message }}"
```
