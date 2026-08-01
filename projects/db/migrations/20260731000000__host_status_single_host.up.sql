-- Collapse host_status from the old per-peer mirror shape
-- (peer_id, snapshot_at_unix, payload_json, received_at_unix, source) to a
-- clean single-host timeseries. Cross-mesh telemetry sync was removed
-- (#199/#179): under the data-classification law telemetry stays local to its
-- origin and is fetched on demand, so this table only ever holds THIS host's
-- own rows. The peer_id/source columns are now vestigial.
--
-- Existing rows are dropped, not migrated: they are oversized pre-fix bloat
-- (each row embedded the full history ring — observed 197 MB of orphaned
-- pre-fix `synced` rows) plus peer mirrors we want gone. host_status is
-- ephemeral local telemetry that the live writer repopulates within minutes,
-- so an empty reset is correct and reclaims the space.
--
-- NOTE: SQLite reclaims file size only on VACUUM. We intentionally do NOT
-- VACUUM here (it rewrites the whole DB and can't run inside a transaction);
-- operators / `db.compact` reclaim the freed pages, matching repo convention.
DROP INDEX IF EXISTS idx_host_status_peer_time;
DROP TABLE IF EXISTS host_status;

CREATE TABLE host_status (
    snapshot_at_unix INTEGER PRIMARY KEY,
    payload_json     TEXT    NOT NULL,
    received_at_unix INTEGER NOT NULL
);
CREATE INDEX idx_host_status_time
    ON host_status (snapshot_at_unix DESC);
