---
title: Authorization
description: Access control lists (ACL), admin users, and group management
---

Authorization in Strøm is **optional** and **config-driven**. When no `acl` section is present in `server-config.yaml`, all authenticated users have full access (backward compatible). Adding the `acl` section enables fine-grained access control.

## Core concepts

**Admin users** bypass all ACL rules and always have full access. They can manage other users, toggle admin status, and assign groups.

**Groups** are named sets of users (e.g., `devops`, `engineering`, `qa`). Groups are stored in the database and managed via the UI or API. You reference them in ACL rules without pre-creating them.

**ACL rules** define what users/groups can do with which workspaces and tasks. Each rule specifies an action: `run` (full access), `view` (read-only), or `deny` (invisible).

## Server configuration

```yaml
acl:
  default: deny    # What happens when no rule matches: deny | view | run
  rules:
    - workspace: "production"
      tasks: ["deploy/*"]
      action: run
      groups: [devops]
      users: [contractor@ext.com]
    - workspace: "*"
      tasks: ["*"]
      action: view
      groups: [engineering]
```

### Fields

| Field | Description |
|-------|-------------|
| `default` | Default action when no rule matches: `deny`, `view`, or `run`. Defaults to `deny`. |
| `rules` | List of ACL rules (each with workspace, tasks, action, groups, users) |
| `workspace` | Exact workspace name or `*` (all workspaces). Supports `*` wildcard. |
| `tasks` | List of task patterns. Task path is `"{folder}/{task_name}"` when a folder is set, or just `"{task_name}"`. Supports `*` wildcard (e.g., `["deploy/*", "rollback"]`). |
| `action` | `run` (full access), `view` (read-only), or `deny` (invisible). |
| `groups` | List of group names to match (OR'd with users). |
| `users` | List of user email addresses to match (OR'd with groups). |

## Evaluation logic

1. If user is **admin** → always `Run` (full access)
2. If **no ACL is configured** → always `Run` (backward compatible)
3. Check all matching rules (workspace + tasks match) → **highest permission wins** (Run > View > Deny)
   - Order of rules in config doesn't matter
   - If a rule matches workspace and task patterns, check if user/groups match
4. If no rules matched → use `acl.default` (defaults to `deny`)

## Jobs and task folders

A job is authorised by the task path of the task it ran, `"{folder}/{task}"`:

- An ordinary job uses the task's folder in the workspace's **current**
  configuration.
- A job that runs on a [git ref](/guides/git-refs/) uses the folder its **own
  commit** declared for the task (the job's `task_folder`), never the current
  one — even when a task of the same name exists on the default branch under
  another folder. Two refs of one task that declare different folders are
  authorised independently.

This applies to job detail, logs (REST, WebSocket stream, MCP), artifacts,
cancel, approve, restart and re-run, the recent steps on a worker's detail
page, and to the job list, dashboard counts and MCP `list_jobs`, which are
filtered before pagination.

A single-step hook job has the task name `_hook:<action>` and no folder, so
a rule matching `_hook:*` at the root grants access to the hook payloads of
that workspace — including the step errors of the jobs that fired them. A
`type: task` hook job is authorised by its task's folder.

The child-job links in a parent's job detail (`child_jobs[]`: id, workspace,
task name, status, ref, revision) are shown to anyone who can see the
parent; the child's own detail, logs and artifacts need access to the
child's task.

## Permission levels

| Permission | See in list | Execute/cancel | View logs |
|-----------|------------|----------------|-----------|
| `Run`     | ✓          | ✓              | ✓         |
| `View`    | ✓          | ✗              | ✓         |
| `Deny`    | ✗          | ✗              | ✗         |

## Admin users

The initial user (from `auth.initial_user` in config) is automatically promoted to admin on startup.

When OIDC SSO is enabled, the first user to sign in becomes admin if no users exist yet.

Admins can:
- View and manage all users and groups (Users page)
- View and manage workers (Workers page)
- Toggle admin status on other users
- Create, update, and delete user groups

### Toggling admin status

Via the UI:
1. Navigate to **Users** page (admin only)
2. Click on a user
3. Toggle the **Administrator** switch

Via the API:
```bash
curl -X PUT https://stroem.example.com/api/users/{id}/admin \
  -H "Authorization: Bearer <jwt-token>" \
  -H "Content-Type: application/json" \
  -d '{"is_admin": true}'
```

## Managing groups

Groups are named sets of users. Create them by assigning them to users — they're materialized on first use.

### Via the UI

1. Navigate to **Users** page (admin only)
2. Click on a user
3. In the **Administration** card, find the **Groups** section
4. Type a group name and click "Add" (existing groups auto-suggest)
5. Click the X next to a group to remove it
6. Changes are saved immediately

### Via the API

```bash
# Get a user's groups
curl https://stroem.example.com/api/users/{id}/groups \
  -H "Authorization: Bearer <jwt-token>"

# Set a user's groups
curl -X PUT https://stroem.example.com/api/users/{id}/groups \
  -H "Authorization: Bearer <jwt-token>" \
  -H "Content-Type: application/json" \
  -d '{"groups": ["devops", "engineering"]}'

# List all distinct group names
curl https://stroem.example.com/api/groups \
  -H "Authorization: Bearer <jwt-token>"
```

Group names must be 1-64 characters, alphanumeric with underscores and hyphens.

## Examples

### Team-based access

```yaml
acl:
  default: deny
  rules:
    # DevOps team can run everything everywhere
    - workspace: "*"
      tasks: ["*"]
      action: run
      groups: [devops]
    # Backend team can run tasks in staging, view production
    - workspace: "staging"
      tasks: ["*"]
      action: run
      groups: [backend]
    - workspace: "production"
      tasks: ["*"]
      action: view
      groups: [backend]
    # QA can run test tasks anywhere
    - workspace: "*"
      tasks: ["test/*", "qa/*"]
      action: run
      groups: [qa]
```

### Folder-based task access

```yaml
acl:
  default: deny
  rules:
    # Contractors can only run specific tasks
    - workspace: "production"
      tasks: ["reports/daily-summary", "reports/weekly-digest"]
      action: run
      users: [contractor@ext.com]
    # Everyone else gets view-only
    - workspace: "*"
      tasks: ["*"]
      action: view
      groups: [employees]
```

### Environment-based access

```yaml
acl:
  default: deny
  rules:
    # Developers have full access to dev workspace
    - workspace: "dev"
      tasks: ["*"]
      action: run
      groups: [developers]
    # Staging is open for experimentation
    - workspace: "staging"
      tasks: ["*"]
      action: run
      groups: [developers, qa]
    # Production is tightly controlled
    - workspace: "production"
      tasks: ["deploy/*"]
      action: run
      groups: [devops]
    - workspace: "production"
      tasks: ["*"]
      action: view
      groups: [developers, qa]
```

## Token expiration behavior

When you revoke a user's admin status or change their group assignments, those changes take effect immediately for new requests.

**However**, existing JWT access tokens (15-minute TTL) remain valid until they expire. If you need to revoke access immediately, the user's token will still work for up to 15 minutes.

API keys reflect changes immediately — they are validated against the database on each request.

## Helm deployment

Use the `acl` section in the server config:

```yaml
server:
  config:
    acl:
      default: deny
      rules:
        - workspace: "*"
          tasks: ["*"]
          action: run
          groups: [admin-team]
        - workspace: "*"
          tasks: ["*"]
          action: view
          groups: [engineering]
```

If using Helm secrets, remember that the ACL config is baked into the ConfigMap (it's not secret). Group assignments are stored in the database and managed via API.

## Known limitations

- **Webhooks** (`/hooks/{name}`) use their own secret-based authentication, not user ACL. They are not subject to ACL rules. That includes the webhook job-status poll (`/hooks/{name}/jobs/{job_id}`), which returns the (masked) output of the jobs that webhook created.
- **Re-run of a job on a git ref**: a caller who already holds a job's id can tell from the status code whether it exists and whether it runs on a ref (unknown → `400`, pinned and denied → `404`, unpinned and denied → `403`). No job data is returned.
- **JWT token TTL**: After revoking admin status or changing groups, existing tokens remain valid for up to 15 minutes (the access token TTL). This is by design to avoid excessive database queries on each request.

## API reference

| Endpoint | Method | Description | Auth |
|----------|--------|-------------|------|
| `/api/users` | GET | List all users | JWT (admin) |
| `/api/users/{id}` | GET | Get user detail | JWT (admin) |
| `/api/users/{id}/admin` | PUT | Set admin flag | JWT (admin) |
| `/api/users/{id}/groups` | GET | Get user's groups | JWT (admin) |
| `/api/users/{id}/groups` | PUT | Set user's groups | JWT (admin) |
| `/api/groups` | GET | List all group names | JWT (admin) |

See [Auth API](/reference/auth-api/) for full endpoint details.
