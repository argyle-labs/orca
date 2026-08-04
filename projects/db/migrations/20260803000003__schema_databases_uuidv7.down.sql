-- Revert Phase A (EXPAND) of the v7-id program for `schema_databases`.
DROP TRIGGER IF EXISTS schema_databases_uuidv7_autofill;
DROP INDEX IF EXISTS idx_schema_databases_uuidv7;
ALTER TABLE schema_databases DROP COLUMN uuidv7;
