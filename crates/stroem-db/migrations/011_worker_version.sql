-- Add version column to worker table for tracking worker software versions
SET lock_timeout = '5s';
ALTER TABLE worker ADD COLUMN version TEXT;
RESET lock_timeout;
