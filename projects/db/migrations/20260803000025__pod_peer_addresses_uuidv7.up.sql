-- NOTE: this migration was written when the mesh tables were named `pod_*`.
-- It now spells them `mesh_*`, which is safe because it can only ever run on a
-- FRESH database: any database old enough to hold `pod_*` tables recorded this
-- migration as applied long before the rename, so it is never replayed there.
-- On a fresh database `apply_schema` creates `mesh_*` directly. The single
-- migration that must handle BOTH shapes is 20260927000000, which guards itself.
--
-- Phase A (EXPAND) of the v7-id program for `mesh_peer_addresses`.
-- Adds a v7 `uuidv7` as a PASSENGER column; the existing key stays PRIMARY KEY.
-- Non-disruptive: no FK/merge-key/cursor is repointed here (that is Phase C,
-- after the id has propagated + verified stable). Backfill + new-row minting
-- both use the registered `uuidv7()` scalar (single source of truth =
-- utils::id::new), so every row — existing and future — gets a distinct
-- time-ordered id with no per-table Rust insert wiring.
ALTER TABLE mesh_peer_addresses ADD COLUMN uuidv7 TEXT;
UPDATE mesh_peer_addresses SET uuidv7 = uuidv7() WHERE uuidv7 IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_mesh_peer_addresses_uuidv7 ON mesh_peer_addresses(uuidv7);
CREATE TRIGGER IF NOT EXISTS mesh_peer_addresses_uuidv7_autofill
AFTER INSERT ON mesh_peer_addresses
WHEN NEW.uuidv7 IS NULL
BEGIN
    UPDATE mesh_peer_addresses SET uuidv7 = uuidv7() WHERE rowid = NEW.rowid;
END;
