-- Revert Phase A (EXPAND) of the v7-id program for `host_addressing`.
DROP TRIGGER IF EXISTS host_addressing_uuidv7_autofill;
DROP INDEX IF EXISTS idx_host_addressing_uuidv7;
ALTER TABLE host_addressing DROP COLUMN uuidv7;
