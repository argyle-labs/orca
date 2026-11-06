-- Local-only: approvals bind to a decision on this node, so the tables are
-- never registered for replication.
CREATE TABLE IF NOT EXISTS approval_requests (
    id                TEXT PRIMARY KEY,
    correlation_id    TEXT NOT NULL,
    tool              TEXT NOT NULL,
    action            TEXT,
    args_hash         TEXT NOT NULL,
    risk              TEXT NOT NULL CHECK (risk IN ('low', 'medium', 'high')),
    requester_user_id TEXT NOT NULL,
    target_summary    TEXT NOT NULL,
    plan_summary      TEXT NOT NULL,
    state             TEXT NOT NULL
        CHECK (state IN ('pending', 'approved', 'denied', 'expired', 'executed')),
    created_at        INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL,
    decided_by        TEXT,
    decided_at        INTEGER,
    decided_factor    TEXT
);
CREATE INDEX IF NOT EXISTS idx_approval_requests_state ON approval_requests(state, created_at);
CREATE INDEX IF NOT EXISTS idx_approval_requests_correlation ON approval_requests(correlation_id);

CREATE TABLE IF NOT EXISTS user_two_factor (
    user_id            TEXT PRIMARY KEY,
    enrolled           INTEGER NOT NULL DEFAULT 0,
    step_up_from_risk  TEXT CHECK (step_up_from_risk IN ('low', 'medium', 'high')),
    step_up_tools      TEXT NOT NULL DEFAULT '[]'
);
