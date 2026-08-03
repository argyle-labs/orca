-- Revert Phase A (EXPAND) of the v7-id program for `profile_shares`.
DROP TRIGGER IF EXISTS profile_shares_uuidv7_autofill;
DROP INDEX IF EXISTS idx_profile_shares_uuidv7;
ALTER TABLE profile_shares DROP COLUMN uuidv7;
