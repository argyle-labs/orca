-- Config store: typed, host-owned rows that drive scheduler, services,
-- backups, NFS watches, etc. Source of truth for runtime configuration.
-- See docs/planned/orca-v1-scope.md §3.1.

CREATE TABLE IF NOT EXISTS config_rows (
    id          TEXT PRIMARY KEY,
    host_owner  TEXT NOT NULL,
    noun        TEXT NOT NULL,
    name        TEXT NOT NULL,
    json        TEXT NOT NULL,
    is_replica  INTEGER NOT NULL DEFAULT 0,
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_by  TEXT NOT NULL DEFAULT 'local',
    UNIQUE (noun, name, host_owner)
);
CREATE INDEX IF NOT EXISTS idx_config_rows_noun  ON config_rows(noun);
CREATE INDEX IF NOT EXISTS idx_config_rows_owner ON config_rows(host_owner);

CREATE TABLE IF NOT EXISTS config_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    row_id      TEXT NOT NULL,
    prior_json  TEXT NOT NULL,
    changed_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    changed_by  TEXT NOT NULL DEFAULT 'local'
);
CREATE INDEX IF NOT EXISTS idx_config_history_row ON config_history(row_id);

CREATE TABLE IF NOT EXISTS config_schemas (
    noun             TEXT PRIMARY KEY,
    schema_json      TEXT NOT NULL,
    sensitive_fields TEXT NOT NULL DEFAULT '[]',
    registered_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
