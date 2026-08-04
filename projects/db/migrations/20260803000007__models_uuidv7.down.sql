-- Revert Phase A (EXPAND) of the v7-id program for `models`.
DROP TRIGGER IF EXISTS models_uuidv7_autofill;
DROP INDEX IF EXISTS idx_models_uuidv7;
ALTER TABLE models DROP COLUMN uuidv7;
