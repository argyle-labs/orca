-- Recreate the orca.db host_status table (empty on restore). The live timeseries
-- lives in metrics.db; this reversal only restores the schema shape.
CREATE TABLE IF NOT EXISTS host_status (
    snapshot_at_unix INTEGER PRIMARY KEY,
    payload_json     TEXT    NOT NULL,
    received_at_unix INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_host_status_time
    ON host_status (snapshot_at_unix DESC);
