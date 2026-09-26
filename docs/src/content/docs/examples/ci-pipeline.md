---
title: CI Pipeline
description: A CI workflow with parallel test and lint steps triggered by webhook
---

This example creates a CI pipeline triggered by GitHub webhooks with parallel lint and test steps.

## Workflow file

Create `workspace/.workflows/ci.yaml`:

```yaml
actions:
  lint:
    type: script
    runner: docker
    script: |
      cd /workspace
      npm ci
      npm run lint

  test:
    type: script
    runner: docker
    script: |
      cd /workspace
      npm ci
      npm test

  build:
    type: script
    runner: docker
    script: |
      cd /workspace
      npm ci
      npm run build
      echo "OUTPUT: {\"artifact\": \"dist/app.tar.gz\"}"

  notify-ci:
    type: script
    script: |
      echo "CI result: {{ input.status }} for job {{ input.job_id }}"
      # In production, call Slack/Teams/GitHub API here

    input:
      status: { type: string }
      job_id: { type: string }

tasks:
  ci-pipeline:
    mode: distributed
    input:
      body: { type: string }
    flow:
      lint:
        action: lint
      test:
        action: test
      # lint and test run in parallel (no mutual dependencies)
      build:
        action: build
        depends_on: [lint, test]
    on_success:
      - action: notify-ci
        input:
          status: "success"
          job_id: "{{ hook.job_id }}"
    on_error:
      - action: notify-ci
        input:
          status: "failed: {{ hook.error_message }}"
          job_id: "{{ hook.job_id }}"

triggers:
  github-push:
    type: webhook
    name: github-ci
    task: ci-pipeline
    secret: "whsec_your_secret_here"
    enabled: true
```

## What this does

1. **`lint`** and **`test`** run in parallel (no dependency between them)
2. **`build`** waits for both lint and test to complete
3. An `on_success` hook notifies when the whole pipeline passes; an `on_error` hook notifies when any step fails — there is no dependent-side "run even if upstream failed" step; that belongs in a hook (see [Hooks](/guides/hooks/))

The DAG looks like:

```
lint  ──┐
        ├──> build
test  ──┘
```

## Key concepts demonstrated

- **Parallel steps**: `lint` and `test` have no mutual dependencies, so they run concurrently
- **Docker runner**: `runner: docker` runs steps inside containers with workspace at `/workspace`
- **Hooks, not a dependent step**: notifying after success or failure is an `on_success` / `on_error` hook, not a flow step with `continue_on_failure`. That flag decides whether a step's *own* dependents run, and it catches every failure reaching it — its own, and any upstream failure whose only path runs through it — so the job no longer fails; it never decides whether the flagged step itself runs. A 0.16-style `notify` step depending on `build` with `continue_on_failure` would, under 0.17's rule, catch a failed `build` and turn the whole pipeline **green** — the opposite of a CI notification's job. That is why this page puts notification in hooks instead.
- **Webhook trigger**: External systems (GitHub) can trigger the pipeline via `POST /hooks/github-ci`
- **Webhook input**: `input.body.*` (e.g. `{{ input.body.ref }}`) accesses the parsed JSON body from the webhook request inside flow step templates — hook templates don't see it, only `hook.*` / `secret.*` (see [Hooks](/guides/hooks/))

## Setting up the GitHub webhook

1. Go to your GitHub repo → Settings → Webhooks → Add webhook
2. Set the Payload URL to `https://stroem.example.com/hooks/github-ci?secret=whsec_your_secret_here`
3. Set Content type to `application/json`
4. Select "Just the push event"

Now every push to the repository triggers the CI pipeline.

## Running manually

```bash
# Via API (simulating a webhook payload)
curl -X POST http://localhost:8080/api/workspaces/default/tasks/ci-pipeline/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"body": {"ref": "refs/heads/main"}}}'

# Via CLI
stroem trigger ci-pipeline --input '{"body": {"ref": "refs/heads/main"}}'
```

## Test matrix with `for_each`

Use [`for_each`](/guides/loops/) to run the same tests across multiple Node.js versions:

```yaml
actions:
  lint:
    type: script
    runner: docker
    script: |
      cd /workspace
      npm ci
      npm run lint

  test-version:
    type: docker
    image: "node:{{ input.node_version }}"
    cmd: |
      cd /app && npm ci && npm test &&
      echo "OUTPUT: {\"node\": \"{{ input.node_version }}\", \"passed\": true}"
    input:
      node_version: { type: string }

  build:
    type: script
    runner: docker
    script: |
      cd /workspace
      npm ci
      npm run build
      echo "OUTPUT: {\"artifact\": \"dist/app.tar.gz\"}"

  notify-ci:
    type: script
    script: |
      echo "CI result: {{ input.status }}"
    input:
      status: { type: string }

tasks:
  ci-matrix:
    mode: distributed
    input:
      body: { type: string }
    flow:
      lint:
        action: lint
      test:
        action: test-version
        for_each: ["18", "20", "22"]
        input:
          node_version: "{{ each.item }}"
      # lint and test run in parallel; build waits for both
      build:
        action: build
        depends_on: [lint, test]
    on_success:
      - action: notify-ci
        input:
          status: "success"
    on_error:
      - action: notify-ci
        input:
          status: "failed: {{ hook.error_message }}"
```

This creates `test[0]` (Node 18), `test[1]` (Node 20), `test[2]` (Node 22) — all running in parallel alongside lint. The `test-version` action uses `type: docker` to run each Node.js version's image directly — note that `type: docker` does **not** mount workspace files, so the application source must be baked into the image. The `lint` and `build` actions use `type: script` + `runner: docker` instead, which mounts the workspace at `/workspace`. Hooks only see the `hook.*` / `secret.*` template context, not individual step outputs — `hook.error_message` and `hook.failed_steps` cover the failure case; to notify with a step's own output, add a step to the flow instead of a hook.
