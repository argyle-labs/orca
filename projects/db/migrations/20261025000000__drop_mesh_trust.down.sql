CREATE TABLE IF NOT EXISTS mesh_trust (
    peer_id      TEXT PRIMARY KEY REFERENCES mesh_peers(peer_id) ON DELETE CASCADE,
    local_secure INTEGER NOT NULL DEFAULT 0,
    peer_secure  INTEGER NOT NULL DEFAULT 0,
    set_at       INTEGER NOT NULL
);
