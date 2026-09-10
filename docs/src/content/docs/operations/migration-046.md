---
title: Migration 046 — skip reasons and continue_when_skipped
description: What changes for continue_on_failure, and how to get the old behaviour back
---

Migration `046_job_step_skip_reason.sql` adds a nullable `job_step.skip_reason`
column. Alongside it, release 0.16.2 changes one behaviour of
`continue_on_failure` and adds the `continue_when_skipped` flow-step flag.

## What changes on upgrade

Before 0.16.2, `continue_on_failure: true` also let a step run when **all** of
its dependencies were skipped. That was undocumented. From 0.16.2,
`continue_on_failure` is failure-only: it lets a step run when a **direct**
dependency failed or was cancelled, and marks the step's own failure as
tolerable. A step whose dependencies were all skipped is now skipped even with
`continue_on_failure`.

## Who is affected

Only steps that have `continue_on_failure: true` **and** whose dependencies can
all be skipped in the same run (every dependency has a `when`, or sits behind
one). Steps with at least one dependency that always runs are not affected.

## The fix

Add `continue_when_skipped: true` to the DEPENDENCY that can be skipped (the
step with the `when`). To keep the exact old behaviour ("run no matter
what"), also keep `continue_on_failure: true` on the dependent.

0.16.3 moved the flag from the dependent to the skipped step; 0.16.2
placement on the dependent has no effect from 0.16.3.

## The column

`skip_reason` is `NULL` on rows written before the migration. The cascade treats
`NULL` as `unreachable`, so a step depending on a `continue_when_skipped`
dependency behind such rows stays skipped — the same outcome as before the
upgrade. The migration is
additive: the server applies it at startup, and an old replica in a mixed
fleet is unaffected because every `job_step` query names its columns
explicitly.
