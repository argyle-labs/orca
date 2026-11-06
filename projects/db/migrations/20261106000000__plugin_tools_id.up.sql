-- Committed UUIDv7 a plugin declares for each tool; NULL until the plugin adopts tool ids.
ALTER TABLE plugin_tools ADD COLUMN tool_id TEXT;
CREATE INDEX IF NOT EXISTS idx_plugin_tools_tool_id ON plugin_tools(tool_id);
