-- Revert Phase A (EXPAND) of the v7-id program for `secrets`.
DROP TRIGGER IF EXISTS secrets_uuidv7_autofill;
DROP INDEX IF EXISTS idx_secrets_uuidv7;
ALTER TABLE secrets DROP COLUMN uuidv7;
