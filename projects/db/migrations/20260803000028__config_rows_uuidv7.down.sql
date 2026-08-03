-- Revert Phase A (EXPAND) of the v7-id program for `config_rows`.
DROP TRIGGER IF EXISTS config_rows_uuidv7_autofill;
DROP INDEX IF EXISTS idx_config_rows_uuidv7;
ALTER TABLE config_rows DROP COLUMN uuidv7;
