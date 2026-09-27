-- NOTE: this migration predates the mesh table rename (20260927000000).
-- It now spells them `mesh_*`, which is safe because it can only ever run on a
-- FRESH database: any database old enough to predate the rename recorded this
-- migration as applied long before the rename, so it is never replayed there.
-- On a fresh database `apply_schema` creates `mesh_*` directly. The single
-- migration that must handle BOTH shapes is 20260927000000, which guards itself.
--
-- Drop the scalar `mesh_peers.peer_addr` in favor of `mesh_peer_addresses` (the
-- multi-route source of truth). Finishes the addressing cleanup: a peer is
-- multi-homed, so a single primary-address column is the scalar-URL smell; its
-- reachability belongs in the routes table. `peer_port` stays — a peer listens
-- on ONE mesh port across all its addresses, which is a peer property, not an
-- address.
--
-- BACKFILL FIRST so no existing peer loses reachability: seed each peer's
-- current `peer_addr` as a `mesh_peer_addresses` route (source='bootstrap') if it
-- is not already present. Kind is classified v4/v6 by the presence of a colon
-- (an FQDN falls into lan_v4, which is still dialable). INSERT OR IGNORE +
-- the (peer_id, kind, value) PK makes this a no-op when the route already
-- exists (e.g. learned via ping) or when re-run.
INSERT OR IGNORE INTO mesh_peer_addresses (peer_id, kind, value, source, last_seen_at)
SELECT peer_id,
       CASE WHEN instr(peer_addr, ':') > 0 THEN 'lan_v6' ELSE 'lan_v4' END,
       peer_addr,
       'bootstrap',
       last_seen_at
FROM mesh_peers
WHERE peer_addr IS NOT NULL AND peer_addr <> '';

-- Now the scalar is redundant; drop it. (SQLite >= 3.35 DROP COLUMN; there is no
-- index or PK on peer_addr so this is a plain metadata + row rewrite.)
ALTER TABLE mesh_peers DROP COLUMN peer_addr;
