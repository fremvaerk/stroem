-- Persist the parsed state.json sidecar so the server can render
-- {{ state.* }} / {{ global_state.* }} without fetching the archive.
-- `json`, not `jsonb`: jsonb rejects the NUL-byte escape that the
-- existing extractor accepts; the value is only ever read whole.
ALTER TABLE task_state      ADD COLUMN state_json JSON NULL;
ALTER TABLE workspace_state ADD COLUMN state_json JSON NULL;

COMMENT ON COLUMN task_state.state_json      IS 'Parsed state.json sidecar (copy of the tarball entry); NULL when absent, unparseable, or written before 047';
COMMENT ON COLUMN workspace_state.state_json IS 'Parsed state.json sidecar (copy of the tarball entry); NULL when absent, unparseable, or written before 047';
