-- mode column is redundant — grouping is handled by other mechanisms
-- (namespace scoping, tier, plugin id). Removing the column unifies how
-- plugins are grouped and eliminates the per-row "which UI bucket" lookup.
ALTER TABLE plugins DROP COLUMN mode;
