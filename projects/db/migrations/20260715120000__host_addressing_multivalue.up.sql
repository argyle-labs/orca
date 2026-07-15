-- host_addressing previously had PRIMARY KEY (key), so a host could store only
-- one value per channel kind. A dual-homed host (e.g. a box with both a wired
-- and a wireless LAN interface) then advertised only one of its LAN IPv4
-- addresses; the other was silently dropped at detection. Widen the PK to
-- (key, value) so every valid address of a kind is a first-class, equal row,
-- mirroring pod_peer_addresses on the peer side.
CREATE TABLE host_addressing_new (
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    source      TEXT NOT NULL,
    detected_at INTEGER NOT NULL,
    PRIMARY KEY (key, value)
);

INSERT OR IGNORE INTO host_addressing_new (key, value, source, detected_at)
    SELECT key, value, source, detected_at FROM host_addressing;

DROP TABLE host_addressing;
ALTER TABLE host_addressing_new RENAME TO host_addressing;
