-- Add admin flag to users
ALTER TABLE "user" ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT FALSE;

-- Groups table: named sets of users, managed by admins
CREATE TABLE user_group (
    group_name TEXT NOT NULL,
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (group_name, user_id)
);
CREATE INDEX idx_user_group_user ON user_group(user_id);
