# Manifest plugins (`orca-plugin.toml`)

For non-Rust or third-party integrations. A manifest registers an external MCP
server (stdio or HTTP/SSE) plus optional nav links, command aliases, vault roots,
and agents. No Rust, no `plugin-toolkit`.

## The manifest

```toml
[plugin]
id                = "my-plugin"           # unique, lowercase, hyphenated
version           = "0.1.0"
tier              = "personal"            # personal | external | homelab
description       = "What this plugin does"
context_injection = "minimal"             # minimal | full

# ── MCP server (stdio) ──────────────────────────────────────────────
[plugin.mcp]
command = "node"
args    = ["/abs/path/to/dist/index.js"]

[plugin.mcp.env]
MY_API_KEY = ""   # projected from the secret backend at dial time

# ── MCP server (HTTP/SSE) — alternative to stdio ────────────────────
# [plugin.mcp]
# url       = "http://<host>:12050"       # or `urls = [...]` for LAN/TS fallbacks
# token_env = "MY_PLUGIN_TOKEN"

# ── Command aliases: short alias → MCP tool name ────────────────────
[plugin.commands]
run    = "my_plugin_run"
status = "my_plugin_status"

[[plugin.nav_links]]
href  = "/my-page"
label = "My Page"

[plugin.agents]
manifest_dir = "agents/"
```

Transport (`command`/`args`/`url`) lives in the manifest on disk, not in DB
columns — the host re-reads it at dial time.

## Register

```bash
orca plugin install ~/code/my-plugin/orca-plugin.toml
orca plugin list
orca plugin uninstall my-plugin
```

## Writing the MCP server (TypeScript)

```typescript
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const server = new McpServer({ name: "my-plugin", version: "0.1.0" });

server.tool(
  "my_plugin_run",
  "Short description of what this tool does.",
  { input: z.string().describe("The input value") },
  async ({ input }) => ({ content: [{ type: "text", text: doWork(input) }] }),
);

await server.connect(new StdioServerTransport());
```
