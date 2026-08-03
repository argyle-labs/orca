-- Revert Phase A (EXPAND) of the v7-id program for `doc_roots`.
DROP TRIGGER IF EXISTS doc_roots_uuidv7_autofill;
DROP INDEX IF EXISTS idx_doc_roots_uuidv7;
ALTER TABLE doc_roots DROP COLUMN uuidv7;
