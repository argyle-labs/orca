-- Revert Phase A (EXPAND) of the v7-id program for `doc_ignore_patterns`.
DROP TRIGGER IF EXISTS doc_ignore_patterns_uuidv7_autofill;
DROP INDEX IF EXISTS idx_doc_ignore_patterns_uuidv7;
ALTER TABLE doc_ignore_patterns DROP COLUMN uuidv7;
