-- Revert Phase A (EXPAND) of the v7-id program for `config_history`.
DROP TRIGGER IF EXISTS config_history_uuidv7_autofill;
DROP INDEX IF EXISTS idx_config_history_uuidv7;
ALTER TABLE config_history DROP COLUMN uuidv7;
