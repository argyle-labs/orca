# Backup Subsystem

Status: Living doc (tracks the as-built subsystem)
Related: [MINIMAL-BACKUP.md](./MINIMAL-BACKUP.md) (the originating RFC),
[CAPABILITY-REGISTRIES.md](./CAPABILITY-REGISTRIES.md),
[MANAGED-UNIT.md](./MANAGED-UNIT.md)

orca's backup subsystem is built on **two orthogonal axes**. Keeping them
separate is the whole design — do not collapse them:

1. **KIND — *what* to back up.** A `BackupProvider`. Registered in one provider
   registry.
2. **TARGET — *where* it is stored.** A `BackupTargetProvider`. Registered in a
   parallel target registry.

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
| `backup.run` | Back up one `--kind`, or every kind with `--all` (opt-in — neither refuses and lists the kinds). `orca backup` on the CLI prompts for the choice. Fans out to every configured target; prunes per (per-target) retention; then runs the collision check. |
| `backup.restore` | Date-selected restore (`--id`/`latest`), searching across targets, with a surface-safe selection gate. |
| `backup.check` | Fleet-wide same-folder collision detection (see below). |

## Axis 1 — KIND (what to back up)

A KIND implements `BackupProvider` (in [`projects/system/src/backup/provider.rs`](../projects/system/src/backup/provider.rs)):

- `kind()` / `title()` / `instances()` — identity and the things it can back up.
- `backup(payload_dir, instance, ctx)` — write the state into the slot.
- `restore(payload_dir, instance, ctx)` — put it back.
- `layout(instance)` — the labeled path the backup is filed under (see
  [Taxonomy](#taxonomy)).

Core ships two kinds; plugins register more via the same seam:

| KIND | Captures |
|---|---|
| `host` | The host's orca config/state under the state dir (`orca.toml`, PKI, profiles, memory). |
| `service` | Bridges every `ServiceBackend` — one instance per backend, via the existing `BackupMethod`. |

Adding a KIND is: implement `BackupProvider`, `register_provider(...)` at plugin
load. It then appears on CLI/REST/MCP automatically and participates in
`orca backup` fan-out and retention. A plugin's own docs describe the kinds it
registers.

## Axis 2 — TARGET (where it goes)

A TARGET implements `BackupTargetProvider` (in
[`projects/system/src/backup/target.rs`](../projects/system/src/backup/target.rs)):

- `open(name, ctx)` — resolve the named target to a filesystem-rooted
  `BackupStore` (creating the dir / ensuring the mount / cloning the repo).
- `sync` / `refresh` — post-write push / pre-read pull for a remote backing. A
  plain local path implements them as no-ops.
- `available(ctx)` — enumerate the concrete locations this kind exposes, so the
  create flow can let a user **point a target at a root** (see below).
- `backing_key(name, ctx)` — a **globally stable** identity of the underlying
  storage, used for fleet-wide collision detection.
- `fits(placement)` — whether to OFFER this target for a workload. Never gates an
  explicit choice.

**Core owns exactly one target: `local`.** Plugins register more via the same
seam; a shared backing exposes a globally stable `backing_key` (e.g.
`scheme://server/export`), which fleet-wide collision detection keys on. A
plugin's own docs describe the targets it registers.

| TARGET | `backing_key` | Notes |
|---|---|---|
| `local` | `local://<host>` | A path on this host's disk. Per-host key → two hosts' local paths are independent. A NAS with its own disks/paths is a `local` target **on that host**. |

### Pointing a target, and the default sub-path

**You point a target at a ROOT only.** A storage plugin creates its mount
separately, then exposes it through `available()` as a `TargetLocation` (id +
label + base path). The create flow lists these; you pick the location (e.g. a
`//nas/backups` mount) and, optionally, a sub-path within it. The selection is
saved to config as a `BackupTargetRef { kind, name }` plus the plugin's own typed
per-target config row.

**The sub-path defaults to the taxonomy** — you do not type `hosts/thor`; the
provider produces it. Overriding the sub-path is optional.

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
| host (no placement label) | `hosts/<hostname>` | `hosts/thor` |
| host (with placement label) | `hosts/<label>/<hostname>` | `hosts/<label>/thor` |
| service | `services/<name>` | `services/sonarr` |

The placement label is sourced from the `backup/placement` config row; a plugin
that knows a host's role sets it, and core names none. A plugin registering its
own KIND supplies its own `layout()`, filed under the same `<category>/…` tree.

The store is **layout-agnostic**: it treats `<category>/<class>/<name>` as an
opaque path and filters `list`/`resolve` by the manifest's identity
(`kind`+`instance`), not by directory names. Segments are sanitized so a provider
can never escape the target root.

## Fleet-wide collision detection

Two backups **conflict** when they are written to the same folder on the same
underlying storage — **even from different machines** (e.g. two hosts both
pointed at the same shared export both writing `…/hosts/thor`).

Detection is fleet-wide:

- Each node self-reports its resolved destinations
  (`{kind, instance, backing_key, subpath}`) into a `backup/destinations` config
  row, which **replicates across the mesh**.
- `backup.check` (also run at the end of `backup.run`) unions every node's
  destinations and flags any two that share a `backing_key` with **same-or-
  overlapping** sub-paths.
- Collisions are keyed on **(backing identity, sub-path)**, not the local
  mountpoint string — so per-host local disks (`local://<host>`) never false-
  positive, while a shared `scheme://server/export` correctly does.
- Each collision raises a **dismissable notification** with a suggested fix.
  Non-blocking — a warning to correct, not a hard failure.

The default taxonomy namespaces host backups by hostname, so ordinary per-host
backups stay distinct; collisions arise from overrides, shared service names, or
same-path misconfiguration.

## Adding a KIND or TARGET (plugin author)

1. Implement `BackupProvider` (KIND) or `BackupTargetProvider` (TARGET) and call
   `register_provider(...)` / `register_target(...)` at plugin load.
2. A KIND supplies `layout()` to file its backups under the `<category>/…` tree;
   a shared TARGET supplies a globally stable `backing_key()`.
3. Fan-out, retention, the taxonomy, and fleet-wide collision checks then apply
   to it through the core machinery.

The plugin's own repo documents the concrete kinds and targets it registers.

## Ownership summary

- **Core (this repo):** the two registries, the location-agnostic `BackupStore`,
  the taxonomy seam (`layout()`), per-target retention, the `backup.*` tools,
  fleet-wide collision detection, and the built-in `host`/`service` kinds +
  `local` target.
- **Plugins (their own repos):** additional KINDs and TARGETs registered through
  the seams. Plugins stay thin; heavy backup deps live behind core machinery
  reached via the runtime.

## Status

Built (PRs #208 / #210 / #211 / #212): the two registries, taxonomy, per-target
retention, `local` target, `host`/`service` kinds, target selection seam,
fleet-wide collision detection, the full `backup.*` tool surface.

Planned (this repo): explicit per-`(kind,instance)` sub-path override; mount
references; scheduled backups.
