-- Restart failed steps (spec docs/superpowers/specs/2026-11-02-restart-failed-steps-design.md):
-- the start steps of an all-failed restart. NULL for every other job,
-- including a single-step restart, which keeps using restart_from_step.
--
-- A nullable column with no default changes only the catalog: no rewrite, no
-- scan, so no manual pre-run.
--
-- IF NOT EXISTS: a rollback deletes this migration's history row and keeps
-- the column (spec § 6), so the next upgrade runs this file again.
ALTER TABLE job ADD COLUMN IF NOT EXISTS restart_from_steps TEXT[];
