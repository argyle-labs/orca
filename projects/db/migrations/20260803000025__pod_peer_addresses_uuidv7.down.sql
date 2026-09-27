-- Revert Phase A (EXPAND) of the v7-id program for `mesh_peer_addresses`.
DROP TRIGGER IF EXISTS mesh_peer_addresses_uuidv7_autofill;
DROP INDEX IF EXISTS idx_mesh_peer_addresses_uuidv7;
ALTER TABLE mesh_peer_addresses DROP COLUMN uuidv7;
