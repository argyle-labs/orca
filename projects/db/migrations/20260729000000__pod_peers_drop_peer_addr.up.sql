-- Drop the scalar `pod_peers.peer_addr` in favor of `pod_peer_addresses` (the
-- multi-route source of truth). Finishes the addressing cleanup: a peer is
-- multi-homed, so a single primary-address column is the scalar-URL smell; its
-- reachability belongs in the routes table. `peer_port` stays — a peer listens
-- on ONE mesh port across all its addresses, which is a peer property, not an
-- address.
--
-- BACKFILL FIRST so no existing peer loses reachability: seed each peer's
-- current `peer_addr` as a `pod_peer_addresses` route (source='bootstrap') if it
-- is not already present. Kind is classified v4/v6 by the presence of a colon
-- (an FQDN falls into lan_v4, which is still dialable). INSERT OR IGNORE +
-- the (peer_id, kind, value) PK makes this a no-op when the route already
-- exists (e.g. learned via ping) or when re-run.
INSERT OR IGNORE INTO pod_peer_addresses (peer_id, kind, value, source, last_seen_at)
SELECT peer_id,
       CASE WHEN instr(peer_addr, ':') > 0 THEN 'lan_v6' ELSE 'lan_v4' END,
       peer_addr,
       'bootstrap',
       last_seen_at
FROM pod_peers
WHERE peer_addr IS NOT NULL AND peer_addr <> '';

-- Now the scalar is redundant; drop it. (SQLite >= 3.35 DROP COLUMN; there is no
-- index or PK on peer_addr so this is a plain metadata + row rewrite.)
ALTER TABLE pod_peers DROP COLUMN peer_addr;
