-- Revert Phase A (EXPAND) of the v7-id program for `plugin_installs`.
DROP TRIGGER IF EXISTS plugin_installs_uuidv7_autofill;
DROP INDEX IF EXISTS idx_plugin_installs_uuidv7;
ALTER TABLE plugin_installs DROP COLUMN uuidv7;
