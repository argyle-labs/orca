-- Phase A (EXPAND) of the v7-id program for `plugin_credentials`.
-- Adds a v7 `uuidv7` as a PASSENGER column; the existing key stays PRIMARY KEY.
-- Non-disruptive: no FK/merge-key/cursor is repointed here (that is Phase C,
-- after the id has propagated + verified stable). Backfill + new-row minting
-- both use the registered `uuidv7()` scalar (single source of truth =
-- utils::id::new), so every row — existing and future — gets a distinct
-- time-ordered id with no per-table Rust insert wiring.
ALTER TABLE plugin_credentials ADD COLUMN uuidv7 TEXT;
UPDATE plugin_credentials SET uuidv7 = uuidv7() WHERE uuidv7 IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_credentials_uuidv7 ON plugin_credentials(uuidv7);
CREATE TRIGGER IF NOT EXISTS plugin_credentials_uuidv7_autofill
AFTER INSERT ON plugin_credentials
WHEN NEW.uuidv7 IS NULL
BEGIN
    UPDATE plugin_credentials SET uuidv7 = uuidv7() WHERE rowid = NEW.rowid;
END;
