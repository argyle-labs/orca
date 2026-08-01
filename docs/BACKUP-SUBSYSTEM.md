# Backup Subsystem

Status: Living doc (tracks the as-built subsystem)
Related: [MINIMAL-BACKUP.md](./MINIMAL-BACKUP.md) (the originating RFC),
[CAPABILITY-REGISTRIES.md](./CAPABILITY-REGISTRIES.md),
[MANAGED-UNIT.md](./MANAGED-UNIT.md)

orca's backup subsystem is built on **two orthogonal axes**. Keeping them
separate is the whole design — do not collapse them:

1. **KIND — *what* to back up.** A `BackupProvider` (host config, a service, an
   Unraid flash drive, an LXC…). Registered in one provider registry.
2. **TARGET — *where* it is stored.** A `BackupTargetProvider` (local disk, NFS,
   SMB, S3, PBS, git…). Registered in a parallel target registry.

A single `backup` domain is parameterized by `--kind`; a backup run fans out over
every registered kind and writes to every configured target. **Core owns generic
machinery and exactly one concrete of each axis** — the `host`/`service` kinds and
the `local` file-path target. **Everything else is plugin-exposed**
(orca-core-generic, plugins-expose-functionality).

## The tool surface

| Tool | What it does |
|---|---|
| `backup.providers` | Every registered KIND and its instances. |
| `backup.targets` | Every registered TARGET, its placement fit, and the concrete locations it exposes for selection. |
| `backup.list` | Dated backups, newest first, aggregated across every configured target. |
| `backup.run` | Back up one `--kind`, or ALL kinds when omitted (`orca backup`). Fans out to every configured target; prunes per retention; then runs the collision check. |
| `backup.restore` | Date-selected restore (`--id`/`latest`), searching across targets, with a surface-safe selection gate. |
| `backup.check` | Fleet-wide same-folder collision detection (see below). |

## Axis 1 — KIND (what to back up)

A KIND implements `BackupProvider` (in `projects/system/src/backup/provider.rs`):

