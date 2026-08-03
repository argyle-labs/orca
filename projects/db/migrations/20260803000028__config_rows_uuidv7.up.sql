-- Phase A (EXPAND) of the v7-id program for `config_rows` — the one REPLICATED
-- natural-key table in the batch (registered in config_store.rs, not the derive).
--
-- Same passenger-column shape as the local tables, PLUS cross-peer convergence:
-- `config_rows` gossips fleet-wide, so a pre-existing row gets an independently
-- minted `uuidv7` on every peer. Plain LWW can't reconcile those (updated_at is
-- unchanged), so replicate_merge adopts MIN(local, incoming) — a deterministic,
-- order-independent CRDT selection that settles the whole fleet on one value.
-- The self-mint trigger handles brand-new locally-owned rows; mesh-received rows
-- arrive WITH the owner's uuidv7 already set (so the trigger's NULL guard skips).
-- Merge convergence lives in code (config_store::upsert_mesh_row), not here.
ALTER TABLE config_rows ADD COLUMN uuidv7 TEXT;
UPDATE config_rows SET uuidv7 = uuidv7() WHERE uuidv7 IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_config_rows_uuidv7 ON config_rows(uuidv7);
CREATE TRIGGER IF NOT EXISTS config_rows_uuidv7_autofill
AFTER INSERT ON config_rows
WHEN NEW.uuidv7 IS NULL
BEGIN
    UPDATE config_rows SET uuidv7 = uuidv7() WHERE rowid = NEW.rowid;
END;
