# Orca Capability Registries — the platform architecture

> Canonical repo-wide architecture. The per-service plugin build-out
> (Phase 2 in `ROADMAP.md`) is one *instance* of the pattern described here.

## The one rule

**Core holds only abstractions — traits, registries, ABIs, engines, and
composition logic. Every concrete capability is an external `argyle-labs`
plugin that registers into a core registry.** If a plugin needs something from
core, we abstract *that thing* into a core trait; we never keep a plugin's
concrete logic in core. `projects/plugins/` no longer exists in the core repo.

This is the existing `service`/`deploy_target` design (trait + process-global
registry + JSON-proxy FFI, `BoxFuture` not `async_trait`, typed payloads only)
promoted from "the service domain" to "how the entire repo works."

## Capability registries (all live in core)

Each is a trait + a `LazyLock<RwLock<..>>` process-global registry + a
JSON-proxy FFI boundary, mirroring `projects/service`.

| Registry | What it abstracts | Status |
|---|---|---|
| `ServiceBackend` | software lifecycle: deploy/backup/restore/configure/status | **exists** (`projects/service`) |
| `DeployTarget` + `Runtime` | a *place to run*: host × {docker,podman,lxc,vm} | **exists** (`projects/deploy-target`) |
| `BackupMethod` | runtime/tool-agnostic backup (tar, pbs, restic, …) | **exists** (`plugin_toolkit::service`) |
| `StorageBackend` | nfs/smb and other storage providers | **exists** |
| `ModelProvider` | LLM providers (claude, ollama, lmstudio, …) | engine lifted to `projects/model`; **registry TODO** |
| `McpFederation` | registered external MCP servers (pool + passthrough) | **TODO** — externalize `plugins/mcp` |
| `AgentProvider` (agents + hooks + skills + slash commands + prompt fragments) | every Claude-acceptable artifact kind, composed | **registry + all compose sinks wired** (agents/skills/commands/fragments/hooks materialized via `orca install`; hooks→settings.json through a fully-typed `ClaudeSettings`, no opaque JSON; base roster bridged as a provider). The agents domain is **core** (`projects/agents`); plugins contribute agents/hooks/skills/commands/fragments through the `plugin_toolkit::agents::register` seam (the `agents.register` capability), like every other domain |
| `NetworkTopology` / discovery | network tools that *build the topology*; services then *expose functionality* on it | **TODO** — formalize |

## Two properties that make it a platform

### 1. One plugin registers against MANY capabilities (cross-domain)
A single plugin is not "a service plugin" — it exposes whatever set of core
capabilities it can back. Its `Hello` handshake declares a **set** of backend
registrations and the loader's `domain_register` table dispatches each.

> Example: a `proxmox` plugin registers as a **DeployTarget** (lxc/vm
> placement), one or more **ServiceBackend**s (a VM or LXC surfaced as a
> service), a **BackupMethod** (`vzdump`/PBS), and a **NetworkTopology**
> source (cluster nodes/links) — all from one repo.

### 2. Uniform abstraction to the user
A VM, an LXC, a container, a host app, and a device are all surfaced through the
**same `service.*` verbs**. "Any VM can be a service; any LXC can be a service."
The runtime/kind is a parameter, never a separate API. Discovery
(`NetworkTopology`) builds the map; `service.*` exposes the specific
functionality on each node.

### Plugins compose, not just register
A single `AgentProvider` from every loaded plugin can contribute **agents,
hooks, skills, slash commands, and CLAUDE.md fragments** — every artifact kind
Claude Code accepts (the trait grows a new defaulted accessor as more are
added). Core **composes** all contributions into (1) the materialized Claude
Code config (`~/.claude/{CLAUDE.md,agents/*,skills/*,commands/*}` +
`settings.json` hooks, today written by `orca install`) and (2) the internal
chat's subagent roster (`conversation`). The core agents domain supplies the
embedded base roster (wolf/otter/…) via `agents::embedded::register_base_roster()`.

## Migration — `projects/plugins/` removed

| Crate | Disposition |
|---|---|
| `llm` | ✅ removed — the model registry, engine, and provider backends (claude/ollama/lmstudio) all live in core `projects/model` |
| `agents` | ✅ core domain at `projects/agents` — `AgentProvider`/`HookProvider`/compose layer + embedded base roster; registration machinery exposed through `plugin_toolkit` (the `agents.register` capability) so any plugin can contribute agents/hooks/skills/commands/fragments |
| `mcp` | → external `argyle-labs/mcp` plugin + core `McpFederation` registry (abstract the `McpPool` the `/api/mcp/*` handlers need) |
| `docker` | → external `argyle-labs/docker` plugin implementing the core `Runtime`/`DeployTarget` trait (abstract what core calls into that trait) |

See [[core-abstractions-plugins-concrete]] and [[model-provider-core-registry]]
in project memory for the durable statement of intent.
