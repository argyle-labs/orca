-- REST/MCP API bearer tokens. Each row stores sha256(plaintext) only;
-- the raw token is returned exactly once from `auth.token_create` and
-- can never be recovered from the DB. See project_rest_auth_design.md.

CREATE TABLE IF NOT EXISTS api_tokens (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    token_hash   TEXT NOT NULL UNIQUE,
    role         TEXT NOT NULL CHECK (role IN ('admin','read')),
    created_at   TEXT NOT NULL,
    last_used_at TEXT,
    expires_at   TEXT
);
CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens(token_hash);
