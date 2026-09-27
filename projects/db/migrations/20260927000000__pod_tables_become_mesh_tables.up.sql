-- The mesh stopped being called a "pod". These tables are the last place the
-- old name was load-bearing, so they are renamed rather than left as a second
-- vocabulary for the same thing.
--
-- `ALTER TABLE … RENAME` is used deliberately instead of create-copy-drop: it
-- preserves whatever shape each table actually has on THIS database, and the
-- shapes differ by history — 20260729000000 dropped `pod_peers.peer_addr`, so
-- an explicit column list would be wrong on any database that has run it and
-- right only on one that has not. A rename cannot get that wrong.
--
-- SQLite rewrites REFERENCES clauses in other tables as part of the rename
-- (legacy_alter_table is off by default), so `mesh_trust`'s foreign key follows
-- `mesh_peers` without a second statement.
ALTER TABLE pod_peers           RENAME TO mesh_peers;
ALTER TABLE pod_peer_addresses  RENAME TO mesh_peer_addresses;
ALTER TABLE pod_trust           RENAME TO mesh_trust;
ALTER TABLE pod_self            RENAME TO mesh_self;
ALTER TABLE pod_discovery       RENAME TO mesh_discovery;
ALTER TABLE pod_pending_offers  RENAME TO mesh_pending_offers;

-- The mesh's own identifier. Same value, current name.
ALTER TABLE mesh_self           RENAME COLUMN pod_id TO mesh_id;
ALTER TABLE mesh_pending_offers RENAME COLUMN pod_id TO mesh_id;

-- An index follows its table but keeps its own name; rename it so the schema
-- does not read half-migrated.
DROP INDEX IF EXISTS idx_pod_pending_offers_fp;
CREATE INDEX IF NOT EXISTS idx_mesh_pending_offers_fp
    ON mesh_pending_offers (peer_pubkey_fp, direction);
DROP INDEX IF EXISTS idx_pod_peer_addresses_peer;
CREATE INDEX IF NOT EXISTS idx_mesh_peer_addresses_peer
    ON mesh_peer_addresses (peer_id);
