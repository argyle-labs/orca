-- Per-identity opt-in to perform data mutations without full admin. A read/
-- member identity holding can_mutate = 1 may invoke tools marked
-- DATA_MUTATION (writes against external managed systems) that would otherwise
-- require admin. Control-plane admin tools are never DATA_MUTATION, so this
-- opt-in cannot reach them. Default 0 (off) preserves existing behavior.
ALTER TABLE api_tokens ADD COLUMN can_mutate INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN can_mutate INTEGER NOT NULL DEFAULT 0;
