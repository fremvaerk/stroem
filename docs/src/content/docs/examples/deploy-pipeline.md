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
    type: script
    script: "echo 'Checking system status...' && sleep 1 && echo 'OUTPUT: {\"healthy\": true}'"

  deploy:
    type: script
    source: actions/deploy.sh
    input:
      env: { type: string, default: "staging" }

  notify:
    type: script
    script: "echo 'Notification: Deployment to {{ input.env }} completed with status={{ input.status }}'"
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

- **Script files** referenced via `source:` (relative to workspace root)
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

## Multi-region deploy with `for_each`

Fan out the deployment across multiple regions in parallel using [`for_each`](/guides/loops/):

```yaml
actions:
  check-status:
    type: script
    script: "echo 'Checking system status...' && echo 'OUTPUT: {\"healthy\": true}'"

  deploy:
    type: script
    script: |
      echo "Deploying to {{ input.region }} ({{ input.env }})..."
      sleep 2
      echo "OUTPUT: {\"region\": \"{{ input.region }}\", \"status\": \"deployed\"}"
    input:
      env: { type: string }
      region: { type: string }

  notify:
    type: script
    script: "echo 'Deployed to {{ input.regions | length }} regions: {{ input.status }}'"
    input:
      regions: { type: string }
      status: { type: string }

tasks:
  multi-region-deploy:
    mode: distributed
    input:
      env: { type: string, default: "staging" }
      regions: { type: string, default: '["us-east-1", "eu-west-1", "ap-south-1"]' }
    flow:
      health-check:
        action: check-status
      deploy-region:
        action: deploy
        depends_on: [health-check]
        for_each: "{{ input.regions }}"
        input:
          env: "{{ input.env }}"
          region: "{{ each.item }}"
      send-notification:
        action: notify
        depends_on: [deploy-region]
        input:
          regions: "{{ deploy_region.output }}"
          status: "success"
```

This creates `deploy-region[0]`, `deploy-region[1]`, `deploy-region[2]` — all running in parallel after the health check passes. The notification step waits for all regions to complete and receives the aggregated output array.

To deploy regions one at a time (e.g., canary rollout), add `sequential: true`:

```yaml
      deploy-region:
        action: deploy
        depends_on: [health-check]
        for_each: "{{ input.regions }}"
        sequential: true
        input:
          env: "{{ input.env }}"
          region: "{{ each.item }}"
```

### Running it

```bash
# Deploy to default regions
stroem trigger multi-region-deploy

# Deploy to specific regions in production
stroem trigger multi-region-deploy \
  --input '{"env": "production", "regions": "[\"us-east-1\", \"eu-west-1\"]"}'
```

## Sub-job fan-out with `type: task` + `for_each`

When each iteration needs multiple steps, use a [`type: task`](/guides/workflow-basics/#tasks) action so each loop item spawns a full child job with its own steps and logs:

```yaml
actions:
  check-region:
    type: script
    script: |
      echo "Health-checking {{ input.region }}..."
      echo 'OUTPUT: {"healthy": true}'
    input:
      region: { type: string }

  deploy-to-region:
    type: script
    script: |
      echo "Deploying {{ input.env }} to {{ input.region }}..."
      sleep 2
      echo 'OUTPUT: {"status": "deployed"}'
    input:
      env: { type: string }
      region: { type: string }

  smoke-test:
    type: script
    script: |
      echo "Running smoke tests in {{ input.region }}..."
      echo 'OUTPUT: {"passed": true}'
    input:
      region: { type: string }

  # Task action — references the region-deploy task below
  run-region-deploy:
    type: task
    task: region-deploy

tasks:
  # Parent task: fans out across regions
  deploy-all-regions:
    mode: distributed
    input:
      env: { type: string, default: "staging" }
      regions: { type: string, default: '["us-east-1", "eu-west-1", "ap-south-1"]' }
    flow:
      deploy:
        action: run-region-deploy
        for_each: "{{ input.regions }}"
        input:
          env: "{{ input.env }}"
          region: "{{ each.item }}"

  # Child task: multi-step deploy for a single region
  region-deploy:
    mode: distributed
    input:
      env: { type: string }
      region: { type: string }
    flow:
      health-check:
        action: check-region
        input:
          region: "{{ input.region }}"
      deploy:
        action: deploy-to-region
        depends_on: [health-check]
        input:
          env: "{{ input.env }}"
          region: "{{ input.region }}"
      verify:
        action: smoke-test
        depends_on: [deploy]
        input:
          region: "{{ input.region }}"
```

Each region gets its own child job (`region-deploy`) with three steps: health check → deploy → smoke test. All three child jobs run in parallel. The parent `deploy` step fans in when all child jobs complete.

This pattern is useful when each iteration involves complex multi-step logic, or when you want separate log streams and status tracking per region in the UI.

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
