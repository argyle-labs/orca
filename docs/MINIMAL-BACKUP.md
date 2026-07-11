# RFC: Universal Minimal Backup + Update-with-Backup Standard

Status: Draft
Related: [MANAGED-UNIT.md](./MANAGED-UNIT.md), [CAPABILITY-REGISTRIES.md](./CAPABILITY-REGISTRIES.md)

## 1. Motivation

Two failure modes, observed on real managed guests, motivate this work:

1. **Wrong backup unit.** A community-scripts-style in-guest updater refused to run
   until a container was re-provisioned, warning *"may cause data loss."* The safe
   answer is "back up first" — but the only backup available was a **full-disk image**
   (`vzdump`). One service VM presents a 140 GB disk of which only ~30 GB is used and
   most of that is disposable (download cache, re-pullable container images, the OS).
   Its irreplaceable state — the application config DBs — is a few hundred MB. Imaging
   the whole disk to protect a few hundred MB of state is wasteful and slow, and it
   discourages taking the backup at all.

2. **Inconsistent, gappy per-host scripts.** Two service hosts each grew a bespoke
   `backup-appdata`-style script. One enumerated *all* services; the other hard-coded
   two service names, one of which no longer existed — so it silently backed up nothing
   useful while eight real services went uncovered. Per-host scripts drift and rot.

The lesson: backups must be **minimal** (state, not bulk), **capability-aware** (use the
right mechanism per unit), and **centrally owned by orca** (declared in the type system,
not scattered across hand-maintained shell scripts).

## 2. Principles

- **Minimal = state, not bulk.** Back up only the irreplaceable: app configs + DBs,
  compose/stack definitions, VM/CT definitions, host-specific config. Never media
  libraries (on network storage), download caches, container images (re-pullable),
  or the OS (reproducible).
- **Capability-aware.** A tiny container whose rootfs *is* its state is fine to back up
  with a rootfs snapshot. A service host with a separate bulk data disk must back up
  only its config paths. Each unit declares which it is.
- **Central, typed, single source of truth.** Every unit declares its minimal backup set
  in the type system. orca enumerates and backs up uniformly, with matching restore.
- **Backup is a prerequisite of mutation, not an afterthought.** A mutating action
  (update / configure / migrate / destructive re-provision) offers to back up first and
  **aborts if the backup fails.**

## 3. What already exists (reuse, don't reinvent)

Grounded in the current tree:

| Capability | Location | Shape |
|---|---|---|
| Location-agnostic backup strategy | `projects/service/src/lib.rs` — `BackupMethod` trait + registry | pluggable `tar` / `pbs`; `select_method()` auto-picks |
| "What state to back up" | `projects/service/src/lib.rs` — `ServiceBackend::data_paths()` | a backend already declares its state paths |
| Backup artifact | `projects/service/src/lib.rs` — `BackupArtifact` | `{service, instance, path, timestamp, size, checksum}` |
| Backup/restore tools | `projects/system/src/service_tools.rs` — `service.backup` / `service.restore` | standalone tools over `EndpointArgs` |
| Five-verb managed-unit surface | `projects/contract/src/unit.rs` — `Verb`, `UpdateArgs`, `CreateArgs`, `ActionDecl`, `UnitProvider`, `dispatch()` | action-discriminated, typed payloads/responses |
| Proxmox guests as units | `proxmox/src/unit_provider.rs` — `ProxmoxUnitProvider` | registers `vm`/`lxc`; Update actions `start/stop/shutdown/reboot`; Create `provision` |
| Docker stacks as units | `docker/src/unit_provider.rs` — `DockerUnitProvider` | registers `stack`; Update actions `edit`/`up`/`down`/… |

**`data_paths()` is already "minimal backup."** This RFC generalizes it from the service
domain to every managed unit, folds backup/restore into the unit verb surface, and adds
the pre-mutation guard.

## 4. Design

### 4.1 `BackupSpec` — the minimal state declaration (new, `contract`)

Every unit kind declares how to produce its minimal backup:

```rust
/// Declares the minimal, restore-sufficient state of a unit.
pub struct BackupSpec {
    /// Paths (in the unit's own filesystem namespace) that constitute state.
    pub include: Vec<String>,
    /// Sub-paths under `include` to exclude (caches, thumbnails, sockets).
    pub exclude: Vec<String>,
    /// How the state is captured for this unit.
    pub strategy: BackupStrategy,
}

pub enum BackupStrategy {
    /// Archive declared paths (service hosts, docker stacks). The minimal default.
    Paths,
    /// Snapshot the whole rootfs — correct only when rootfs IS the state and is
    /// small (tiny CTs); bulk data must live on a separate, excluded mount.
    Rootfs,
    /// Unit definition only (cores/mem/net/disk layout) — pairs with Paths for VMs.
    Definition,
}
```

A provider MAY compose strategies (e.g. a VM returns `Definition` + `Paths`). Reuses
`BackupMethod` for the actual write (tar/pbs), so location-agnosticism is unchanged.

