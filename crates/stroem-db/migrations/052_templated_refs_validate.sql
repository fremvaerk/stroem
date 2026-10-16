-- Validate the check that 051 widened NOT VALID (spec 2026-10-15 § 8). A
-- migration of its own: sqlx runs each migration in one transaction, and a
-- VALIDATE inside 051 would scan job_step while holding 051's ACCESS
-- EXCLUSIVE lock. Here the scan takes SHARE UPDATE EXCLUSIVE; writes continue.
ALTER TABLE job_step VALIDATE CONSTRAINT job_step_action_type_check;
