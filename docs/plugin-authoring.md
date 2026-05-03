# Orca Plugin Authoring Guide

Plugins extend Orca with new MCP tools, sidebar navigation, UI pages, agents, and persistent data. Each plugin is an independent process that speaks the MCP protocol over stdio or HTTP. Orca manages its lifecycle and wires it into the system.

---

## Anatomy of a plugin

A plugin has two parts:

1. **`orca-plugin.toml`** — the manifest. Declares the plugin's identity, its MCP server, nav links, commands, and agents.
2. **MCP server** — any process that implements the [MCP protocol](https://spec.modelcontextprotocol.io/) over stdio or HTTP. Written in any language.

```
my-plugin/
├── orca-plugin.toml      ← manifest
├── src/
│   └── index.ts          ← MCP server (TypeScript example)
├── dist/                 ← compiled output
└── agents/               ← optional agent .md files
    └── my-agent.md
```

---

## The manifest (`orca-plugin.toml`)

```toml
[plugin]
id                = "my-plugin"           # unique, lowercase, hyphenated
version           = "0.1.0"
tier              = "personal"            # personal | external | homelab
description       = "What this plugin does"
context_injection = "minimal"             # minimal | full — how much context agents get

# ── MCP server (stdio) ────────────────────────────────────────────────────────
[plugin.mcp]
command = "node"
args    = ["/abs/path/to/dist/index.js"]

# Environment variables injected into the process
[plugin.mcp.env]
MY_API_KEY = ""   # loaded from 1Password / orca creds

# ── MCP server (HTTP/SSE) — alternative to stdio ──────────────────────────────
# [plugin.mcp]
# command   = "http://10.10.10.5:12050"   # HTTP endpoint
# token_env = "MY_PLUGIN_TOKEN"           # env var holding the bearer token

# ── Universal command aliases ─────────────────────────────────────────────────
# Maps a short alias → the actual MCP tool name.
# These can be used in agent definitions and orca invocations.
[plugin.commands]
run    = "my_plugin_run"
status = "my_plugin_status"

# ── Sidebar navigation ────────────────────────────────────────────────────────
# Flat link
[[plugin.nav_links]]
href  = "/my-page"
label = "My Page"

# Collapsible group (label only, no href)
[[plugin.nav_links]]
label    = "My Tools"
children = [
  { href = "/my-page/a", label = "Section A" },
  { href = "/my-page/b", label = "Section B" },
]

# Panel component (sidebar widget, not a page link)
[[plugin.nav_links]]
panel = "services"   # renders ServicesPanel.svelte when declared

# ── Mode (UI section) ─────────────────────────────────────────────────────────
# Omit for "orca" (default). Set to create a separate sidebar section.
# mode = "rebuy"

# ── Vault doc roots ───────────────────────────────────────────────────────────
# Use //vault/ prefix to register a vault tree root in the sidebar.
# [[plugin.nav_links]]
# href  = "//vault/my-docs"
# label = "My Docs"

# ── Agents ───────────────────────────────────────────────────────────────────
[plugin.agents]
manifest_dir = "agents/"   # relative to this file
```

### Registration

```bash
orca plugin add ~/code/my-plugin/orca-plugin.toml
orca plugin list
orca plugin remove my-plugin
orca plugin enable my-plugin
orca plugin disable my-plugin
```

---

## Writing the MCP server (TypeScript)

The MCP SDK handles protocol boilerplate. Define tools with Zod schemas.

```typescript
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const server = new McpServer({ name: "my-plugin", version: "0.1.0" });

server.tool(
  "my_plugin_run",
  "Short description of what this tool does.",
  {
    input: z.string().describe("The input value"),
    count: z.number().default(10).describe("How many results"),
  },
  async ({ input, count }) => {
    const result = doWork(input, count);
    return { content: [{ type: "text", text: result }] };
  }
);

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch(console.error);
```

**`tsconfig.json` minimum:**
```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "Node16",
    "moduleResolution": "Node16",
    "lib": ["ES2022", "DOM"],
    "outDir": "dist",
    "strict": true
  }
}
```

**`package.json` minimum:**
```json
{
  "type": "module",
  "scripts": { "build": "tsc" },
  "dependencies": {
    "@modelcontextprotocol/sdk": "^1.x",
    "zod": "^3.x"
  },
  "devDependencies": { "typescript": "^5.x" }
}
```

Build: `npm run build` → outputs to `dist/index.js`.

---

## Persistent data (plugin_data)

Plugins store arbitrary key/value data in Orca's encrypted SQLite database. Values are strings — use JSON for structured data.

### From the MCP server

```typescript
const ORCA_URL = process.env.ORCA_API_URL ?? "http://localhost:12000";
const PLUGIN_ID = "my-plugin";

async function orcaGet(key: string): Promise<string | null> {
  try {
    const res = await fetch(`${ORCA_URL}/api/plugins/${PLUGIN_ID}/data/${key}`);
    if (!res.ok) return null;
    return ((await res.json()) as any).value ?? null;
  } catch { return null; }
}

async function orcaSet(key: string, value: string): Promise<void> {
  try {
    await fetch(`${ORCA_URL}/api/plugins/${PLUGIN_ID}/data/${key}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ value }),
    });
  } catch {}  // non-fatal — Orca may not be running in all contexts
}
```

### From the CLI

```bash
orca plugin data-set my-plugin my-key "hello world"
orca plugin data-get my-plugin my-key
orca plugin data-list my-plugin
orca plugin data-delete my-plugin my-key
```

### From the REST API

```
GET    /api/plugins/{id}/data           → list all entries
GET    /api/plugins/{id}/data/{key}     → get one entry
PUT    /api/plugins/{id}/data/{key}     → set { "value": "..." }
DELETE /api/plugins/{id}/data/{key}     → delete
```

### From the frontend

```typescript
import { getPluginData, setPluginData, listPluginData } from '$lib/api/client';

