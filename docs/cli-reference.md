# CLI reference

`orca` is the command-line surface. Everything is one of two shapes:

1. **Tool nouns** — `orca <noun> <verb>`. Every `#[orca_tool]` in a domain
   crate is emitted automatically as a clap subcommand; the same tool is the
   REST and MCP surface. No per-command wiring.
2. **Built-in commands** — a small curated set (`serve`, `run`, `daemon`, …)
   that are process entrypoints rather than tools.

Run `orca --help` for the build-current top level, and `orca <noun> --help`
for a noun's verbs — those are always authoritative. This page is the map.

## Remote dispatch

Any non-`local_only` tool takes `--peer <hostname>` to run on another pod
member over the mTLS mesh; the peer enforces the same role checks as a local
call. Example: `orca config list --peer willow`.

---

## Managed Unit nouns

A VM, an LXC, and a Docker container are all things you CRUD, so they share
one verb set — the **Managed Unit** surface (`docs/MANAGED-UNIT.md`). The
canonical tool name is `<kind>.<verb>` (e.g. `vm.list`), and the CLI exposes
each kind as a **top-level noun**:

| Noun | Example | Provided by |
|---|---|---|
| `container` | `orca container list`, `orca container exec <id> -- <cmd>` | docker plugin |
| `vm` | `orca vm start <id>`, `orca vm snapshot <id>` | proxmox plugin |
| `lxc` | `orca lxc list`, `orca lxc reconcile <id>` | proxmox plugin |

Canonical verbs: `list`, `detail`, `create`, `update`, `delete`, `upsert`,
plus provider-specific actions (`exec`, `start`, `stop`, `snapshot`, …). These
nouns appear **only when the providing plugin is loaded** — units are plugin
territory, not core.

---

## Tool nouns by area

Core domains (always present). Verbs shown are representative — use
`orca <noun> --help` for the full, current list.

### Config & data
| Noun | Verbs | What |
|---|---|---|
| `config` | list, detail, upsert, delete | declarative config rows (host-owned, mesh-routed) |
| `schema` | list, detail, create, delete | JSON schemas for config nouns |
| `spec` | list, detail, create, refresh, delete | OpenAPI / GraphQL spec registry |
| `db` | stats, detail, sweep, compact, update | local SQLite maintenance |
| `files` | list, read, update, delete, stat, search, tree | filesystem surface |

### Identity & secrets
| Noun | Verbs | What |
|---|---|---|
| `secrets` | list, detail, create, update, upsert, delete | encrypted secret store |
| `auth` | login, logout | session auth |
| `pki` | list, create | CA / peer certificate material |

### Fleet & mesh
| Noun | Verbs | What |
|---|---|---|
| `pod` | list, join, leave, kick, trust, ping, sync, snapshot, … | mesh membership + pairing |
| `namespace` | list, detail, create, use, delete | per-user shareable workspaces |
| `inventory` | tree, detail | resource inventory view |
| `network` | topology_view | topology / drift view (read-only) |
| `host` | info | host identity + facts |

### Lifecycle & ops
| Noun | Verbs | What |
|---|---|---|
| `system` | install, delete, update, build, kill, serve_release, detail, capability_*, retention_* | host + orca lifecycle |
| `schedule` | list, run, status | cron / periodic jobs |
| `service` | list, status, deploy, backup, restore, configure | managed services |
| `storage` | list, shares, mount, unmount, recover | NFS / SMB client mounts |
| `notify` | send | unified event dispatcher |

### Extend
| Noun | Verbs | What |
|---|---|---|
| `plugin` | list, detail, install, uninstall, create, update, delete | plugin registry (see `docs/dynamic-linking.md`) |
| `agent` | list, get, run | registered agents (from plugins) |

---

## Built-in commands

Process entrypoints, not tools:

| Command | What |
|---|---|
| `orca` | interactive session (the default) |
| `orca run -a <agent> "…"` | one-shot: send a prompt to an agent, print the response |
| `orca escalate "…"` | ask a hosted model directly (non-interactive escalation) |
| `orca audit <project>` | run the Bear audit (deps + code review) |
| `orca log` | search / manage session logs |
| `orca serve` | web UI + REST + MCP-over-HTTP on `:12000` / `:12443` |
| `orca mcp-serve` | MCP stdio server (register with Claude Code) |
| `orca daemon` | run as the managed daemon (cooperative port handoff) |
| `orca dev` / `orca dev-serve` | dev-mode daemon + fleet hot-reload |
| `orca pod` | pod / mesh bootstrap + management (also the `pod` tool noun) |
| `orca hook` | Claude Code hook handlers |
| `orca openapi` | emit orca's own OpenAPI 3 spec to stdout |
| `orca admin` | local-only admin commands (never exposed over REST/MCP) |

> **Target agent surface** (see `docs/ROADMAP.md` §1.9): `orca agents` will
> launch the interactive agent surface, `orca agents fox "…"` will run a
> named agent, and `orca agents "…"` will route through the top-level orca
> agent. This activates once agents and model backends are supplied by
> plugins. Today, `orca run` / `orca escalate` are the agent entrypoints.
