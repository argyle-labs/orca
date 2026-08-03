-- Revert Phase A (EXPAND) of the v7-id program for `pod_peer_addresses`.
DROP TRIGGER IF EXISTS pod_peer_addresses_uuidv7_autofill;
DROP INDEX IF EXISTS idx_pod_peer_addresses_uuidv7;
ALTER TABLE pod_peer_addresses DROP COLUMN uuidv7;
