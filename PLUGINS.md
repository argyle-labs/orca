# Orca Plugin System

Orca has two ways to extend the tool surface. Both are first-class.

1. **Native cdylib plugins** — a Rust crate built as a `cdylib`, loaded
   in-process at runtime via `abi_stable` (dlopen + layout/version gate). This
   is the model for new first-party integrations. The author depends on a
   single gateway crate, `plugin-toolkit`, exports an ABI root module, and
   registers tools with `#[orca_tool]`. See
   [docs/plugin-authoring.md](docs/plugin-authoring.md).

2. **Manifest plugins (`orca-plugin.toml`)** — a registry entry that points at
   an external MCP server (stdio or HTTP/SSE) plus optional nav links, command
   aliases, and agents. Useful for non-Rust or out-of-process integrations.
   The manifest schema is parsed by `db::plugin_manifest`.

## Quick start

```bash
# Register a manifest plugin
orca plugin add ~/code/my-plugin/orca-plugin.toml

# List registered plugins
orca plugin list

# Read/write plugin data (encrypted KV, per plugin)
orca plugin data-set my-plugin my-key "value"
orca plugin data-get my-plugin my-key
orca plugin data-list my-plugin
```

## Guides

- **[Writing an Orca plugin](docs/plugin-authoring.md)** — both the native
  cdylib model (`plugin-toolkit` + `#[orca_tool]` + `#[export_root_module]`)
  and the `orca-plugin.toml` manifest model.
- **[Plugin architecture](docs/planned/plugin-architecture.md)** — the
  tiering model and where it is headed (aspirational).

## First-party plugins

These ship in the orca repo under `projects/plugins/`. They are compiled into
the binary as library crates (`rlib`) and dispatched through the `#[orca_tool]`
macro — the in-tree "core integration" plugins.

| Plugin | Crate | Description | Tools |
|--------|-------|-------------|-------|
| `agents` | `projects/plugins/agents` | Embedded agent prompts + resolution helpers | `agent.list`, `agent.get` |
| `docker` | `projects/plugins/docker` | Docker/compose integration — engine status + project CRUD over the CLI | `docker.{list,detail,create,update,delete}` |
| `llm` | `projects/plugins/llm` | LLM backends — model discovery + inference across Claude, Ollama, LM Studio | `model.{list,detail,create,update,delete}` |
| `mcp` | `projects/plugins/mcp` | MCP server registry + federation passthrough (long-lived `McpPool` JSON-RPC client) | `mcp.{list,detail,update,delete,run}` |
| `smb` | `projects/plugins/smb` | SMB/CIFS storage adapter — mount, share discovery, credentials (via `plugin_toolkit::storage`, no `#[orca_tool]`) | — (storage backend) |

## First-party native cdylib plugins

These are first-party Orca Labs integrations built as standalone `cdylib`
plugins in their own repos, loaded in-process at runtime via `plugin-loader`.
They are the worked reference for the native plugin model.

| Plugin | Repo | Description |
|--------|------|-------------|
| `jellyfin` | `argyle-labs/jellyfin` | Jellyfin media-server control — typed client codegen'd from the Jellyfin OpenAPI spec via `plugin-toolkit-build` |
| `plex` | `argyle-labs/plex` | Plex media-server control |

> See [docs/tools/jellyfin.md](docs/tools/jellyfin.md),
> [docs/tools/plex.md](docs/tools/plex.md), and
> [docs/tools/dockge.md](docs/tools/dockge.md) for per-service operator notes.
