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

At startup, and on every reload, all configured workspaces are loaded concurrently rather than one at a time, so a single slow or misbehaving source doesn't delay the others. If a git workspace's clone/fetch fails with an error containing "credential rejected by remote", it means the configured deploy key or token is not authorized for that repository — the server fails that attempt immediately instead of letting it retry for over a minute.

## Disabling triggers per server

Set `triggers: false` on a workspace entry to load it without firing any of its triggers on that server. Cron schedules are not scheduled, webhook names are not routed, and event-source consumers are not started (running consumers are cancelled on the next reconcile). Tasks and actions load normally and can still be run manually from the UI, CLI, API, or MCP.

```yaml
workspaces:
  analytics:
    type: git
    url: https://github.com/org/data-workflows.git
    ref: main
    triggers: false   # this server never fires analytics' schedules/webhooks/event sources
```

Works for both `folder` and `git` sources. The default is `true`. The usual env override applies, with the workspace name as the middle segment (env keys are lower-cased before matching, so the name must otherwise be spelled exactly as in the config; a name containing `-` needs the hyphen in the variable name too):

```bash
STROEM__WORKSPACES__ANALYTICS__TRIGGERS=false
```

Typical use: a staging or developer server that loads the same repository as production so its tasks can be exercised by hand, without a second copy of production's scheduled work running against the same external systems.

This is a server-side setting, so it does not travel with the repository. To turn off a single trigger everywhere, use the trigger's own `enabled: false` in the workspace YAML instead (see [Triggers](/guides/triggers/)).

The workspace list (`GET /api/workspaces`, the UI Workspaces page, and `stroem-api workspaces`) reports `triggers_enabled: false` for such workspaces; the triggers themselves remain listed so you can see what would fire elsewhere.

## Browsing workspaces in the UI

The web UI treats a workspace as a navigation level of its own:

- **Workspaces** (`/workspaces`) lists every configured workspace with its task, action and trigger counts, revision, and any load error or warning. Each name links to the workspace page.
- **Workspace page** (`/workspaces/<name>`) shows that workspace only: header with revision and counts, a **Refresh** button, the task tree (folders collapsible), and the workspace's triggers with their schedule and next run. A workspace that failed to load still gets a page with its error, so a shared link never lands on an empty screen.
- **Tasks** (`/tasks`) is the cross-workspace list. When more than one workspace is loaded, a **Merged / By workspace** switch appears next to the search box. *Merged* shows one folder tree with a Workspace column; *By workspace* nests each workspace's folders under a collapsible workspace row. The choice, collapsed workspaces and open folders are remembered in the browser.
- Breadcrumbs on a task page read `Workspaces / <workspace> / <task>`, and the workspace name shown on a job page links to its workspace.

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
