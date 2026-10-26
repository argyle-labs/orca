DROP TABLE IF EXISTS mesh_promotion_nonces;
CREATE TABLE IF NOT EXISTS mesh_seen_nonces (
    nonce      TEXT PRIMARY KEY,
    method     TEXT NOT NULL,
    sender     TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mesh_seen_nonces_expires_at
    ON mesh_seen_nonces (expires_at);
CREATE TABLE IF NOT EXISTS mesh_nonce_counts (
    scope TEXT PRIMARY KEY,
    n     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS mesh_secure_registry (
    id         TEXT PRIMARY KEY,
    member_id  TEXT NOT NULL,
    state      TEXT NOT NULL,
    ca_fp      TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    signature  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS mesh_policy (
    id         TEXT PRIMARY KEY,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    ca_fp      TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    signature  TEXT NOT NULL
);
ALTER TABLE mesh_peers ADD COLUMN bound_envelopes INTEGER NOT NULL DEFAULT 0;
