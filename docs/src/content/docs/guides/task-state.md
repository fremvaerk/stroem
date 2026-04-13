---
title: Task State Snapshots
description: Persisting files and structured data between task runs
---

Tasks in Strøm can persist both files and structured data between runs using state snapshots. This enables workflows like SSL certificate renewal, incremental processing with cursors, and caching build artifacts.

## Overview

State snapshots are immutable archives created after a step completes, containing:

- **Files**: Any artifacts written to `/state-out` (certificates, keys, compiled binaries, caches)
- **Structured data**: JSON objects emitted via the `STATE:` protocol (counters, cursors, metadata, timestamps)

Each task maintains a rolling history of state snapshots. When a new job starts, all steps have access to the latest snapshot from the previous run.

## How it works

### Writing state

State is written in two ways:

#### File-based state

Write files to the `/state-out` directory (or `$STATE_OUT_DIR` environment variable):

```bash
#!/bin/bash
# Renew SSL certificate
openssl req -new -x509 -days 365 -nodes \
  -out /etc/letsencrypt/live/example.com/fullchain.pem \
  -keyout /etc/letsencrypt/live/example.com/privkey.pem

# Persist the certificate for the next job
cp /etc/letsencrypt/live/example.com/fullchain.pem $STATE_OUT_DIR/cert.pem
cp /etc/letsencrypt/live/example.com/privkey.pem $STATE_OUT_DIR/privkey.pem

echo "Certificate renewed"
```

Files are automatically collected into a gzip tarball and stored after the step completes.

#### Structured state (STATE: protocol)

Emit JSON to stdout using the `STATE:` prefix to store structured data:

```bash
#!/bin/bash
# Check certificate expiry and emit state
DAYS=$(openssl x509 -in /etc/letsencrypt/live/example.com/fullchain.pem \
  -noout -enddate | cut -d= -f2)

echo "STATE: {\"expiry_date\": \"$DAYS\", \"checked_at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"}"
echo "STATE: {\"domain\": \"example.com\"}"  # Multiple lines are deep-merged
```

Multiple `STATE:` lines in the same step are automatically merged (last write wins for duplicate keys):

```bash
echo "STATE: {\"cursor\": \"page-1\", \"total_pages\": 50}"
echo "STATE: {\"cursor\": \"page-2\"}"  # Overwrites cursor, keeps total_pages
# Result: {"cursor": "page-2", "total_pages": 50}
```

Both approaches work together — files and structured state are stored in the same snapshot. File state becomes `state.json` in the tarball.

### Reading state

#### Previous files

Access the previous state directory via the `/state` mount (or `$STATE_DIR` environment variable):

```bash
#!/bin/bash
# Check if a certificate exists from a previous run
if [ -f "$STATE_DIR/cert.pem" ]; then
    echo "Found previous certificate, checking expiry..."
    openssl x509 -in "$STATE_DIR/cert.pem" -noout -enddate
else
    echo "No previous certificate found"
fi
```

The `/state` directory is read-only and contains the contents of the latest snapshot.

#### In templates

Structured state is available in Tera templates as the `state` object. Use it to conditionally execute steps:

```yaml
tasks:
  renew-cert:
    flow:
      - name: check-expiry
        action: check-ssl-expiry
        input:
          domain: "{{ input.domain }}"

      - name: renew-cert
        action: renew-ssl
        depends_on: [check-expiry]
        # Only run if no previous state, or less than 30 days remain
        when: "not state or state.days_remaining < 30"
        input:
          domain: "{{ input.domain }}"

      - name: upload-cert
        action: upload-to-cdn
        depends_on: [renew-cert]
        input:
          domain: "{{ state.domain | default(value='example.com') }}"
```

If no previous state exists, the `state` object is not present in the template context — use `not state` to guard operations.

## Runner behavior

