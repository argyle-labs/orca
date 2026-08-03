-- Revert Phase A (EXPAND) of the v7-id program for `user_active_profile`.
DROP TRIGGER IF EXISTS user_active_profile_uuidv7_autofill;
DROP INDEX IF EXISTS idx_user_active_profile_uuidv7;
ALTER TABLE user_active_profile DROP COLUMN uuidv7;
