-- Revert Phase A (EXPAND) of the v7-id program for `learning_progress`.
DROP TRIGGER IF EXISTS learning_progress_uuidv7_autofill;
DROP INDEX IF EXISTS idx_learning_progress_uuidv7;
ALTER TABLE learning_progress DROP COLUMN uuidv7;
