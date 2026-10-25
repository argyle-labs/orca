CREATE TABLE IF NOT EXISTS mesh_seen_nonces (
    nonce      TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mesh_seen_nonces_expires_at
    ON mesh_seen_nonces (expires_at);
