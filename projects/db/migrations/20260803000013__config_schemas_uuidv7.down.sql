-- Revert Phase A (EXPAND) of the v7-id program for `config_schemas`.
DROP TRIGGER IF EXISTS config_schemas_uuidv7_autofill;
DROP INDEX IF EXISTS idx_config_schemas_uuidv7;
ALTER TABLE config_schemas DROP COLUMN uuidv7;
