# Plugin architecture — two tiers

Orca has two plugin tiers. Both are first-class. The earlier
"everything is MCP-stdio" design is superseded — see the bottom of
this doc for what's preserved from it.

```
┌─────────────────────────────────────────────────────────────┐
│ orca daemon (single binary)                                 │
│                                                             │
│  ┌────────────────────┐      ┌──────────────────────────┐   │
│  │ Core integrations  │      │ Reconcilers              │   │
│  │ (in-process Rust)  │◀────▶│  - lxc-vm-reconciler     │   │
│  │                    │ call │  - storage-shares        │   │
│  │  proxmox, nfs,     │ into │  - host-lifecycle        │   │
│  │  smb, docker,      │      │  - backup-restore        │   │
│  │  dockge, unraid,   │      │  - secrets/envs          │   │
│  │  homeassistant,    │      │                          │   │
│  │  pbs, opnsense,    │      │ NOT plugins. orca-       │   │
│  │  adguard           │      │ internal subsystems.     │   │
│  └────────────────────┘      └──────────────────────────┘   │
│           ▲                            │                    │
│           │ in-process fn call         │ MCP tool call      │
│           │                            ▼                    │
│           │           ┌──────────────────────────────┐      │
│           │           │ Plugin host runtime          │      │
│           │           │ (subprocess + mTLS JSON-RPC) │      │
│           │           └──────────────┬───────────────┘      │
│           │                          │                      │
└───────────┼──────────────────────────┼──────────────────────┘
            │                          │
            ▼                          ▼
   (function dispatch)          ┌──────────────────────┐
                                │ SDK / 3rd-party      │
                                │ (orca-plugin.toml +  │
                                │  MCP-stdio sidecar)  │
                                │  jaguar, ibis,       │
                                │  ferret, etc.        │
                                └──────────────────────┘
```

## Tier 1 — Core integrations (in-process Rust crates)

What's already shipped. Each lives at `projects/plugins/<name>/` as a
Rust crate, linked into the orca binary at build time, dispatched by
the `#[orca_tool]` macro.

**Members:** proxmox, nfs, smb, docker, dockge, unraid, homeassistant,
agents, arr, db, graphql, llm, mcp, openapi, runtime. Future:
pbs, opnsense, adguard, caddy.

> **Note:** `projects/plugins/ntfy/` is **not** a plugin despite
> living under `plugins/` — its own `lib.rs` declares it a library
> ("no plugin scaffolding"). Tracked for relocation in ROADMAP CC.2.

**Properties:**

- In-process function call from reconcilers. No IPC overhead.
- Shipped + signed as part of the orca release.
- Sees orca's secret backend, config store, mesh — same memory.
- Lifecycle = orca's lifecycle.
- Owned by the orca repo. Adding one requires PR review.

## Tier 2 — SDK / third-party (orca-plugin.toml + MCP-stdio)

For experimental, user-owned, or non-Rust integrations. Spec lives in
[`../plugin-authoring.md`](../plugin-authoring.md).

**Members:** jaguar (TS / arr), ibis (Kotlin / media), ferret (TS /
download clients), anything a user writes.

**Properties:**

- Separate process. MCP JSON-RPC 2.0 over stdio (subprocess) or mTLS
  JSON-RPC over the pod mesh.
- Declares itself via `orca-plugin.toml`.
- Owns its own secrets via `[plugin.secrets]` declaration — see §4.
- Crashes are isolated. Lifecycle independent of orca.
- Written in any language with an MCP SDK.

## Tier-selection criteria

Pick **Tier 1 (in-process)** when:

- Hot path (called many times per reconcile loop).
- Tight coupling to orca internals (secret backend access, mesh, audit).
- Lifecycle must match the daemon (e.g. mesh peer discovery).
- Ownership belongs to the orca core team.

Pick **Tier 2 (SDK)** when:

- Cold path / event-driven.
- Calls an external service whose churn is independent of orca.
- Written in a non-Rust language (JS, Kotlin, Python).
- Third-party ownership.

Default: integrations that already exist in `projects/plugins/` stay
in Tier 1. New integrations against external APIs default to Tier 2
unless one of the Tier 1 criteria applies.

---

## Reconcilers ≠ plugins

This distinction is load-bearing. Reconcilers are **orca-internal
subsystems** that observe desired state vs realized state and emit
drift events. They are not pluggable; they ship with orca.