const entry = await getPluginData({ id: 'my-plugin', key: 'my-key' });
await setPluginData({ id: 'my-plugin', key: 'my-key', body: { value: 'hello' } });
```

---

## Adding a UI page

1. Create `src/routes/my-page/+page.svelte` and `+page.ts` in the frontend.
2. Add a nav link in `orca-plugin.toml`:
   ```toml
   [[plugin.nav_links]]
   href  = "/my-page"
   label = "My Page"
   ```
3. Load plugin data or call MCP tools from the page:
   ```typescript
   // +page.ts
   import { runMcpTool } from '$lib/api/client';
   export const load = async () => {
     const res = await runMcpTool({
       body: { server: 'my-plugin', name: 'my_plugin_run', arguments: { input: 'test' } }
     });
     return { result: res?.content?.[0]?.text ?? '' };
   };
   ```

For pages that need workspace configuration (URLs, tokens, etc.), show `WorkspaceSetup.svelte` when config is absent:
```svelte
<WorkspaceSetup
  service="My Service"
  fields={[{ key: 'api_url', label: 'API URL', required: true }]}
  onSave={async (values) => {
    await setPluginData({ id: 'my-plugin', key: 'workspace', body: { value: JSON.stringify(values) } });
    await invalidateAll();
  }}
/>
```

---

## Adding agents

Agents are markdown files with YAML frontmatter. Place them in the `agents/` directory declared in the manifest.

```markdown
---
name: my-agent
description: Does X using my-plugin tools
tools: Read, Glob, Bash, my_plugin.*
color: green
---

You are an agent that does X.

Use my_plugin_run to do the work.
```

Rebuild Orca after adding agents (they're compiled into the binary):
```bash
cd ~/code/orca && make install-dev
```

---

## Reference: the leetcode plugin

The leetcode plugin at `~/code/leetcode/orca-plugin/` is a complete real-world example:

| File | Purpose |
|------|---------|
| `orca-plugin.toml` | Manifest with MCP command, agents dir, nav link, command aliases |
| `src/index.ts` | MCP server: 5 tools, Orca data store helpers, solved-state tracking |
| `agents/leetcoder.md` | Agent that can read/run/write problems |
| `tsconfig.json` | TypeScript config (note: needs `"DOM"` in lib for `fetch`) |

Key patterns from `src/index.ts`:
- `orcaGet` / `orcaSet` — thin wrappers around the REST API for persistent state
- `solvedCache` — in-process cache populated lazily from Orca data
- `getDescription(num)` — strips `/* */` comment markers, returns clean markdown
- Tool return value: always `{ content: [{ type: "text", text: string }] }`

---

## Checklist for a new plugin

- [ ] `orca-plugin.toml` with `id`, `version`, `tier`, `description`
- [ ] MCP server with at least one tool
- [ ] `npm run build` (or equivalent) produces the binary/script
- [ ] Absolute path in `args` (or use a wrapper script that resolves `__dirname`)
- [ ] `orca plugin add ~/path/to/orca-plugin.toml`
- [ ] `orca mcp list` shows the server; test a tool with `orca mcp call my-plugin my_tool_name`
- [ ] Nav links added if the plugin has a UI page
- [ ] Agents added if the plugin has AI-assisted workflows
