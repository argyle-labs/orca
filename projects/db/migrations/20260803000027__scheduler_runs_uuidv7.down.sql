-- Revert Phase A (EXPAND) of the v7-id program for `scheduler_runs`.
DROP TRIGGER IF EXISTS scheduler_runs_uuidv7_autofill;
DROP INDEX IF EXISTS idx_scheduler_runs_uuidv7;
ALTER TABLE scheduler_runs DROP COLUMN uuidv7;
