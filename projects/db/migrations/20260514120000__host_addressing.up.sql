CREATE TABLE IF NOT EXISTS host_addressing (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    source TEXT NOT NULL,
    detected_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS pod_peer_addresses (
    peer_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    source TEXT NOT NULL,
    last_seen_at INTEGER NOT NULL,
    PRIMARY KEY (peer_id, kind, value),
    FOREIGN KEY (peer_id) REFERENCES pod_peers(peer_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_pod_peer_addresses_peer ON pod_peer_addresses(peer_id);
