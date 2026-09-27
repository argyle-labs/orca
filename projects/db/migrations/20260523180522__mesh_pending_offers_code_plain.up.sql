-- NOTE: this migration predates the mesh table rename (20260927000000).
-- It now spells them `mesh_*`, which is safe because it can only ever run on a
-- FRESH database: any database old enough to predate the rename recorded this
-- migration as applied long before the rename, so it is never replayed there.
-- On a fresh database `apply_schema` creates `mesh_*` directly. The single
-- migration that must handle BOTH shapes is 20260927000000, which guards itself.
--
ALTER TABLE mesh_pending_offers ADD COLUMN code_plain TEXT;