- `kind()` / `title()` / `instances()` — identity and the things it can back up.
- `backup(payload_dir, instance, ctx)` — write the state into the slot.
- `restore(payload_dir, instance, ctx)` — put it back.
- `layout(instance)` — the labeled path the backup is filed under (see
  [Taxonomy](#taxonomy)).

Core ships two kinds; the rest come from plugins:

| KIND | Owner | Captures |
|---|---|---|
| `host` | core | orca's own host config (`orca.toml`, PKI, profiles, memory). Excludes DB + logs (local history, not portable config). |
| `service` | core | Bridges every `ServiceBackend` (sonarr, audiobookshelf…) — one instance per backend, via the existing `BackupMethod` (tar/pbs). |
| **`unraid`** | **unraid plugin (planned)** | Unraid flash/USB config backup (the standard `.zip` of `/boot`), plus **appdata** captured over the Unraid API / plugins. See [Unraid](#worked-example-unraid). |
| `lxc` / `vm` | proxmox plugin (planned) | Guest definition + in-guest state, via PBS/tar. |
| `stack` | docker plugin (planned) | Compose/env + named-volume/config bind paths. |

Adding a KIND is: implement `BackupProvider`, `register_provider(...)` at plugin
load. It then appears on CLI/REST/MCP automatically and participates in
`orca backup` fan-out and retention for free.

## Axis 2 — TARGET (where it goes)

A TARGET implements `BackupTargetProvider` (in
`projects/system/src/backup/target.rs`):

- `open(name, ctx)` — resolve the named target to a filesystem-rooted
  `BackupStore` (creating the dir / ensuring the mount / cloning the repo).
- `sync` / `refresh` — post-write push / pre-read pull, for remote backings
  (git push/pull, S3 up/download). No-ops for a plain local path.
- `available(ctx)` — enumerate the concrete locations this kind exposes, so the
  create flow can let a user **point a target at a root** (see below).
- `backing_key(name, ctx)` — a **globally stable** identity of the underlying
  storage, used for fleet-wide collision detection.
- `fits(placement)` — whether to OFFER this target for a workload (e.g. PBS only
  on Proxmox). Never gates an explicit choice.

**Core owns exactly one target: `local`.** Every other backing is a plugin:

| TARGET | Owner | `backing_key` | Notes |
|---|---|---|---|
| `local` | core | `local://<host>` | A path on this host's disk. Per-host key → two hosts' local paths never collide. maple and willow (NAS's with their own disks/paths) are `local` targets **on those hosts**. |
| `nfs` | nfs/storage plugin (planned) | `nfs://server/export` | An NFS export mounted and written to. Shared → cross-host collisions are possible and detected. |
| `smb` | smb/storage plugin (planned) | `smb://server/share` | An SMB/CIFS share. The plugin creates the mount, then exposes it via `available()`. |
| `s3` | s3 plugin (planned) | `s3://bucket[/prefix]` | Remote object storage; `sync`/`refresh` do the up/download. |
| `pbs` | proxmox plugin (planned) | `pbs://repo` | Proxmox Backup Server. Offered only where `fits()` sees Proxmox. |
| `git` | core or plugin (planned) | `git://<remote>` | Off-host, versioned. Powers the "back up host config to a repo" case. |

### Pointing a target, and the default sub-path

**You point a target at a ROOT only.** A storage plugin (smb/nfs) creates its
mount separately, then exposes it through `available()` as a `TargetLocation`
(id + label + base path). The create flow lists these; you pick the location
(e.g. the SMB `//nas/backups` mount) and, optionally, a sub-path within it. The
selection is saved to config as a `BackupTargetRef { kind, name }` plus the
plugin's own typed per-target config row.

**The sub-path defaults to the taxonomy** — you do not type
`hosts/proxmox/thor`; the provider produces it. Overriding the sub-path is
optional.

## Taxonomy

Every backup is filed under a provider-declared, labeled layout so backups
self-organize into a navigable tree that is **identical on every backing**:

```
<target-root>/<category>/<class>/<name>/<id>/
    manifest.json    # typed BackupRecord (identity + metadata)
    payload/…        # the backup itself
```

| Provider | Layout | Example |
|---|---|---|
| host (Proxmox node) | `hosts/<placement>/<hostname>` | `hosts/proxmox/thor` |
| host (bare) | `hosts/bare/<hostname>` | `hosts/bare/maple` |
| service (container) | `containers/<runtime>/<name>` | `containers/docker/sonarr` |
| service (VM) | `vms/vm/<name>` | `vms/vm/homeassistant` |
| unraid (planned) | `hosts/unraid/<hostname>` (flash) · `containers/docker/<name>` (appdata) | `hosts/unraid/tower` |

The store is **layout-agnostic**: it treats `<category>/<class>/<name>` as an
opaque path and filters `list`/`resolve` by the manifest's identity
(`kind`+`instance`), not by directory names. Segments are sanitized so a provider
can never escape the target root.

## Fleet-wide collision detection

Two backups **conflict** when they are written to the same folder on the same
underlying storage — **even from different machines** (e.g. two hosts both
pointed at the same NFS export both writing `…/hosts/proxmox/thor`).

Detection is fleet-wide:

- Each node self-reports its resolved destinations
  (`{kind, instance, backing_key, subpath}`) into a `backup/destinations` config
  row, which **replicates across the mesh**.
- `backup.check` (also run at the end of `backup.run`) unions every node's
  destinations and flags any two that share a `backing_key` with **same-or-
  overlapping** sub-paths.
- Collisions are keyed on **(backing identity, sub-path)**, not the local
  mountpoint string — so per-host local disks (`local://<host>`) never false-
  positive, while a shared `nfs://server/export` correctly does.
- Each collision raises a **dismissable notification** with a suggested fix.
  Non-blocking — a warning to correct, not a hard failure.

The default taxonomy already namespaces host backups by hostname, so ordinary
per-host backups don't collide; collisions arise from overrides, shared service
names, or same-path misconfiguration.

## Worked example: Unraid

Unraid is not yet implemented; the pathway is:

1. **KIND** — the Unraid plugin registers an `unraid` `BackupProvider`:
   - **flash/USB config** — capture the standard `/boot` config `.zip` (the
     Unraid flash backup) as one instance, filed under `hosts/unraid/<hostname>`.
   - **appdata** — enumerate containers via the Unraid API / plugins and capture
     each container's appdata as `containers/docker/<name>` instances.
2. **TARGET** — point it wherever fits:
   - a **`local`** target on the Unraid box itself (its own array/cache disk);
   - an **`nfs`** / **`smb`** mount to maple or willow;
   - an **`s3`** target for off-site copies.
   Because targets are a LIST, you can send the same backup to several at once
   (e.g. local array **and** S3).
3. It participates in retention, the taxonomy, and fleet-wide collision checks
   with no extra work — those are core, kind-agnostic, and target-agnostic.

## Ownership summary

- **Core:** the two registries, the location-agnostic `BackupStore`, the
  taxonomy seam (`layout()`), retention, the `backup.*` tools, fleet-wide
  collision detection, and the built-in `host`/`service` kinds + `local` target.
- **Plugins:** concrete KINDs (unraid, lxc/vm, stack) and concrete TARGETs (nfs,
  smb, s3, pbs, git). Plugins stay thin; heavy backup deps live behind core
  machinery reached via the runtime.

## Status

Built (PRs #208 / #210 / #211): the two registries, taxonomy, retention,
`local` target, `host`/`service` kinds, target selection seam, fleet-wide
collision detection, the full `backup.*` tool surface.

Planned: the `unraid` kind; `nfs` / `smb` / `s3` / `pbs` / `git` target plugins;
explicit per-`(kind,instance)` sub-path override; scheduled backups.
