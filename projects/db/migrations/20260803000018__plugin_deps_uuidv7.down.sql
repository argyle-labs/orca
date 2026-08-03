-- Revert Phase A (EXPAND) of the v7-id program for `plugin_deps`.
DROP TRIGGER IF EXISTS plugin_deps_uuidv7_autofill;
DROP INDEX IF EXISTS idx_plugin_deps_uuidv7;
ALTER TABLE plugin_deps DROP COLUMN uuidv7;
