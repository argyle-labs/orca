-- Revert Phase A (EXPAND) of the v7-id program for `feature_flags`.
DROP TRIGGER IF EXISTS feature_flags_uuidv7_autofill;
DROP INDEX IF EXISTS idx_feature_flags_uuidv7;
ALTER TABLE feature_flags DROP COLUMN uuidv7;
