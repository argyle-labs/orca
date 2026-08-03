-- Revert Phase A (EXPAND) of the v7-id program for `plugins`.
DROP TRIGGER IF EXISTS plugins_uuidv7_autofill;
DROP INDEX IF EXISTS idx_plugins_uuidv7;
ALTER TABLE plugins DROP COLUMN uuidv7;
