---
title: Multi-Workspace
description: Managing multiple workflow sources with folder and git workspaces
---

Strøm supports multiple workspaces, each with its own set of workflow files. Workspaces are configured in `server-config.yaml`.

## Configuration

```yaml
workspaces:
  default:
    type: folder
    path: ./workspace
  data-team:
    type: git
    url: https://github.com/org/data-workflows.git
    ref: main
    poll_interval_secs: 60
```

## Workspace types

### Folder source

Loads workflow files from a local directory path. Computes a content hash as the revision for tarball caching. Polls for changes every 30 seconds using file metadata hashing.

```yaml
workspaces:
  default:
    type: folder
    path: ./workspace
```

### Git source

Clones a git repository and loads workflow files from it. Supports SSH key and token authentication.

```yaml
workspaces:
  data-team:
    type: git
    url: https://github.com/org/data-workflows.git
    ref: main
    poll_interval_secs: 60
    auth:
      type: token
      token: "ghp_xxx"
```

Git workspaces use `poll_interval_secs` (default: 60) to control how often the server checks for new commits. The check uses a lightweight ls-remote operation — only when the remote HEAD actually changes does the server perform a full fetch and reload.

## API routes

Each workspace is independent — tasks, actions, and scripts are scoped to their workspace. Tasks are accessed via workspace-scoped API routes:

```bash
# List tasks in a specific workspace
curl http://localhost:8080/api/workspaces/data-team/tasks

# Trigger a task in a specific workspace
curl -X POST http://localhost:8080/api/workspaces/data-team/tasks/etl-pipeline/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"date": "2025-01-01"}}'
```

## CLI usage

```bash
# List all workspaces
stroem workspaces

# List tasks in a specific workspace
stroem tasks --workspace data-team

# Trigger a task in a specific workspace
stroem trigger etl-pipeline --workspace data-team --input '{"date": "2025-01-01"}'
```

## Worker behavior

Workers automatically download the correct workspace files before executing each step. Workspace tarballs are cached locally using ETag-based caching, so workers only re-download when a workspace changes.

Configure the local cache directory in the worker config:

```yaml
workspace_cache_dir: /var/stroem/workspace-cache
```