| Runner | `/state` (read) | `/state-out` (write) | `STATE:` protocol |
|--------|-----------------|---------------------|-------------------|
| Shell (local) | `$STATE_DIR` path | `$STATE_OUT_DIR` path | Yes |
| Docker | Bind mount `:ro` at `/state` | Bind mount `:rw` at `/state-out` | Yes |
| Kubernetes | emptyDir volume | emptyDir volume | Yes |

All runners set `STATE_DIR` and `STATE_OUT_DIR` environment variables automatically.

:::note
For Kubernetes runners, file-based state from previous runs is available via `/state`. However, uploading file state (`/state-out`) in the initial release works only for the `STATE:` protocol (structured data). File artifacts are supported on shell and Docker runners.
:::

## Multi-step tasks

When a task has multiple steps:

- **All steps** start with the same previous state snapshot
- **Sequential steps** see state written by earlier steps (state is resolved at claim time)
- **Parallel steps** all see the pre-parallel state independently; each can write new state
- Only steps that write to `/state-out` or emit `STATE:` lines create a new snapshot

**Example: Sequential state propagation**

```yaml
tasks:
  incremental-process:
    flow:
      - name: load-cursor
        action: fetch-cursor        # Reads $STATE_DIR/cursor.json, emits STATE:

      - name: process-batch
        action: process-data        # Reads $STATE_DIR/cursor.json (updated by step 1)
        depends_on: [load-cursor]   # Sees step 1's state

      - name: save-cursor
        action: save-cursor         # Writes new cursor to $STATE_OUT_DIR
        depends_on: [process-batch]
```

