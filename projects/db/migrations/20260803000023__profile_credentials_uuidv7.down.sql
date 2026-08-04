-- Revert Phase A (EXPAND) of the v7-id program for `profile_credentials`.
DROP TRIGGER IF EXISTS profile_credentials_uuidv7_autofill;
DROP INDEX IF EXISTS idx_profile_credentials_uuidv7;
ALTER TABLE profile_credentials DROP COLUMN uuidv7;
