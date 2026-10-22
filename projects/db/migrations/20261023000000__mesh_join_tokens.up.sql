CREATE TABLE IF NOT EXISTS mesh_join_tokens (
    token_hash TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
