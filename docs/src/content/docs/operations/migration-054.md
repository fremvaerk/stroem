---
title: Migration 054 — input files
description: Three new tables for file inputs, and the rollout order
---

Migration `054_input_files.sql` ships with [file inputs](/guides/file-inputs/) (`type: file`). It adds tables only. It does not change an existing table and adds no index to a large table.

## The tables

- `input_file` holds one row per uploaded file: the ID, the workspace and task it was uploaded for, the uploader, the origin (`api`, `mcp_inline` or `mcp_url`), the original and the safe name, the content type, the size, the SHA-256 and the storage key.
- `job_input_file` links a job and a field to a file (primary key `(job_id, field)`). Many jobs can use one file.
- `input_file_upload_token` holds the single-use MCP upload URLs. Only the SHA-256 of a token is stored.

The migration also adds two indexes, both on the new tables (`input_file.created_at` for the orphan sweep, `job_input_file.file_id`). No manual pre-run is needed. The server applies the migration at startup.

`job.job_id` and `"user".user_id` gain foreign keys from the new tables. Job retention now deletes a job's `job_input_file` rows in the same transaction as the job. Deleting a user keeps that user's uploads (`uploaded_by_user_id` becomes `NULL`) and deletes the user's unused upload URLs.

## Behaviour change: `file` is a reserved type name

`file` is now a primitive input type. A workspace that defines a **connection type named `file`** behaves differently after the upgrade:

- A task or action input written as `type: file` is now a file field, not a reference to that connection type. A caller that sends a connection name for it gets `Input field '…': the file cannot be used`.
- `stroem validate` rejects the workspace with `Connection type name 'file' is reserved (collides with a primitive input-field type or alias)`.

Rename such a connection type (and every `type: file` input that refers to it) **before** you upgrade the server.

## Rollout order

An older server replica treats `type: file` as a connection type and answers `400` for a task that uses it. Do the steps in this order:

1. Upgrade **every server replica**.
2. Upgrade **every worker**. A step that has files and runs on an older worker fails with `this step has file inputs, which the worker does not support; upgrade the worker`.
3. Add workflow YAML that uses `type: file`.

Check the blob storage for uploads before step 3: see [Storage](/operations/storage/#input-files).
