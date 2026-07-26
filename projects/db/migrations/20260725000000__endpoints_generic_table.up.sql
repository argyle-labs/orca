-- Generic endpoint registry data migration.
--
-- The `endpoints` table itself is created by apply_schema() (which runs before
-- migrations); this CREATE IF NOT EXISTS is a safety net for the rare path where
-- a migration runs against a connection that predates the apply_schema addition.
CREATE TABLE IF NOT EXISTS endpoints (
    id             TEXT PRIMARY KEY,
    provider       TEXT NOT NULL,
    name           TEXT NOT NULL,
    addresses      TEXT NOT NULL DEFAULT '[]',
    enabled        INTEGER NOT NULL DEFAULT 1,
    auth_principal TEXT,
    insecure       INTEGER,
    created_at     TEXT,
    updated_at     INTEGER,
    UNIQUE(provider, name)
);

-- Copy legacy per-plugin endpoint rows into the shared table, tagging `provider`
-- from the source table name. `ntfy_endpoints` is the only legacy *_endpoints
-- table that ever reached the daemon (its own CREATE TABLE IF NOT EXISTS
-- migration, 20260611, runs earlier in the same pending pass, so FROM is always
-- safe here). The other providers (proxmox/docker/dockge/homeassistant) only had
-- process-local SchemaFragments that never materialized a daemon table, so there
-- is nothing to copy — that is exactly the bug this table fixes.
--
-- NOTE: `token` on ntfy_endpoints is a SECRET and is deliberately NOT copied —
-- secrets never live in `endpoints`. The id is a one-time non-uuidv7 random value
-- for these legacy rows; all new rows are minted uuidv7 by the derive.
-- INSERT OR IGNORE + UNIQUE(provider,name) makes re-running the migration a
-- no-op.
INSERT OR IGNORE INTO endpoints
    (id, provider, name, addresses, enabled, auth_principal, insecure, created_at, updated_at)
SELECT lower(hex(randomblob(16))), 'ntfy', name, '[]', enabled, NULL, NULL, created_at, NULL
FROM ntfy_endpoints;
