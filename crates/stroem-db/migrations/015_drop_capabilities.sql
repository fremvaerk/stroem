-- Backfill: copy capabilities into tags for any worker where tags is empty.
-- Both columns are NOT NULL (enforced since migrations 001 and 004), so NULL guards
-- are omitted. Guard against overwriting real tags with empty capabilities.
UPDATE worker SET tags = capabilities
WHERE tags = '[]'::jsonb
  AND capabilities != '[]'::jsonb
  AND capabilities != 'null'::jsonb;

-- Drop the now-redundant capabilities column (catalog-only, no table rewrite).
ALTER TABLE worker DROP COLUMN capabilities;