In this flow:
1. `load-cursor` runs first, reads previous cursor from `/state`, emits new cursor via `STATE:`
2. `process-batch` is claimed next and sees the updated state (step 1's `STATE:` JSON)
3. `save-cursor` also sees the updated state

## Configuration

State storage uses the same archive backend as [log storage](/guides/logs/) by default. Override with a dedicated section:

```yaml
# server-config.yaml
state_storage:
  prefix: "state/"          # archive key prefix (default: "state/")
  max_snapshots: 5          # snapshots retained per task (default: 5)
  # Optional: override the archive backend
  # archive:
  #   type: s3
  #   bucket: my-state-bucket
  #   region: eu-west-1
  #   endpoint: "https://s3.example.com"  # optional (for S3-compatible services)
```

If neither `state_storage` nor `log_storage.archive` is configured, state features are disabled — state directories remain empty and no snapshots are stored.

### Archive key format

State snapshots are stored with this key structure:

```
{prefix}{workspace}/{task_name}/{job_id}.tar.gz
```

Example: `state/production/renew-ssl/550e8400-e29b-41d4-a716-446655440000.tar.gz`

## Environment variables

| Variable | Value | Availability |
|----------|-------|--------------|
| `STATE_DIR` | Path to the extracted previous state (read-only) | All runners |
| `STATE_OUT_DIR` | Path for writing new state | All runners |

These are set automatically by the worker and runner. If no previous state exists, `STATE_DIR` points to an empty directory.

## Retention

Snapshots are pruned automatically per task to keep a rolling window of recent snapshots:

- Default: keep the last **5 snapshots** per task (configurable via `max_snapshots`)
- Older snapshots are deleted from both the database and archive
- Snapshots survive job retention cleanup — the snapshot's `job_id` reference is cleared when the job is deleted, but the snapshot remains in the archive

## Use cases

### SSL certificate renewal

```yaml
actions:
  check-ssl:
    type: script
    script: |
      #!/bin/bash
      if [ -f "$STATE_DIR/cert.pem" ]; then
        EXPIRY=$(openssl x509 -in "$STATE_DIR/cert.pem" -noout -enddate | cut -d= -f2)
        DAYS=$(( ($(date -d "$EXPIRY" +%s) - $(date +%s)) / 86400 ))
        echo "STATE: {\"days_remaining\": $DAYS, \"expiry\": \"$EXPIRY\"}"
      else
        echo "STATE: {\"days_remaining\": 0, \"expiry\": null}"
      fi

  renew-ssl:
    type: script
    script: |
      #!/bin/bash
      # Use certbot or acme.sh to renew
      certbot renew
      cp /etc/letsencrypt/live/example.com/fullchain.pem $STATE_OUT_DIR/cert.pem
      cp /etc/letsencrypt/live/example.com/privkey.pem $STATE_OUT_DIR/privkey.pem

tasks:
  maintain-cert:
    flow:
      - name: check
        action: check-ssl

      - name: renew
        action: renew-ssl
        depends_on: [check]
        when: "not state or state.days_remaining < 30"
```

### Incremental processing with cursors

```yaml
actions:
  process-logs:
    type: script
    script: |
      #!/bin/bash
      # Load cursor from previous state
      CURSOR=$(cat "$STATE_DIR/cursor.json" 2>/dev/null | jq -r .offset // "0")

      # Process logs starting from cursor
      tail -n +$CURSOR /var/log/app.log | while read line; do
        echo "$line" | process-line
      done

      # Save new cursor position
      NEW_CURSOR=$(wc -l < /var/log/app.log)
      echo "STATE: {\"offset\": $NEW_CURSOR, \"last_processed\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"}"

tasks:
  log-processor:
    flow:
      - name: ingest
        action: process-logs
```

### Build artifact caching

```yaml
actions:
  build:
    type: script
    runner: docker
    language: javascript
    dependencies: [next]
    script: |
      cd /workspace
      # Restore cache from previous build
      if [ -d "$STATE_DIR/.next/cache" ]; then
        cp -r "$STATE_DIR/.next/cache" .next/
      fi

      # Build and cache artifacts
      npm run build
      mkdir -p $STATE_OUT_DIR/.next
      cp -r .next/cache $STATE_OUT_DIR/.next/
      echo "STATE: {\"build_time\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\", \"size_mb\": $(du -sm .next | cut -f1)}"

tasks:
  web-build:
    flow:
      - name: build
        action: build
```

## Global workspace state

In addition to per-task state, Strøm supports **global workspace state** — shared across all tasks in a workspace. Any task can read and write it.

### Writing global state

Use the `GLOBAL_STATE:` protocol or write files to `/global-state-out` (`$GLOBAL_STATE_OUT_DIR`):

```bash
#!/bin/bash
# Share a token across all tasks in this workspace
echo "GLOBAL_STATE: {\"api_token\": \"$(vault read -field=token secret/api)\", \"refreshed_at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"}"
```

### Reading global state

Previous global state is available at `/global-state` (`$GLOBAL_STATE_DIR`) and in Tera templates as `{{ global_state.* }}`:

```yaml
tasks:
  deploy:
    flow:
      - name: deploy
        action: deploy-app
        input:
          token: "{{ global_state.api_token }}"
```

### How it differs from task state

| | Task state | Global state |
|---|---|---|
| Scope | Per task | Per workspace (all tasks) |
| Protocol | `STATE:` | `GLOBAL_STATE:` |
| Read dir | `/state` | `/global-state` |
| Write dir | `/state-out` | `/global-state-out` |
| Templates | `{{ state.* }}` | `{{ global_state.* }}` |
| Concurrent writes | Per-task (rare) | Cross-task (last writer wins) |

Both types of state are available simultaneously — a step can read and write both task state and global state in the same run.

## Limitations

- State is immutable once a snapshot is created. To update state, emit new `STATE:`/`GLOBAL_STATE:` lines or write new files from the step.
- The maximum size of a state snapshot is **50 MB**. Oversized uploads are rejected with a warning (non-fatal — the job continues).
- For Kubernetes runners, file-based state (`/state-out`) works via the `STATE:` protocol. Direct file uploads are supported on shell and Docker runners.
- Global state concurrent writes don't merge — last writer wins. Coordinate via task dependencies if ordering matters.
