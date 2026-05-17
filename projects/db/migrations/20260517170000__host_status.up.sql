-- Per-peer system snapshot timeseries.
--
-- Each row is one collected snapshot. Rows where `peer_id = <this host's
-- own peer id>` are written by the local persistence task (`source='local'`).
-- Rows for any other peer_id are mirrored in by the sync puller from that
-- peer's own DB (`source='synced'`) — read-only as far as this host is
-- concerned. The (peer_id, snapshot_at_unix) PK makes a duplicate sync
-- import a no-op (INSERT OR IGNORE).
--
-- Row growth is bounded by a per-peer cap enforced inside the insert helper
-- (see `host_status::insert_status`), so we don't need a TTL trigger here.

CREATE TABLE IF NOT EXISTS host_status (
    peer_id          TEXT    NOT NULL,
    snapshot_at_unix INTEGER NOT NULL,
    payload_json     TEXT    NOT NULL,
    received_at_unix INTEGER NOT NULL,
    source           TEXT    NOT NULL CHECK (source IN ('local','synced')),
    PRIMARY KEY (peer_id, snapshot_at_unix)
);

CREATE INDEX IF NOT EXISTS idx_host_status_peer_time
    ON host_status (peer_id, snapshot_at_unix DESC);
