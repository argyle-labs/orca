# Orca Contexts

## Overview

Orca supports multiple named contexts. Each context defines which plugins,
MCP servers, agents, and dashboard views are active. Switching contexts changes
what Claude sees, what tools are available, and how information is presented.

This is the primary mechanism for keeping homelab, development, and work
concerns from bleeding into each other.

---

## Planned contexts

### homelab
Active plugins: meerkat (infrastructure), starr (arr stack), cascade (media),
fetch (download clients), AdGuard, OPNsense.
Dashboard: host health, array status, active downloads, now playing, alerts.
MCP tools exposed: all meerkat.* and plugin tools scoped to homelab.

### rebuy (work)
Active plugins: rebuy-cli MCP server (already exists), rebuy project tools,
ephemeral environments, release tooling.
Dashboard: PR status, ephemeral env health, release pipeline, spec diffs.
MCP tools exposed: rebuy_* tools only; homelab and dev tools hidden.

### dev (personal development)
Active plugins: orca itself (dogfooding), meerkat dev instance, GitHub.
Dashboard: open PRs across personal repos, CI status, recent commits.
MCP tools exposed: code-focused tools; infra tools available but not foregrounded.

---

## How contexts work

Each context is a named configuration profile:

```toml
# ~/.config/orca/contexts/homelab.toml
[context]
name        = "homelab"
description = "Home infrastructure and media"
plugins     = ["meerkat", "starr", "cascade", "fetch"]
agents      = ["meerkat-status", "meerkat-deploy", "badger"]
dashboard   = "homelab"

# ~/.config/orca/contexts/rebuy.toml
[context]
name        = "rebuy"
description = "Rebuy engineering"
plugins     = ["rebuy-cli"]
agents      = ["boar", "pinky", "rebuy-kb"]
dashboard   = "rebuy"
```

Switching contexts:
```
orca context switch homelab
orca context switch rebuy
orca context list
orca context current
```

The active context is stored in Orca's state (orca.db). The daemon re-registers
only the plugins and agents for the active context.

---

## Dashboard

The dashboard UI is shared — same layout, same components — populated by
different data sources depending on the active context. The dashboard definition
lives in the context config, not in the UI code.

A homelab dashboard card that shows "array status" in homelab context shows
"PR pipeline" in rebuy context. The card type is the same; the data source
and label differ.

Dashboard cards are defined in the context config as data source bindings:
- `mcp_tool` — call an MCP tool and render the result
- `agent` — run an agent and render its output
- `static` — render a static markdown block

---

## MCP namespace isolation

When a context is active, `tools/list` returns only tools registered to that
context's plugins. Tools from other contexts are not visible to Claude.

This prevents:
- Homelab tool calls during a rebuy coding session
- Work secrets visible during personal dev work
- Tool namespace pollution when many plugins are registered

Orca's dispatcher checks the active context before routing any tool call.
Calls to out-of-context tools return a standard "tool not found in current context"
error rather than an auth error, to avoid leaking what tools exist.

---

## CLI integration

```
orca context switch homelab     # switch active context
orca context list               # list all configured contexts with descriptions
orca context current            # show active context and its registered plugins
orca context add <path>         # register a new context config
orca context remove <name>      # deregister a context
```

The active context is also exposed as an MCP tool:
- `orca.context.current` — returns active context name and registered tools count
- `orca.context.list` — returns all configured contexts
- `orca.context.switch` — switches context (requires confirmation)

---

## Implementation notes

- Context switching restarts only the plugin processes that differ between
  the old and new context. Shared plugins stay running.
- The active context name is stored in orca.db alongside other state.
- Agent files (`.md`) are scoped to contexts — `orca context switch` changes
  which agents Claude Code sees in `.claude/agents/`.
- Dashboard definitions are a frontend concern; the backend just exposes
  the context config and tool namespace. The frontend renders accordingly.
- This replaces the current implicit "everything active all the time" model.
