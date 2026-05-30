-- Transport (command/args/env/url/token_env) is plugin-authored and lives in
-- the manifest. Storing it on the row duplicates the source of truth and lets
-- the two drift. After this migration, the host re-parses manifest_path at
-- dial time via db::plugin_manifest.
ALTER TABLE plugins DROP COLUMN mcp_command;
ALTER TABLE plugins DROP COLUMN mcp_args;
ALTER TABLE plugins DROP COLUMN mcp_env;
ALTER TABLE plugins DROP COLUMN mcp_url;
ALTER TABLE plugins DROP COLUMN mcp_token_env;
