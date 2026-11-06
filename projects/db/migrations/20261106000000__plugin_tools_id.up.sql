-- Committed UUIDv7 a plugin declares for each tool; NULL until the plugin adopts tool ids.
ALTER TABLE plugin_tools ADD COLUMN id TEXT;
CREATE INDEX IF NOT EXISTS idx_plugin_tools_id ON plugin_tools(id);
