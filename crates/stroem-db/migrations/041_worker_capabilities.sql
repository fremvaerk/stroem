-- Split worker capabilities from tags, giving tags "taint" semantics.
--
-- Before this migration:
--   worker.tags: JSONB   -- mixed "ability tokens" (script/docker/kubernetes/agent)
--                        -- and free-form labels (gpu, node-20, ...).
--   job_step.required_tags: JSONB  -- includes the ability token prefix.
--   claim SQL:  required_tags <@ worker.tags
--
--   Consequence: a worker with tags ["script","gpu"] would claim any script
--   step, defeating the point of the "gpu" label.
--
-- After this migration:
--   worker.capabilities: JSONB    -- what runners this worker supports.
--                                 -- Any of: "script","docker","kubernetes","agent".
--   worker.tags:         JSONB    -- taints: free-form labels the worker
--                                 -- reserves itself for. If non-empty, only
--                                 -- steps that explicitly requested ALL of
--                                 -- these can reach the worker.
--   job_step.required_ability: TEXT  -- the runner the step needs.
--   job_step.required_tags:    JSONB -- ONLY user-declared tags (no ability token).
--
--   claim SQL:
--     $1::jsonb @> to_jsonb(required_ability)   -- worker.capabilities contains it
--     AND $2::jsonb <@ required_tags::jsonb     -- step opted into worker's taints
--
-- Behaviour change (IMPORTANT — surface to operators via release notes):
--   1. A worker with tags=["gpu"] NO LONGER accepts steps that didn't
--      request "gpu". Operators who want the old permissive behaviour must
--      remove the tags from the worker config.
--   2. Conversely, an action with tags=["gpu"] no longer REQUIRES a
--      gpu-labelled worker — it just permits gpu-tainted workers to claim
--      it. Any worker whose capability set matches AND whose tags are a
--      subset of the step's tags can claim. Pre-041 semantics were the
--      opposite: worker.tags had to be a superset. This is a semantic
--      inversion and any workflows that relied on the old direction must
--      be re-checked.

ALTER TABLE worker
    ADD COLUMN capabilities JSONB NOT NULL DEFAULT '[]'::jsonb;

-- required_ability is intentionally NULL by default. The claim SQL never
-- reads a NULL value (task/agent/approval/event_source are excluded by
-- action_type before the ability check), so historical rows for those
-- server-dispatched action types get a value that matches what
-- `compute_required_ability` emits for them (empty string / no ability).
-- The backfill below writes '' for those action types so historical and
-- future rows agree, and writes the derived ability everywhere else.
ALTER TABLE job_step
    ADD COLUMN required_ability TEXT NOT NULL DEFAULT '';

-- Helper: safely convert a JSONB value to a text[] of its array elements.
-- Returns an empty array when the value is NULL, non-array, or empty.
-- Defensive against a legacy row that got a scalar written into a JSONB
-- column (e.g. by a hand-edit or an earlier release bug) — without this
-- guard, jsonb_array_elements_text() would raise 22023 and roll the
-- entire migration back with an opaque error.
CREATE OR REPLACE FUNCTION _041_jsonb_str_array(v jsonb) RETURNS text[]
LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE
        WHEN v IS NULL OR jsonb_typeof(v) <> 'array' THEN ARRAY[]::text[]
        ELSE (SELECT COALESCE(array_agg(elem), ARRAY[]::text[])
              FROM jsonb_array_elements_text(v) AS elem)
    END
$$;

-- Backfill worker.capabilities:
--   pull the four known ability tokens out of tags (case-insensitive so
--   `Script` / `DOCKER` don't stay orphaned as taints); leave everything
--   else in tags (they become taints).
UPDATE worker
SET
    capabilities = (
        SELECT COALESCE(jsonb_agg(DISTINCT lower(t)), '[]'::jsonb)
        FROM unnest(_041_jsonb_str_array(tags)) AS t
        WHERE lower(t) IN ('script', 'docker', 'kubernetes', 'agent')
    ),
    tags = (
        SELECT COALESCE(jsonb_agg(t), '[]'::jsonb)
        FROM unnest(_041_jsonb_str_array(tags)) AS t
        WHERE lower(t) NOT IN ('script', 'docker', 'kubernetes', 'agent')
    );

-- Do NOT auto-assign `script` to workers whose pre-041 tags had no ability
-- token. Doing so would silently promote e.g. a GPU-only worker that
-- routed via required_tags into a script-capable worker, then it would
-- start claiming plain script steps unexpectedly. Instead, leave
-- capabilities=[] for those rows — the worker simply won't claim
-- anything, which is a visible failure the operator can fix by editing
-- worker-config.yaml. The server-side register_worker validation also
-- rejects empty capabilities on the next reconnect, giving a clear error.

-- Backfill job_step.required_ability:
--   the ability token is derived from action_type + runner (matching what
--   `compute_required_ability` produces on new rows post-041). We use
--   action_type as the primary key rather than reading required_tags
--   because required_tags could have missed the ability token via a bug
--   or a manual edit — action_type is the ground truth.
--
--   For server-dispatched action types (task/agent/approval/event_source
--   — the runner_type field wasn't present pre-041 so we approximate via
--   the row's `runner` column) we write '' to match the new-row shape.
UPDATE job_step
SET required_ability =
    CASE
        WHEN action_type IN ('task', 'approval') THEN ''
        WHEN action_type = 'agent'   THEN 'agent'
        WHEN action_type = 'docker'  THEN 'docker'
        WHEN action_type = 'pod'     THEN 'kubernetes'
        WHEN action_type = 'script' AND runner = 'docker' THEN 'docker'
        WHEN action_type = 'script' AND runner = 'pod'    THEN 'kubernetes'
        WHEN action_type = 'script' THEN 'script'
        -- event_source consumers are server-dispatched but their steps
        -- may run on a worker (they're just filtered from claim by
        -- action_type NOT IN (...)) — treat as script for safety, since
        -- an event_source consumer's inner script is what a worker would
        -- claim if it ever appeared as `ready`.
        ELSE 'script'
    END,
    required_tags = (
        SELECT COALESCE(jsonb_agg(t), '[]'::jsonb)
        FROM unnest(_041_jsonb_str_array(required_tags)) AS t
        WHERE lower(t) NOT IN ('script', 'docker', 'kubernetes', 'agent')
    );

-- Index the ability so the new claim SQL doesn't do a table scan.
CREATE INDEX idx_job_step_ready_ability
    ON job_step (required_ability)
    WHERE status = 'ready';

-- Helper only needed during this migration.
DROP FUNCTION _041_jsonb_str_array(jsonb);
