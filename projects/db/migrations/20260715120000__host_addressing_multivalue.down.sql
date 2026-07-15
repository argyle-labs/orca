-- Revert host_addressing to PRIMARY KEY (key), collapsing any multi-valued
-- channel back to its most-recently-detected value.
CREATE TABLE host_addressing_old (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    source      TEXT NOT NULL,
    detected_at INTEGER NOT NULL
);

INSERT OR REPLACE INTO host_addressing_old (key, value, source, detected_at)
    SELECT key, value, source, detected_at
    FROM host_addressing
    ORDER BY detected_at ASC;

DROP TABLE host_addressing;
ALTER TABLE host_addressing_old RENAME TO host_addressing;
