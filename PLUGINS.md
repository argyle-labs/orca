# Orca Plugin System

## Quick start

```bash
# Register a plugin
orca plugin add ~/code/my-plugin/orca-plugin.toml

# List registered plugins
orca plugin list

# Read/write plugin data
orca plugin data-set my-plugin my-key "value"
orca plugin data-get my-plugin my-key
orca plugin data-list my-plugin
```

## Guides

- **[Writing an Orca plugin](docs/plugin-authoring.md)** — `orca-plugin.toml` schema, MCP server scaffold, data API, UI pages, agents. The [leetcode plugin](../leetcode/orca-plugin/) is a complete reference example.

- **[Writing a Meerkat plugin](../meerkat/docs/plugin-authoring.md)** — MCP binary interface, `meerkat.toml` `[[plugin]]` blocks, wiring meerkat hosts to Orca. Covers building services like the arr stack (jaguar: Sonarr, Radarr, Prowlarr, qBittorrent).

## Existing plugins

| Plugin | Type | Description |
|--------|------|-------------|
| `leetcode` | stdio MCP (Node.js) | LeetCode practice — run problems, track progress |
| `rebuy` | stdio MCP (Node.js) | Rebuy engineering — API docs, Jira, Confluence, Bitbucket |
| `meerkat` | stdio MCP (Go) | Homelab local: Proxmox, Docker, Unraid, Home Assistant |
| `meerkat-willow` | HTTP MCP | Willow NAS — infra host |
| `meerkat-freyr` | HTTP MCP | Freyr — arr stack (Sonarr, Radarr, qBittorrent) |
| `meerkat-baldur` | HTTP MCP | Baldur — media/library host |
