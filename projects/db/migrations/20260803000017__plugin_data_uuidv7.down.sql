-- Revert Phase A (EXPAND) of the v7-id program for `plugin_data`.
DROP TRIGGER IF EXISTS plugin_data_uuidv7_autofill;
DROP INDEX IF EXISTS idx_plugin_data_uuidv7;
ALTER TABLE plugin_data DROP COLUMN uuidv7;
