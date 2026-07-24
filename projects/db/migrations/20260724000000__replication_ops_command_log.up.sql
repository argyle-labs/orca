-- Command-log for delete replication. Before this, a hard DELETE of a
-- replicated row (users, config_rows, endpoint_resource entities) left no trace
-- that replicated, so a peer still holding the row re-gossiped it and it
-- resurrected. Deletes now also append a durable op here; the op replicates
-- (LWW on stamp_ms) and peers replay it to physically remove their own copy.
-- Compacted by replication_ops::reap once ops age past the anti-entropy
-- horizon. See projects/db/src/replication_ops.rs.
CREATE TABLE IF NOT EXISTS replication_ops (
    op_id     TEXT PRIMARY KEY,
    entity    TEXT NOT NULL,
    key_col   TEXT NOT NULL,
    key_val   TEXT NOT NULL,
    op        TEXT NOT NULL,
    origin    TEXT NOT NULL,
    stamp_ms  INTEGER NOT NULL,
    UNIQUE (entity, key_val)
);
CREATE INDEX IF NOT EXISTS idx_replication_ops_pending
    ON replication_ops(op) WHERE op = 'delete';
