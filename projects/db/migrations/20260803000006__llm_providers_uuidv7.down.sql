-- Revert Phase A (EXPAND) of the v7-id program for `llm_providers`.
DROP TRIGGER IF EXISTS llm_providers_uuidv7_autofill;
DROP INDEX IF EXISTS idx_llm_providers_uuidv7;
ALTER TABLE llm_providers DROP COLUMN uuidv7;
