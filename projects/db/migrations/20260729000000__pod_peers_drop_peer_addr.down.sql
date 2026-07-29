-- Restore the scalar `peer_addr` column and best-effort repopulate it from the
-- peer's primary route (prefer lan_v4, else any) in `pod_peer_addresses`. The
-- original exact value is not recoverable if routes have since changed; this
-- restores a dialable primary, which is all `peer_addr` ever held.
ALTER TABLE pod_peers ADD COLUMN peer_addr TEXT NOT NULL DEFAULT '';

UPDATE pod_peers
SET peer_addr = COALESCE(
    (SELECT value FROM pod_peer_addresses a
      WHERE a.peer_id = pod_peers.peer_id AND a.kind = 'lan_v4'
      ORDER BY a.value LIMIT 1),
    (SELECT value FROM pod_peer_addresses a
      WHERE a.peer_id = pod_peers.peer_id
      ORDER BY a.kind, a.value LIMIT 1),
    ''
);
