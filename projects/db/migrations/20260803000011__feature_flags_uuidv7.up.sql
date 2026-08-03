-- Phase A (EXPAND) of the v7-id program for `feature_flags`.
-- Adds a v7 `uuidv7` as a PASSENGER column; the existing key stays PRIMARY KEY.
-- Non-disruptive: no FK/merge-key/cursor is repointed here (that is Phase C,
-- after the id has propagated + verified stable). Backfill + new-row minting
-- both use the registered `uuidv7()` scalar (single source of truth =
-- utils::id::new), so every row — existing and future — gets a distinct
-- time-ordered id with no per-table Rust insert wiring.
ALTER TABLE feature_flags ADD COLUMN uuidv7 TEXT;
UPDATE feature_flags SET uuidv7 = uuidv7() WHERE uuidv7 IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_feature_flags_uuidv7 ON feature_flags(uuidv7);
CREATE TRIGGER IF NOT EXISTS feature_flags_uuidv7_autofill
AFTER INSERT ON feature_flags
WHEN NEW.uuidv7 IS NULL
BEGIN
    UPDATE feature_flags SET uuidv7 = uuidv7() WHERE rowid = NEW.rowid;
END;
