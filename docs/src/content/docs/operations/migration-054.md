---
title: Migration 054 — input files
description: Three new tables for file inputs, and the rollout order
---

Migration `054_input_files.sql` ships with [file inputs](/guides/file-inputs/) (`type: file`). It adds tables only. It does not change an existing table and adds no index to a large table.

## The tables

- `input_file` holds one row per uploaded file: the ID, the workspace and task it was uploaded for, the uploader, the origin (`api`, `mcp_inline` or `mcp_url`), the original and the safe name, the content type, the size, the SHA-256 and the storage key.
- `job_input_file` links a job and a field to a file (primary key `(job_id, field)`). Many jobs can use one file.
- `input_file_upload_token` holds the single-use MCP upload URLs. Only the SHA-256 of a token is stored.

No manual pre-run is needed. The server applies the migration at startup.

## Rollout order

An older server replica treats `type: file` as a connection type and answers `400` for a task that uses it. Do the steps in this order:

1. Upgrade **every server replica**.
2. Upgrade **every worker**. A step that has files and runs on an older worker fails with `this step has file inputs, which the worker does not support; upgrade the worker`.
3. Add workflow YAML that uses `type: file`.

Check the blob storage for uploads before step 3: see [Storage](/operations/storage/#input-files).