| Subsystem | What it reconciles | Calls into |
|---|---|---|
| lxc-vm-reconciler | meerkat/proxmox/configs/*.conf ↔ live /etc/pve/* | proxmox plugin (Tier 1) |
| storage-shares | shares.toml ↔ /etc/exports + smb.conf + Avahi + wsdd | nfs, smb plugins (Tier 1) |
| host-lifecycle | drivers.toml + updates.toml ↔ host state | system + plugin adapters |
| backup-restore | backup policies ↔ snapshot inventory | pbs, arr, homeassistant plugins |
| secrets/envs | declared projections ↔ realized envs on targets | secret backends + adapter plugins |
| network (planned) | DNS / firewall / DHCP declarations ↔ live | adguard, opnsense plugins |

### How a reconciler invokes a plugin

**Tier 1 (in-process):** the reconciler holds a typed handle to the
plugin crate and calls Rust functions directly. Errors are `Result<T,
E>`; cancellation is `tokio::CancellationToken`. No serialization.

```rust
// inside the lxc-vm-reconciler
self.proxmox.lxc_set(node, vmid, &delta).await?;
```

**Tier 2 (SDK):** the reconciler issues an MCP tool call through the
plugin host runtime. Args are serialized JSON; errors are MCP error
codes. The plugin host owns subprocess lifecycle, mTLS, and retries.

```rust
// inside any reconciler
self.plugins.call("jaguar", "sonarr.arr.health", json!({})).await?;
```

Reconcilers never depend on whether the called plugin is Tier 1 or
Tier 2 at the design level — but the call site is different. There's
no auto-bridging; the reconciler picks the right surface explicitly.

---

## `[plugin.secrets]` — secrets contract for Tier 2

Tier 2 plugins must declare the secrets and envs they require so the
env+secret reconciler (ROADMAP §1.11) can project them. The orca-side
projection layer reads this block; the plugin reads the resulting env
vars normally.

```toml
[plugin]
id      = "jaguar"
version = "0.3.0"

[plugin.secrets]
# Required secrets — handle resolves via the configured backend.
SONARR_API_KEY  = { handle = "op://Orca/services.sonarr/api_key",  required = true }
RADARR_API_KEY  = { handle = "op://Orca/services.radarr/api_key",  required = true }

# Required non-secret envs — cleartext OK, projected as plain env.
SONARR_URL = { value = "http://10.10.10.x:8989", required = true }
RADARR_URL = { value = "http://10.10.10.x:7878", required = true }

# Optional — plugin operates without them but logs a warning.
PROWLARR_API_KEY = { handle = "op://Orca/services.prowlarr/api_key", required = false }
```

Tier 1 plugins declare the same contract in code (typed struct on the
plugin crate's config). Both feed into the projection adapter in
`projects/plugins/<env-projection>/` (planned, ROADMAP §1.11).

**Handle grammar:** see [secrets-identity.md](secrets-identity.md). One
1Password vault per orca deployment (default `Orca`); items are named
`automations.<host>` or `services.<name>` (the dot is part of the
*item title*, not a URI separator — 1Password `op://` is exactly
three segments).

---

## Spec-first for service plugins

Every Tier 2 plugin that wraps a documented HTTP API must be built
against the published spec. Pull the spec before writing connector
code. Service inventories (jaguar, ibis, ferret) live in
[plugin-authoring.md](../plugin-authoring.md).

---

## What's shipped, what's missing

### Shipped (Tier 1)

`projects/plugins/{agents, arr, db, docker, dockge, graphql,
homeassistant, llm, mcp, nfs, ntfy, openapi, proxmox, runtime, smb,
unraid}` — all in-process today.

### Shipped (Tier 2 infrastructure)

`projects/plugins/runtime` — subprocess host + mTLS JSON-RPC.
`projects/sdk` — multi-language SDK (rust / go / ts / kotlin).
`orca-plugin.toml` manifest spec — see [plugin-authoring.md](../plugin-authoring.md).

### Missing

- **`[plugin.secrets]` block parsing + projection wiring.** Plugin
  manifests can declare; the env-projection reconciler that consumes
  the declaration is ROADMAP §1.11.
- **Tier 2 example in tree.** jaguar / ibis / ferret are planned —
  none shipped yet.
- **Tier 1 → Tier 2 graduation path** (a Rust plugin moving out of
  process) is undocumented. Punt until a real case exists.

---

## Superseded design notes

Earlier drafts modeled all integrations — including proxmox, nfs, smb,
docker — as MCP-stdio sidecars. That is no longer the architecture.
What is preserved from those drafts:

- Tool naming convention `{instance}.{domain}.{operation}`.
- Federated tool surface — Claude / clients see one flat namespace.
- Spec-first for HTTP-API plugins.
- Secrets stay with the process that uses them.

What is **not** preserved:

- Per-plugin `mode` and `mcp_transport` DB columns (dropped in
  migrations `20260530130000__plugins_drop_mode.up.sql` and
  `20260530140000__plugins_drop_mcp_transport.up.sql`). Don't bring
  them back.
- The "host plugin vs service plugin" distinction as a stored
  attribute. It's a deployment-time fact, not a contract field.
- The implication that meerkat is a plugin. Meerkat is retired (orca
  `feedback_no_rebuy_or_meerkat_in_orca.md`).
