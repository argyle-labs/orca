DROP TABLE IF EXISTS mesh_policy;
DROP TABLE IF EXISTS mesh_secure_registry;
DROP TABLE IF EXISTS mesh_nonce_counts;
DROP INDEX IF EXISTS idx_mesh_seen_nonces_expires_at;
DROP TABLE IF EXISTS mesh_seen_nonces;
CREATE TABLE IF NOT EXISTS mesh_promotion_nonces (
    nonce      TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
