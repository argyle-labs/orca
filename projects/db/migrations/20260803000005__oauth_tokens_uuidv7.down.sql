-- Revert Phase A (EXPAND) of the v7-id program for `oauth_tokens`.
DROP TRIGGER IF EXISTS oauth_tokens_uuidv7_autofill;
DROP INDEX IF EXISTS idx_oauth_tokens_uuidv7;
ALTER TABLE oauth_tokens DROP COLUMN uuidv7;
