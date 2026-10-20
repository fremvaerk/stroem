---
title: Migration 053 — trigger run indexes
description: Two partial indexes on job for the trigger last-run lookup, and the manual pre-run for large databases
---

Migration `053_trigger_run_indexes.sql` ships with the
[last run and missed runs](/guides/triggers/#last-run-and-missed-runs) status
of triggers. It adds two partial indexes on `job`, under new names, and
changes no data.

## The indexes

- `idx_job_trigger_runs` holds the runs of scheduler and webhook triggers
  that are not `skipped`, and the consumer jobs of event source triggers
  (jobs with a `_fingerprint` key in `input`). It finds a trigger's newest
  run.
- `idx_job_trigger_skipped` holds the `skipped` rows that `concurrency: skip`
  writes. With it, the server finds a trigger's newest fire.

Each lookup reads one index entry per trigger. The cost does not depend on
how many runs, `skipped` rows or emitted event source jobs the trigger has.
Both indexes hold trigger jobs only, so they stay small.

## Pre-run on a large database

The server applies the migration at startup, inside one transaction. Each
index build scans all of `job` under a `SHARE` lock, and writes to `job` wait
for the length of the scan. On a large database, build the indexes first with
`CONCURRENTLY`. Run the two statements one at a time, outside a transaction
(in autocommit):

```sql
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_trigger_runs
  ON job (source_type, source_id, created_at DESC)
  WHERE (source_type IN ('trigger', 'webhook') AND status <> 'skipped')
     OR (source_type = 'event_source' AND input ? '_fingerprint');
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_trigger_skipped
  ON job (source_id, created_at DESC)
  WHERE source_type = 'trigger' AND status = 'skipped';
```

The migration uses `CREATE INDEX IF NOT EXISTS`. After the pre-run it is a
no-op. If a `CONCURRENTLY` build fails, Postgres leaves an invalid index with
the same name. Drop it and run the statement again.