### 4.2 Backup / restore as unit actions (extend `contract::unit` + plugins)

Add to the managed-unit vocabulary, surfaced automatically by the toolkit:

- `Create { action: "backup" }` → produces a `BackupArtifact` from the unit's `BackupSpec`.
- `Update { action: "restore", payload: RestorePayload { from: BackupArtifact } }`.

Providers implement them by delegating to the existing `BackupMethod`; `service.backup`/
`service.restore` become thin wrappers over the unit surface (no duplicate logic).

### 4.3 Update-with-backup guard (new interceptor, `contract::unit::dispatch`)

A dispatch interceptor wraps **mutating** actions (`update` family, `configure`, `migrate`,
destructive `provision`). Before the mutation:

1. Resolve the guard **policy** for the unit (§4.5).
2. If policy calls for it, invoke the unit's `backup` action.
3. **If the backup fails, abort the mutation** and return the backup error.
4. Otherwise proceed, annotating the outcome with the `BackupArtifact` produced.

This is the centralized, gap-free replacement for per-host "backup before update" shell
wrappers. Read-only actions (`list`, `detail`, `status`) and lifecycle no-data actions
(`start`, `stop`) are never guarded.

### 4.4 Config-standard guards (new `UnitGuard`, `contract`)

Declared, per-kind invariants validated on `provision`/`update`:

```rust
pub struct UnitGuard {
    pub kind: String,                 // "lxc" | "vm" | "stack" | ...
    pub min_cpu: Option<u32>,
    pub min_mem_mb: Option<u64>,
    pub require_root_console: bool,    // serial getty / pct-enter reachable
    pub require_update_command: bool,
    // extensible
}
```

On a guarded action, orca checks the target against its kind's `UnitGuard`. On violation
it **does not** emit a scary "may cause data loss" prompt — it takes the minimal backup
(per §4.3) and then either auto-remediates (e.g. raise memory to the minimum) or refuses
with a precise, typed reason. This is the orca-owned version of the under-provisioning
gate that motivated the work, and the home for the "vm/lxc guards" in the proxmox plugin.

### 4.5 Guard policy

Default: **prompt, default-yes** — interactive callers are asked "back up before this
change? [Y/n]" (default Yes); non-interactive callers back up automatically. Policy is
resolved per unit with a global default, leaving room for `always` / `never` overrides
later without changing the interceptor.

## 5. Ownership (core defines *what*, plugins define *how*)

- **Core (`contract` + `service`):** `BackupSpec`/`BackupStrategy`, `UnitGuard`, the
  backup/restore actions in the verb vocabulary, the guard interceptor, the policy type.
- **Plugins (concrete):**
  - **proxmox:** `lxc`/`vm` `BackupSpec` (Definition + optional in-guest Paths; rootfs
    only for tiny CTs), `UnitGuard` minimums per kind, backup/restore via PBS/tar.
  - **docker:** `stack` `BackupSpec` = compose/env + named-volume/`/config` bind paths,
    excluding image layers and bulk mounts.
  - **service backends:** already have `data_paths()`; adopt `BackupSpec` directly.

No `async_trait` (hand-desugar to `BoxFuture`). Plugins stay thin — heavy backup deps
(archivers, PBS client) live behind the core `BackupMethod`, reached via the runtime.

## 6. Migration from hand scripts

The per-host `*-mgmt.sh backup` / `backup-appdata.sh` scripts and the in-guest
`update` wrappers (prompt → backup → abort-on-fail) are the **interim** implementation of
this design. Once the capability lands:

- Each host's minimal path set becomes its unit's `BackupSpec.include`.
- The in-guest `update` prompt is superseded by the §4.3 interceptor.
- The scoped guest→host backup SSH forced-command (interim trigger) is replaced by the
  unit `backup` action dispatched over the mesh.

Hand scripts are removed only after the orca path is proven per host.

## 7. Increment plan (each its own PR)

1. **Core:** `BackupSpec`/`BackupStrategy`, backup/restore unit actions, guard interceptor,
   policy type. Wire `service.backup/restore` as thin wrappers. (Pieces 1–3.)
2. **Proxmox:** `BackupSpec` + `UnitGuard` for `vm`/`lxc`; backup/restore actions;
   provisioning + root-console guards. (Piece 4, highest fleet value.)
3. **Docker + service backends:** `stack`/service `BackupSpec`; adopt across plugins.
4. **Migration:** replace per-host scripts host-by-host; remove interim wrappers.

## 8. Open questions

- **Large-but-state data** (e.g. a photo app's library that IS irreplaceable state but
  large): `Paths` strategy is correct but tar is heavy — do we require an incremental
  method (borg/restic/pbs-file-level) for `Paths` above a size threshold?
- **Restore granularity:** per-service restore (a `*-mgmt.sh restore <app>` equivalent) vs
  whole-unit — expose both via `RestorePayload`?
- **Policy storage:** per-unit policy lives in config rows vs unit metadata?
