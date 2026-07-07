# Orca Plugin Build-Out Program

> **Architecture:** see [CAPABILITY-REGISTRIES.md](CAPABILITY-REGISTRIES.md) —
> the repo-wide platform model (core = abstractions/registries only; every
> concrete capability is an external plugin; one plugin registers against many
> capabilities). This build-out roster is one instance of that pattern.

Goal: **one orca plugin repo per homelab service** under
[`argyle-labs`](https://github.com/argyle-labs), each a Rust `cdylib` modeled on
the [jellyfin](https://github.com/argyle-labs/jellyfin) plugin. This doc is the
canonical roster + capability contract for the build-out. See
[PLUGINS.md](../PLUGINS.md) for the plugin-system mechanics.

## Minimal API — one generic `service.*` surface

**Severely limited surface area (decided).** Plugins expose **zero**
`#[orca_tool]`s. The entire fleet shares ONE generic surface that lives once in
`plugin-toolkit`; the **service is a parameter**. Each plugin only registers a
`ServiceBackend` impl — exactly how `nfs`/`smb` register a `StorageBackend` via
`export_storage_plugin!`. 23 plugins add **0** tools.

```
service.list                                   # known services + capabilities
service.deploy    {service, instance, runtime, host, …}  # compose deploy_target
service.backup    {service, instance, …}
service.restore   {service, instance, from}
service.configure {service, instance, config}
service.status    {service, instance, …}
```
(`connect`/`sync` — persistent endpoint registry + peer sync — land next,
reusing the existing replicated-endpoint infra rather than a parallel store.)

### `service` ⟂ `deploy_target` — no duplicated responsibility
A **service** is *software*; a **deploy_target** is a *place to run software*
`(host, runtime, kind)`. They never overlap:
- `service.deploy` asks the backend for a runtime-agnostic **`WorkloadSpec`**
  (`ServiceBackend::workload_spec`) and hands it to a matching registered
  **deploy target's `launch`**. The service domain drives **no** `pct`/`docker`
  itself — placement mechanics live once, in `deploy-target`.
- The service domain **reuses `deploy_target::Runtime`** (`docker|podman|lxc|vm`)
  rather than defining a parallel `Modality` enum. `device`/`host`-only services
  (mikrotik, a host UPS daemon) advertise **no** runtimes and drop the `Deploy`
  capability.
- What service owns that deploy_target can't: **backup / restore / configure /
  status** + producing the spec — all need service-specific knowledge.

### Plugin shape — pure Rust, zero bash
A plugin is **only** `Cargo.toml` + `.cargo/config.toml` + `LICENSE` +
`src/{lib,abi_export}.rs` + `README` + `CAPABILITIES.md`. **No shell scripts, no
`compose.yml`, no `Dockerfile`, no `lxc/`/`vm/` provision scripts.** The consumer
never touches bash. Everything mechanical is generic:
- **deploy / install / entrypoint** → `deploy_target` renders the `WorkloadSpec`
  into compose / LXC / VM. (A custom container image is the rare exception a
  plugin adds a `Dockerfile` for by hand.)
- **backup / restore** → the service domain's pluggable `BackupMethod`.
- **configure / status** → Rust methods on the backend.

So the *only* per-plugin work is declarative: `provider` / `runtimes` /
`default_port` / `capabilities` / `data_paths`, plus `workload_spec`, and
optionally `configure` / `status`.

### Pluggable backup methods (`service.backup, that's it`)
Backup is runtime-agnostic AND tool-agnostic. A `BackupMethod` registry
(`plugin_toolkit::service`) ships two built-ins and accepts more from plugins:
- **`tar`** — file snapshot of `data_paths` inside the running instance
  (`docker/podman exec` + `cp`, or `pct exec` + `pull`).
- **`pbs`** — Proxmox Backup Server: a Proxmox **LXC/VM** is backed up natively
  with `vzdump --storage <pbs>`; a container/host filesystem via
  `proxmox-backup-client`.

Selection is automatic: an explicit `endpoint.backup_method` wins; otherwise a
**Proxmox LXC/VM with PBS available routes to `pbs`**, else `tar`. Plugins
register restic/borg/etc. via `service::register_method`.

### `ServiceBackend` trait (lives in `projects/service`, re-exported as `plugin_toolkit::service`)
Declarative: `provider`, `runtimes`, `default_port`, `capabilities`, `endpoint`,
`data_paths`, `descriptor`. Behavioral: `workload_spec(runtime)`, `configure`,
`status` — plus `backup`/`restore`, which **default to the generic pluggable
implementation** (a plugin overrides them only for non-filesystem backup, e.g. a
DB dump). Async methods are hand-desugared to `BoxFuture` (object-safe, no
`async_trait` macro — see [[no-async-trait-macro]]).

Export macro: `export_service_plugin! { name, target_compat, backend: XBackend::new("x") }`
— mirrors `export_storage_plugin!`; emits the cdylib root module, derives the
`BackendDef` from the backend's own `descriptor()`, routes `invoke()` through
`service::dispatch_op`. The loader's `domain_register` table gains a `"service"`
arm. **Status: implemented + compiles green** (audiobookshelf + mikrotik verified).

### Modality rule

Every service that can run in one modality must support **all logical
modalities** — if it can be an LXC it must also deploy as a Docker/podman
container and (where sensible) a VM. Do not skip a modality because it isn't
done that way today. The `Runtime` enum in `lifecycle.rs` is therefore
`{ Docker, Podman, Lxc, Vm }` (subset per plugin where a modality is genuinely
impossible — e.g. `mikrotik` is device-API-only, `opnsense` is VM-only).

## Roster (23 new repos)

### Media & content (8)
| Plugin | Modalities | Notes |
|---|---|---|
| `audiobookshelf` | docker/podman/lxc/vm | audiobook + podcast server |
| `calibre-web` | docker/podman/lxc/vm | ebook web reader |
| `kavita` | docker/podman/lxc/vm | manga/ebook server |
| `komga` | docker/podman/lxc/vm | comics/manga server |
| `navidrome` | docker/podman/lxc/vm | music streaming (Subsonic API) |
| `immich` | docker/podman/lxc/vm | **multi-service** (server + ML + postgres + redis) |
| `libation` | docker/podman/lxc | Audible library downloader |
| `uptime-kuma` | docker/podman/lxc/vm | uptime monitoring |

### Home & IoT (3)
| Plugin | Modalities | Notes |
|---|---|---|
| `mqtt` | docker/podman/lxc/vm | Mosquitto broker |
| `zigbee2mqtt` | docker/podman/lxc | Zigbee bridge (USB passthrough) |
| `zwave-js-ui` | docker/podman/lxc | Z-Wave bridge (USB passthrough) |

### Network & infra (7)
| Plugin | Modalities | Notes |
|---|---|---|
| `opnsense` | **vm** | firewall/router appliance — plugin sets up + configures OPNsense |
| `adguard` | docker/podman/lxc/vm | DNS sinkhole |
| `unifi` | docker/podman/lxc/vm | UniFi network controller |
| `mikrotik` | **device API** | RouterOS — no deploy, manage existing device |
| `pbs` | vm/lxc | Proxmox Backup Server |
| `nut` | host/docker/lxc | UPS monitoring (NUT + apcupsd) |
| `caddy` | docker/podman/lxc/vm | reverse proxy |

### Media acquisition (2)
| Plugin | Modalities | Notes |
|---|---|---|
| `qbittorrent` | docker/podman/lxc/vm | torrent client (same org) |
| `sabnzbd` | docker/podman/lxc/vm | usenet client (same org) |

### Storage HA & local LLM (3)
| Plugin | Modalities | Notes |
|---|---|---|
| `nfs-gateway` | vm/lxc/container | HA NFS gateway w/ failover (was `nas-a`); orca handles failover per system |
| `ollama` | docker/podman/lxc/vm | local LLM runner |
| `lmstudio` | **host** | LM Studio local LLM runner (OpenAI-compatible server on :1234) — desktop/host app, connect+configure an existing install (no Deploy) |

### NOT new repos
- `media-b`, `media-a` → Plex LXCs, covered by existing `plex` plugin.
- `llm` → there is no `llm` plugin; the model registry + engine + provider backends live in core `projects/model`. Local runners are per-runner service plugins: `ollama` (containerized) and `lmstudio` (host desktop app). Do not build a generic llm runner.

## Cross-cutting capabilities (core / plugin-toolkit, not new repos)
1. **Config sync between all systems** — any plugin's config replicates across paired peers.
2. **NFS/SMB setup on any orca host** — `nfs`/`smb` graduate from backends to deployers (`nfs.setup_host`, `smb.setup_host`).
3. **Universal backup/restore/configure** — the contract above, uniform across every deployed service.

## End state
All plugins released → registered in local orca → any service / secret / config
can be added to a machine via **cli / api / mcp**.

## Status
See `CAPABILITIES.md` in each repo for the per-plugin contract checklist.
Scaffolds generated by `orca/scripts/scaffold-plugin.sh`.
