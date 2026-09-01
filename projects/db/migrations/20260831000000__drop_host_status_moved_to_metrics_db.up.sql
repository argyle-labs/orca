-- host_status is a per-tick host-snapshot TIMESERIES. orca.db holds config only;
-- timeseries live in the encrypted metrics.db store (db::metrics). The table now
-- lives there (see metrics.rs init_schema + db::host_status), so drop the orca.db
-- copy. Existing rows here are local, disposable telemetry (age/size/row capped,
-- never mesh-mirrored) — they are not carried over; the writer repopulates
-- metrics.db on the next cadence tick.
DROP INDEX IF EXISTS idx_host_status_time;
DROP TABLE IF EXISTS host_status;
