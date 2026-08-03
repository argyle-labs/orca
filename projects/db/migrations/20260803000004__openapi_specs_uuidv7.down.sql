-- Revert Phase A (EXPAND) of the v7-id program for `openapi_specs`.
DROP TRIGGER IF EXISTS openapi_specs_uuidv7_autofill;
DROP INDEX IF EXISTS idx_openapi_specs_uuidv7;
ALTER TABLE openapi_specs DROP COLUMN uuidv7;
