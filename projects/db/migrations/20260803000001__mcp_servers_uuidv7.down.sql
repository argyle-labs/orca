-- Revert Phase A (EXPAND) of the v7-id program for `mcp_servers`.
DROP TRIGGER IF EXISTS mcp_servers_uuidv7_autofill;
DROP INDEX IF EXISTS idx_mcp_servers_uuidv7;
ALTER TABLE mcp_servers DROP COLUMN uuidv7;
