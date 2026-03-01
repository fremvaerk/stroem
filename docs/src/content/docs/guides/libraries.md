---
title: Libraries
description: Share actions, tasks, and connection types across workspaces using Git repositories or local folders
---

Libraries let you import shared **actions**, **tasks**, and **connection types** from external Git repositories or local folders. Library items are available to all workspaces and are referenced using a dot-prefixed namespace: `library_name.item_name`.

## Configuration

Libraries are defined in the server configuration file (`server-config.yaml`), not in workspace YAML. All configured libraries are shared across all workspaces.

### Git Libraries

```yaml
libraries:
  common:
    type: git
    url: https://github.com/org/stroem-common-library.git
    ref: v1.2.0              # tag, branch, or commit SHA
    auth: my-git-token       # optional, references a git_auth entry

git_auth:
  my-git-token:
    type: token
    token: "${GITHUB_TOKEN}"
```

**Pinned refs** (tags, commit SHAs) are cloned once and cached. **Branch refs** are fetched on every workspace reload, with a warning logged about floating refs.

### Folder Libraries

For local development or shared network mounts:

```yaml
libraries:
  local-lib:
    type: folder
    path: /shared/stroem-libraries/notifications
```

### Git Authentication

Authentication credentials are defined in the `git_auth` section and referenced by name:

```yaml
git_auth:
  my-github-token:
    type: token
    token: "${GITHUB_TOKEN}"

  my-ssh-key:
    type: ssh_key
    private_key_path: /path/to/id_rsa
```

Credentials can be injected via environment variables using the `STROEM__` prefix:

```bash
STROEM__GIT_AUTH__MY_GITHUB_TOKEN__TOKEN=ghp_xxx
```

## Library Repository Structure

A library repository has the same structure as a workspace. Only **actions**, **tasks**, and **connection types** are imported. Triggers, secrets, and connections are ignored (they remain workspace-local).

```
stroem-common-library/
├── .workflows/
│   ├── actions.yaml           # action definitions
│   ├── tasks.yaml             # task definitions
│   └── connection-types.yaml  # connection type definitions
└── scripts/
    ├── slack-notify.sh
    └── canary-deploy.sh
```

## Referencing Library Items

Library items use dot-separated names: `library_name.item_name`.

```yaml
actions:
  run-canary:
    type: task
    task: common.canary-deploy         # library task as sub-job

tasks:
  deploy:
    flow:
      notify:
        action: common.slack-notify    # library action
      canary:
        action: run-canary             # local action referencing library task
      build:
        action: local-action           # local action (no prefix)
```

Unprefixed names always resolve to the local workspace. Name collisions between libraries are impossible because the prefix is mandatory.

### Connection Types from Libraries

Library connection types work with the existing connections system:

```yaml
# Server config provides: common library with "postgres" connection type
# Workspace YAML:
connections:
  prod_db:
    type: common.postgres              # library connection type
    host: "db.example.com"
    port: 5432
    database: "myapp"
```

Task inputs with connection-type references work the same way — the UI renders a dropdown for connections of the specified type.

## How Prefixing Works

Within a library's own YAML, items reference each other without a prefix:

```yaml
# Inside library "common"'s tasks.yaml:
tasks:
  canary-deploy:
    flow:
      notify:
        action: slack-notify       # references library-local action
      deploy:
        action: kubectl-apply      # another library-local action
```

During import, the resolver automatically:

1. Prefixes all items: `slack-notify` → `common.slack-notify`
2. Rewrites internal references: `action: slack-notify` → `action: common.slack-notify`
3. Rewrites task references: `task: rollback` → `task: common.rollback`
4. Rewrites connection-type input references similarly

References to unknown names (not in the library) are left as-is, allowing library tasks to reference workspace-local actions.

## Validation

The CLI `stroem validate` command runs without library context. Names containing `.` are assumed to be library references and produce a warning rather than an error:

```
Warning: Task 'deploy' step 'notify' references library action 'common.slack-notify' (skipped)
```

Server-side validation (after library resolution) validates fully — all prefixed names must exist.

## Path Resolution at Runtime

1. The server builds workspace tarballs including library source files under `_libraries/{library_name}/`
2. Workers extract the tarball and use it for step execution
3. Scripts from library actions are resolved against `_libraries/{library_name}/` instead of the workspace root
4. `cmd` commands execute in the workspace root regardless of library origin
