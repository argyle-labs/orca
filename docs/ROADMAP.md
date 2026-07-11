# Orca Roadmap

Canonical sequencing for orca development, grounded in the actual tree.
Every "shipped" claim cites a file path or a live `#[orca_tool]` verb;
every "missing" claim names what does not yet exist. When code and this
file disagree, the code wins — fix this file.

---

## What orca is

A local-first AI agent orchestrator and homelab control plane: **one
self-contained Rust binary** on every host in a pod, **one config repo**
as source of truth, and **one tool surface** exposed across three
surfaces — CLI, REST, and MCP. Every operation is an `#[orca_tool]`
(`projects/derive` + `projects/dispatch` + `projects/contract`) that
dispatches identically on all three. The web UI is not in this repo — it
is the out-of-process [`peacock`](https://github.com/argyle-labs/peacock)
plugin, which owns the root route via the render-at-path seam
(`projects/contract/src/web.rs`). The **generic** model layer — the
`ModelBackend` trait, registry, and `model.*` surface — is core
(`projects/model`); the **concrete** backends (Ollama, LM Studio, hosted
escalation) are plugins that implement it, exactly as storage adapters
implement the storage trait.

### Platform rule

Core holds **only abstractions** — traits, registries, ABIs, engines,
composition logic. Every concrete capability is an external plugin that
registers into a core registry. `projects/plugins/` has been removed — every
plugin is its own `argyle-labs` repo, no exceptions. (`agents` is the exception
to the *plugin* framing: it is a **core domain** at `projects/agents` whose
registration machinery is exposed through `plugin_toolkit`, so plugins register
agents against it like any other capability.) See
`docs/CAPABILITY-REGISTRIES.md`.

### Parity rule

**Until orca reaches parity with the existing homelab automation,
nothing else is in scope.** No hand-maintained host script,
per-host shell automation, or ad-hoc compose file is retired until orca
passes the four-check parity test (functional / side-effect /
failure-mode / operational) on every target host. Phase 2 (service
surface) and Phase 3 (deferred polish) cannot begin until Phase 1 closes.

---

### Being retired

Things still in the tree that are on their way out; docs describe the
target, not the vestige:

- **In-process cdylib plugins + `abi_stable` + `plugin-abi`** → removed in
  favor of subprocess-only plugins over `plugin-proto`. The `plugin-abi`
  crate and any `Backing::Cdylib` path are being deleted. See
  `docs/dynamic-linking.md`.
- **WASM browser client** → gone; the web UI is the `peacock` HTTP plugin.
- **Concrete model backends** (`projects/model/src/backend/{ollama,lmstudio,claude}.rs`)
  → extracted into plugins that register against the `ModelBackend` trait.
  The generic layer (trait, `build_backend()` factory → registry, `model.*`)
  **stays in core**.

---

## Phase 0 — Shipped

Grounded end-to-end in tree. The live tool surface spans domains
(`agent auth config db files host inventory model namespace network notify
pki plugin pod schedule schema secrets service spec storage system web`);
run `orca --help` for the build-current list.

| Capability | Location |
|---|---|
| One-binary CLI / REST / MCP tool surface via `#[orca_tool]` | `projects/{derive,dispatch,contract}` |
| **Managed Unit** — universal CRUD surface unifying VMs, containers, services, media behind one verb set | `projects/contract/src/unit.rs` + `projects/dispatch/src/unit_surface.rs` |
| Install / delete / self-update (`system.{install,delete,update,serve_release,build,kill}`); channels stable/rc/dev, mesh-relay, pinned versions | `projects/system/src/{install.rs,update.rs,commands.rs}` |
| In-process scheduler (cron tick, periodic primitive, runs table; `schedule.{list,run,status}`) | `projects/system/src/{scheduler.rs,periodic.rs}` |
| Host identity / status / system_info collectors (`host.info`, `system.detail`) | `projects/system/src/{host.rs,host_identity.rs,host_status.rs}` |
| Daemon (HTTP 12000 / HTTPS 12443 / mesh 12002, dual-bind, runtime log levels) | `projects/system/src/daemon.rs` |
| Config store (SQLite, history, schemas, owner-routing; `config.*`, `schema.*`) | `projects/db/src/config_store.rs` + `projects/db/migrations/` |
| Pod mesh: mTLS, mDNS discovery, peer pairing, dispatch, cert rotation (`pod.*`, 15 verbs) | `projects/pod` |
| Secrets store (encrypted SQLite; `secrets.*`) + auth (`auth.*`) + PKI (`pki.*`) | `projects/auth/src/{secrets.rs,pki.rs}` |
| Namespaces — per-user shareable workspaces (`namespace.*`) | `projects/namespace` |
| Files surface (`files.{list,read,update,delete,stat,search,tree}`) | `projects/files` |
| Notifications — unified typed event dispatcher (`notify.send`) | `projects/notifications` |
| Spec registry (OpenAPI + GraphQL, namespace-assignable; `spec.*`) + parsers | `projects/spec` + `projects/openapi` + `projects/graphql` |
| Inventory / topology view (`inventory.*`, `network.topology_view`) — drift-detecting, never applies | `projects/orca-inventory` + `projects/system/src/topology/` |
| Generic model layer — `ModelBackend` trait + `build_backend()` factory + `model.*` registry surface (concrete backends move to plugins) | `projects/model` |
| Container reconciler (Docker/LXC via CLI — **auto-start only**, not config reconcile) | `projects/containers/src/reconciler.rs` |
| Storage client (`storage.{list,shares,mount,unmount,recover}`) — NFS/SMB **client** side | `projects/system/src/storage_tools.rs` |
| Service backup/restore/deploy/status (`service.*`) | `projects/system/src/service_tools.rs` + `projects/service` |
| Interactive TUI / REPL + background agent jobs + session logs | `projects/conversation` |

### Plugin system — shipped

| Piece | Location |
|---|---|
| Out-of-process subprocess plugins (JSON frames over UDS, capability-delegated) | `projects/plugin-proto` + `projects/plugin-loader/src/{supervisor.rs,capability.rs}` |
| Capability delegation — plugins call back to daemon for `http.request` / `db.op` / `secret.op` instead of linking reqwest/rusqlite | `projects/plugin-loader/src/capability.rs` |
| Plugin authoring gateway (single dep: `plugin-toolkit`; `#[orca_tool]` + `serve`) | `projects/plugin-toolkit` + `projects/plugin-toolkit-build` + `projects/macro-runtime` |
| Manifest / MCP plugins (`orca-plugin.toml` → external MCP server; `plugin.*`) | `projects/runtime` (crate `plugins`) + `db::plugin_manifest` |
| First-party plugins as standalone repos (proxmox, docker, dockge, unraid, nfs, smb, plex, jellyfin, arr, ollama, lmstudio, mcp, ntfy, homeassistant, peacock) | `argyle-labs/*` |

> Thin-by-architecture is complete through Phase C (PRs #18–#45): plugins
> no longer link axum/tower/reqwest/rusqlite; tokio is in-process-only.
> Phase D (`schemars` → build-time consts) is **deferred** — see
> `docs/OUT-OF-PROCESS-PLUGINS.md`.

---

## Phase 1 — System-lifecycle parity (THE FOCUS)

Setup + update + maintenance of the host, its guests, and the
storage/network plumbing under it. No Phase 2 work until every item below
meets its exit criterion on every relevant host.

### 1.1 Proxmox guest reconciler — declarative LXC/VM  ·  **biggest gap**

**Have** — Proxmox plugin reads guest state and snapshots; topology
collector detects drift (`projects/system/src/topology/`). The container
reconciler (`projects/containers/src/reconciler.rs`) handles Docker/LXC
**auto-start only** — it does not parse or apply `pct.conf`.

**Missing** — `pct.conf`/`qemu-server.conf` parser+serializer, diff
engine, per-key strategy registry (`replace` / `preserve-runtime` /
`fail-on-drift`), `pct set`/`qm set` apply path, bind-source readiness
probe gating `pct start`, inner-service health gate, tmpfs scratch model,
restore-aware `vzrestore` wrapper, drift-detection periodic job, and the
`{reconcile,drift,restore}` actions on the `vm`/`lxc` unit nouns (alongside
the shipped `start`/`stop`), provided by the proxmox plugin.

**Exit** — `reconcile` is a no-op on every meerkat CT; `pct start` via
orca succeeds only when the inner service comes up healthy; zero diverged
keys for 7 consecutive days; `proxmox/lxcs/*.sh` retired under the parity
rule. **Driver:** the media-a 2026-06-01 restore — silent repo↔live
drift, manual `sed` to re-point binds, plex came up enabled-but-inactive.

### 1.2 Host update lifecycle — packages + drivers + reboot

**Have** — orca self-update (`projects/system/src/update.rs`); partial OS
package updates for `apt`/`apk`/`brew` (`commands.rs:879`).

**Missing** — package drivers for `dnf`/`pacman`/`pkg`/`opkg`; declarative
`updates.toml` (schedule / security-apply / hold / reboot-window); GPU +
accelerator driver lifecycle with **DKMS rebuild + verify-load** after
kernel upgrade (the load-bearing piece — silent failed rebuilds cause
ghost outages); Container Toolkit hook; reboot orchestration with ordered
pre-hook chain (drain caddy → stop docker → unmount nfs) and health-gated
rolling reboots. NVIDIA first.

### 1.3 Drift detection (fleet-wide)

**Have** — scheduler + periodic primitives (`scheduler.rs`, `periodic.rs`).

**Missing** — per-noun drift-checker registrations, drift-event schema +
retention, and drift-list verbs. (`storage_tools.rs:141` has a narrow
per-mount "drift set" only.)

### 1.4 Inner-service health probes

**Have** — `ServiceCapability::Status` / `ServiceStatus`
(`projects/service/src/lib.rs`); per-service OpenAPI plugins report state.

**Missing** — a generic `service.health(runtime=lxc:N|docker:X|host:Y)`
gate wired into the reconcile (1.1) and reboot (1.2) paths.

### 1.5 Storage — server side

**Have** — client side is shipped (§Phase 0 `storage.*`, nfs/smb plugins).

**Missing** — NFS export + SMB share reconcilers, Avahi/WSD advertise,
gateway-mode detection, runtime health + failover.

### 1.6 Backup — native-API-first + PBS

**Have** — `service.backup` / `service.restore` via `BackupArtifact`
(`service_tools.rs:151`); per-service native endpoints via OpenAPI.

**Missing** — PBS plugin, native-first per-service verbs on the canonical
surface, restore-drill harness, offsite destination.

### 1.7 Network reconciler

**Missing** — declarative DNS / firewall / DHCP / switch / AP config with
diff+apply. Greenfield.

### 1.8 Host lifecycle tools — doctor / uninstall / decommission

**Have** — `uninstall` is an in-process helper (`cmd_uninstall_report`),
`pair` is a CLI helper (`cmd_pod_pair`) — neither is a tool surface yet.

**Missing** — `system.doctor`, `system.uninstall` as first-class tools,
and a host-decommission flow (drain → deregister → wipe secrets/certs).

### 1.9 Agent surface + plugin-supplied agents and backends

**Target shape** — `orca` is the CLI surface (`orca <noun> <verb>`). The
agent surface is one command under it:

- `orca agents` — launch the interactive agent surface.
- `orca agents fox "…"` — run a specific registered agent (`fox`); error
  if no agent named `fox` is registered.
- `orca agents "…"` — hand the request to the top-level `orca` agent to
  route and execute.

**Blocks on** — plugin-contributed agents registering into the **core agents
domain** (`projects/agents`) via the `plugin_toolkit::agents::register` seam, and
**LLM backends as plugins** (the `projects/model` retirement above). Not properly
active until both land.

---

## Phase 2 — Service surface (blocked on Phase 1)

Per-service plugin build-out under `argyle-labs`, one repo per homelab
service, modeled on the shipped plugins. Each registers `service.*` and
its domain verbs against the core registries. Deferred until lifecycle
parity closes.

---

## Phase 3 — Deferred until parity

Frontend polish beyond "UI reflects server state", advanced PKI, native
push notifications, the `dev:<branch>` release channel (currently rejects
all releases), namespace consolidation, and install hardening. None start
before Phase 1 exit criteria are met fleet-wide.
