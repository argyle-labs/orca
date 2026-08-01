-- Restore the old per-peer host_status shape. This is a cache with no durable
-- value; a rollback leaves it empty and the local writer repopulates this
-- host's own rows (source='local') within minutes.
DROP INDEX IF EXISTS idx_host_status_time;
DROP TABLE IF EXISTS host_status;

CREATE TABLE host_status (
    peer_id          TEXT    NOT NULL,
    snapshot_at_unix INTEGER NOT NULL,
    payload_json     TEXT    NOT NULL,
    received_at_unix INTEGER NOT NULL,
    source           TEXT    NOT NULL CHECK (source IN ('local','synced')),
    PRIMARY KEY (peer_id, snapshot_at_unix)
);
CREATE INDEX idx_host_status_peer_time
    ON host_status (peer_id, snapshot_at_unix DESC);
