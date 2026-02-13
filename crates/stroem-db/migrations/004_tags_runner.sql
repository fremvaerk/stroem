-- Worker tags (flexible replacement for capabilities)
ALTER TABLE worker ADD COLUMN tags JSONB NOT NULL DEFAULT '["shell"]';
UPDATE worker SET tags = capabilities;

-- Step required tags and runner mode
ALTER TABLE job_step ADD COLUMN required_tags JSONB NOT NULL DEFAULT '[]';
ALTER TABLE job_step ADD COLUMN runner TEXT NOT NULL DEFAULT 'local';

-- Backfill required_tags from action_type
UPDATE job_step SET required_tags = '["shell"]'
  WHERE action_type = 'shell' AND action_image IS NULL;
UPDATE job_step SET required_tags = '["docker"]'
  WHERE action_type = 'shell' AND action_image IS NOT NULL;
UPDATE job_step SET required_tags = '["docker"]' WHERE action_type = 'docker';
UPDATE job_step SET required_tags = '["kubernetes"]' WHERE action_type = 'pod';

-- Backfill runner
UPDATE job_step SET runner = 'docker'
  WHERE action_type = 'shell' AND action_image IS NOT NULL;
UPDATE job_step SET runner = 'none' WHERE action_type IN ('docker', 'pod');

ALTER TABLE job_step ADD CONSTRAINT job_step_runner_check
  CHECK (runner IN ('local', 'docker', 'pod', 'none'));

CREATE INDEX idx_job_step_required_tags ON job_step USING gin(required_tags);
