-- Revert Phase A (EXPAND) of the v7-id program for `plugin_types`.
DROP TRIGGER IF EXISTS plugin_types_uuidv7_autofill;
DROP INDEX IF EXISTS idx_plugin_types_uuidv7;
ALTER TABLE plugin_types DROP COLUMN uuidv7;
