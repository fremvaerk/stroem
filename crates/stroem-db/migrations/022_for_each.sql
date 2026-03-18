ALTER TABLE job_step ADD COLUMN for_each_expr TEXT;
ALTER TABLE job_step ADD COLUMN loop_source TEXT;
ALTER TABLE job_step ADD COLUMN loop_index INTEGER;
ALTER TABLE job_step ADD COLUMN loop_total INTEGER;
ALTER TABLE job_step ADD COLUMN loop_item JSONB;

CREATE INDEX idx_job_step_loop_source ON job_step (job_id, loop_source) WHERE loop_source IS NOT NULL;
