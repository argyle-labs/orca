# Orca Plugin System

Orca has two ways to extend the tool surface. Both are first-class.

1. **Native subprocess plugins** — a Rust crate built as a small binary that
   the daemon spawns and speaks to over a Unix socket (`plugin-proto` wire
   protocol, capability-delegated). This is the model for first-party
   integrations. The author depends on a single gateway crate,
   `plugin-toolkit`, registers tools with `#[orca_tool]`, and serves them via
   `plugin_toolkit::serve`. See
   [docs/plugin-authoring.md](docs/plugin-authoring.md) and
   [docs/dynamic-linking.md](docs/dynamic-linking.md).

2. **Manifest plugins (`orca-plugin.toml`)** — a registry entry that points at
   an external MCP server (stdio or HTTP/SSE) plus optional nav links, command
   aliases, and agents. Useful for non-Rust or out-of-process integrations.
   Manifest parsing lives in core (`db::plugin_manifest`) at registration time.
   A future `plugin_toolkit::manifest` seam will absorb the in-plugin `toml` +
   `parse_path` parsing that plugins inline today.

## Quick start

```bash
# Register a manifest plugin
orca plugin install ~/code/my-plugin/orca-plugin.toml

# List registered plugins
orca plugin list

# Remove a plugin
orca plugin uninstall my-plugin
```

## Guides

- **[Writing an Orca plugin](docs/plugin-authoring.md)** — both the native
  subprocess model (`plugin-toolkit` + `#[orca_tool]` + `serve`) and the
  `orca-plugin.toml` manifest model.
- **[Dynamic linking](docs/dynamic-linking.md)** — how the daemon loads and
  runs plugins at runtime.

## First-party plugins

**Each first-party plugin is its own repository** under the
[`argyle-labs`](https://github.com/argyle-labs) org and registers with orca like
any other plugin. Most build as a native subprocess binary whose only orca
dependency is `plugin-toolkit`, spawned and driven at runtime by
`plugin-loader`. These standalone repos are the **canonical homes**.

### Infrastructure & hosts

| Plugin | Repo | Description |
|--------|------|-------------|
| `proxmox` | [argyle-labs/proxmox](https://github.com/argyle-labs/proxmox) | Proxmox VE — nodes/guests, cluster status, plus `cluster_roster` + `topology` ABI backends |
| `unraid` | [argyle-labs/unraid](https://github.com/argyle-labs/unraid) | Unraid host GraphQL — typed queries, endpoint registry, topology, schema-drift detection |
| `docker` | [argyle-labs/docker](https://github.com/argyle-labs/docker) | Docker Engine + Compose adapted into orca's containers domain |
| `dockge` | [argyle-labs/dockge](https://github.com/argyle-labs/dockge) | Dockge — self-hosted Docker Compose stack manager (plugin + deploy assets) |

### Web UI

| Plugin | Repo | Description |
|--------|------|-------------|
| `peacock` | [argyle-labs/peacock](https://github.com/argyle-labs/peacock) | The orca web UI — a SvelteKit 2 + Svelte 5 app (project at `peacock/ui/`). Registers `contract::web` and owns the root route `/`; orca core proxies unmatched `/` requests to its `peacock.render` tool in prod, or to its Vite dev server (`dev_upstream`) in dev. The `ui.enabled` DB setting gates the `/` owner at runtime; with no web plugin registered, orca is headless. |

### Storage

| Plugin | Repo | Description |
|--------|------|-------------|
| `nfs` | [argyle-labs/nfs](https://github.com/argyle-labs/nfs) | NFS `StorageBackend` with stale-mount self-heal (backend-only) |
| `smb` | [argyle-labs/smb](https://github.com/argyle-labs/smb) | SMB/CIFS `StorageBackend` for orca's storage domain (backend-only) |

### Media

| Plugin | Repo | Description |
|--------|------|-------------|
| `plex` | [argyle-labs/plex](https://github.com/argyle-labs/plex) | Self-hosted Plex with GPU hardware transcoding + orca lifecycle/diagnostics |
| `jellyfin` | [argyle-labs/jellyfin](https://github.com/argyle-labs/jellyfin) | Self-hosted Jellyfin with GPU hardware transcoding + orca lifecycle/diagnostics |
| `arr` | [argyle-labs/arr](https://github.com/argyle-labs/arr) | The *arr stack — Sonarr, Radarr, Prowlarr, Lidarr — in one plugin |

### AI, messaging & home

> The `model.*` registry and the model engine + provider backends (Anthropic,
> claude-code, Ollama, LM Studio) live in **core** (`projects/model`) — there is
> no `llm` plugin. The entries below are the *local runner* service plugins.

| Plugin | Repo | Description |
|--------|------|-------------|
| `ollama` | [argyle-labs/ollama](https://github.com/argyle-labs/ollama) | Local LLM runner `ServiceBackend` (docker/podman/lxc/vm) |
| `lmstudio` | [argyle-labs/lmstudio](https://github.com/argyle-labs/lmstudio) | LM Studio local LLM runner `ServiceBackend` — host desktop app, OpenAI-compatible server on :1234 (connect+configure, no deploy) |
| `mcp` | [argyle-labs/mcp](https://github.com/argyle-labs/mcp) | Federates MCP servers (stdio + HTTP/SSE) into orca's tool surface — an MCP client |
| `ntfy` | [argyle-labs/ntfy](https://github.com/argyle-labs/ntfy) | ntfy push notifications — a notifications backend + self-host deploy lifecycle |
| `homeassistant` | [argyle-labs/homeassistant](https://github.com/argyle-labs/homeassistant) | Home Assistant — lifecycle + entities/automations/service API |

> See [docs/tools/jellyfin.md](docs/tools/jellyfin.md),
> [docs/tools/plex.md](docs/tools/plex.md), and
> [docs/tools/dockge.md](docs/tools/dockge.md) for per-service operator notes.

### Migration in progress: in-tree plugins

The one-repo-per-plugin layout is still being rolled out. Some plugin code
**currently still lives in-tree** under `projects/plugins/` and is being moved
out to its own repo — treat `projects/plugins/` as a **transitional** location,
not the place to add a new plugin:

| In-tree crate | Status |
|---------------|--------|
| `projects/plugins/agents` | Core embedded agent prompts + resolution (`agent.list`, `agent.get`) — stays in-tree |
| `projects/plugins/docker` | Transitional copy; canonical home is [argyle-labs/docker](https://github.com/argyle-labs/docker) |
| `projects/plugins/mcp` | Transitional copy; canonical home is [argyle-labs/mcp](https://github.com/argyle-labs/mcp) |

To author a **new** plugin, create a standalone repo — see
[docs/plugin-authoring.md](docs/plugin-authoring.md).
