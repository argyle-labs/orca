-- Revert Phase A (EXPAND) of the v7-id program for `plugin_tools`.
DROP TRIGGER IF EXISTS plugin_tools_uuidv7_autofill;
DROP INDEX IF EXISTS idx_plugin_tools_uuidv7;
ALTER TABLE plugin_tools DROP COLUMN uuidv7;
