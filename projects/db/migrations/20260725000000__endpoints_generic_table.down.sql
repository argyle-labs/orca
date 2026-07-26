-- Revert the legacy-copy data migration. The `endpoints` table is owned by
-- apply_schema (core-migrated), so it is left in place; only the rows this
-- migration copied in are removed.
DELETE FROM endpoints WHERE provider = 'ntfy';
