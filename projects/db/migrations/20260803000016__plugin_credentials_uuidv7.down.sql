-- Revert Phase A (EXPAND) of the v7-id program for `plugin_credentials`.
DROP TRIGGER IF EXISTS plugin_credentials_uuidv7_autofill;
DROP INDEX IF EXISTS idx_plugin_credentials_uuidv7;
ALTER TABLE plugin_credentials DROP COLUMN uuidv7;
