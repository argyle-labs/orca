-- orca:tolerate-missing-table
--
-- The mesh stopped being called a "pod". These tables were the last place the
-- old name was load-bearing, and the baseline no longer creates them: since the
-- pod concept is gone, `apply_schema` creates `mesh_*` directly.
--
-- `apply_schema` runs BEFORE migrations, so by the time this runs:
--
--   * On a FRESH database the tables already exist under their current names
--     and there is nothing to rename. The GUARD statement below fails with
--     `no such table: pod_peers`, which aborts the batch before anything
--     destructive runs; the `orca:tolerate-missing-table` marker tells the
--     runner that means "already in the target state", and it records the
--     migration as applied.
--
--   * On an EXISTING database the real data is still in `pod_*`, and
--     `apply_schema` has just created EMPTY `mesh_*` shells alongside it. The
--     shells must be dropped before the populated tables can take their names
--     — otherwise the rename fails with "already exists" and the live roster
--     would be silently replaced by empty tables.
--
-- `ALTER TABLE … RENAME` is used deliberately instead of create-copy-drop: it
-- preserves whatever shape each table actually has on THIS database, and the
-- shapes differ by history — 20260729000000 dropped `pod_peers.peer_addr`, so
-- an explicit column list would be wrong on any database that has run it and
-- right only on one that has not. A rename cannot get that wrong.
--
-- SQLite rewrites REFERENCES clauses in other tables as part of a rename
-- (legacy_alter_table is off by default), so `pod_trust`'s foreign key follows
-- `pod_peers` through the temporary name and lands pointing at `mesh_peers`.

-- GUARD — MUST BE THE FIRST STATEMENT. On an already-current database this
-- fails and nothing below it executes.
ALTER TABLE pod_peers RENAME TO mesh_peers_migrating;

-- Empty shells just created by apply_schema. Child tables first: mesh_trust
-- carries a foreign key onto mesh_peers.
DROP TABLE IF EXISTS mesh_trust;
DROP TABLE IF EXISTS mesh_peer_addresses;
DROP TABLE IF EXISTS mesh_peers;
DROP TABLE IF EXISTS mesh_self;
DROP TABLE IF EXISTS mesh_discovery;
DROP TABLE IF EXISTS mesh_pending_offers;

-- The real data takes the current names.
ALTER TABLE mesh_peers_migrating RENAME TO mesh_peers;
ALTER TABLE pod_peer_addresses   RENAME TO mesh_peer_addresses;
ALTER TABLE pod_trust            RENAME TO mesh_trust;
ALTER TABLE pod_self             RENAME TO mesh_self;
ALTER TABLE pod_discovery        RENAME TO mesh_discovery;
ALTER TABLE pod_pending_offers   RENAME TO mesh_pending_offers;

-- The mesh's own identifier. Same value, current name.
ALTER TABLE mesh_self           RENAME COLUMN pod_id TO mesh_id;
ALTER TABLE mesh_pending_offers RENAME COLUMN pod_id TO mesh_id;

-- An index follows its table but keeps its own name.
DROP INDEX IF EXISTS idx_pod_pending_offers_fp;
CREATE INDEX IF NOT EXISTS idx_mesh_pending_offers_fp
    ON mesh_pending_offers (peer_pubkey_fp, direction);
DROP INDEX IF EXISTS idx_pod_peer_addresses_peer;
CREATE INDEX IF NOT EXISTS idx_mesh_peer_addresses_peer
    ON mesh_peer_addresses (peer_id);
