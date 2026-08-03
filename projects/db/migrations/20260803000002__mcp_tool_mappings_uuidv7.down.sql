-- Revert Phase A (EXPAND) of the v7-id program for `mcp_tool_mappings`.
DROP TRIGGER IF EXISTS mcp_tool_mappings_uuidv7_autofill;
DROP INDEX IF EXISTS idx_mcp_tool_mappings_uuidv7;
ALTER TABLE mcp_tool_mappings DROP COLUMN uuidv7;
