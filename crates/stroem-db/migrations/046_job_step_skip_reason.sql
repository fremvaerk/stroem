-- continue_when_skipped (spec 2026-09-09 §5): why a step is `skipped`.
-- One of 'condition' | 'empty' | 'cascade' | 'unreachable'. NULL on rows written
-- before this migration or by an older replica; the cascade treats NULL as
-- 'unreachable' (the pre-change behaviour). No CHECK: the Rust enum is the
-- authority so a future reason needs no migration.
ALTER TABLE job_step ADD COLUMN skip_reason TEXT;
