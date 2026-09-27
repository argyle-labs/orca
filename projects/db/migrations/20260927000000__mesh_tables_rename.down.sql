DROP INDEX IF EXISTS idx_mesh_pending_offers_fp;
DROP INDEX IF EXISTS idx_mesh_peer_addresses_peer;
ALTER TABLE mesh_pending_offers RENAME COLUMN mesh_id TO pod_id;
ALTER TABLE mesh_self           RENAME COLUMN mesh_id TO pod_id;
ALTER TABLE mesh_pending_offers RENAME TO pod_pending_offers;
ALTER TABLE mesh_discovery      RENAME TO pod_discovery;
ALTER TABLE mesh_self           RENAME TO pod_self;
ALTER TABLE mesh_trust          RENAME TO pod_trust;
ALTER TABLE mesh_peer_addresses RENAME TO pod_peer_addresses;
ALTER TABLE mesh_peers          RENAME TO pod_peers;
CREATE INDEX IF NOT EXISTS idx_pod_pending_offers_fp
    ON pod_pending_offers (peer_pubkey_fp, direction);
CREATE INDEX IF NOT EXISTS idx_pod_peer_addresses_peer
    ON pod_peer_addresses (peer_id);
