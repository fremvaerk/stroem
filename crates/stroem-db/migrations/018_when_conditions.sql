-- Step-level conditional execution (Phase 5a)
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS when_condition TEXT;
