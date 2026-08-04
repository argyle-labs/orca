-- Phase A (EXPAND) of the v7-id program for `mcp_servers`.
-- Adds a v7 `uuidv7` as a PASSENGER column; the existing key stays PRIMARY KEY.
-- Non-disruptive: no FK/merge-key/cursor is repointed here (that is Phase C,
-- after the id has propagated + verified stable). Backfill + new-row minting
-- both use the registered `uuidv7()` scalar (single source of truth =
-- utils::id::new), so every row — existing and future — gets a distinct
-- time-ordered id with no per-table Rust insert wiring.
ALTER TABLE mcp_servers ADD COLUMN uuidv7 TEXT;
UPDATE mcp_servers SET uuidv7 = uuidv7() WHERE uuidv7 IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_mcp_servers_uuidv7 ON mcp_servers(uuidv7);
CREATE TRIGGER IF NOT EXISTS mcp_servers_uuidv7_autofill
AFTER INSERT ON mcp_servers
WHEN NEW.uuidv7 IS NULL
BEGIN
    UPDATE mcp_servers SET uuidv7 = uuidv7() WHERE rowid = NEW.rowid;
END;
