-- Add plugin_namespace column to plugin_tools and plugin_types. fq_name/fq_type_id
-- is now stamped as `{namespace}.{name}` (namespace defaults to plugin_id for
-- back-compat). The existing UNIQUE constraint on fq_name/fq_type_id enforces
-- the reject-on-collision policy when two plugins share a namespace.

ALTER TABLE plugin_tools ADD COLUMN plugin_namespace TEXT NOT NULL DEFAULT '';
UPDATE plugin_tools SET plugin_namespace = plugin_id WHERE plugin_namespace = '';

ALTER TABLE plugin_types ADD COLUMN plugin_namespace TEXT NOT NULL DEFAULT '';
UPDATE plugin_types SET plugin_namespace = plugin_id WHERE plugin_namespace = '';
