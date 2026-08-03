-- Revert Phase A (EXPAND) of the v7-id program for `settings`.
DROP TRIGGER IF EXISTS settings_uuidv7_autofill;
DROP INDEX IF EXISTS idx_settings_uuidv7;
ALTER TABLE settings DROP COLUMN uuidv7;
