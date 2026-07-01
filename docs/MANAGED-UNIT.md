# Managed Unit — the universal lifecycle surface

> Status: **design proposal** (2026-07-01). No code yet. Companion to
> [`CAPABILITY-REGISTRIES.md`](./CAPABILITY-REGISTRIES.md), which this generalizes.

## Why

orca exists to **abstract away the differences between all our systems**. A VM, an
LXC, a Docker container, a Podman container, a service, and the managers
themselves (Proxmox, Unraid) are all things you `start`, `stop`, `restart`,
`update`, `back up`, `restore`, and check the `status` of. Today each of those
lives behind a different trait with different verb names. That multiplies API
surface and forces the host to know *which kind of thing* it's talking to.

**Goal:** one canonical set of primitives that works against *any* managed thing,
so adding a new system type is "map its native verbs onto the canonical ones,"
not "invent a new domain." Fewer verbs, more systems.

## What the enumeration found

Four existing backend domains already overlap almost entirely:

| canonical verb | `deploy_target` | `container_runtime` | `service` | `storage` |
|---|---|---|---|---|
| `status`  | (outcome)  | `inspect`/`observe`/`probe_liveness` | `status`  | `usage` |
| `start`   | `launch`   | `start`   | (deploy)  | — |
| `stop`    | `stop`     | `stop`    | —         | — |
| `restart` | `restart`  | `restart` | —         | — |
| `update`  | —          | —         | —         | — |
| `backup`  | `snapshot` | —         | `backup`  | — |
| `restore` | —          | —         | `restore` | — |
| `configure`| —         | —         | `configure`| — |
| `logs`    | `logs`     | `logs`    | —         | — |
| `exec`    | `shell`    | `exec`    | —         | — |
| `recover` | —          | `attempt_unwedge` | — | `recover_stale` |
| `migrate` | `migrate`  | —         | —         | — |

`deploy_target` is already ~80% of the target shape: it carries a composite
identity `(host, runtime, kind)`, capability gating, and launch/stop/restart/
logs/shell/metrics/snapshot/migrate. The managed-unit surface **generalizes
`deploy_target`** and folds the others in.

## Concepts

### Unit identity

A **unit** is any individually-addressable managed thing. Identity is four axes:

```
UnitId { manager, kind, id, name }
```

- `manager` — who exposes it (e.g. `proxmox@cluster-a`, `docker@host-b`, `local`).
  The manager is itself a unit (recursion, see below).
- `kind` — free string, never a fixed core enum: `vm`, `lxc`, `docker`, `podman`,
  `service`, `host`, … Core never branches on it; plugins do.
- `id` — the manager-native identifier (vmid, container id, service slug).
- `name` — human label.

### Canonical verbs (synonyms fold at the plugin)

```
enum Verb { Status, Start, Stop, Restart, Update, Backup, Restore,
            Configure, Logs, Exec, Recover, Migrate }
```

The host only ever issues a canonical verb. Each system maps its native term:
`start = startup = power_on = launch`; `restart = reboot`; `stop = shutdown =
power_off`; `backup = snapshot`. The plugin owns the translation to its API.

Every verb is **capability-gated**: a unit advertises exactly the verbs it
supports, so a read-only unit exposes `{Status, Logs}` and nothing else. This is
how partial adopters stay cheap (see "Adoption").

### Provider enumerates many units

A plugin does not register "one adapter per kind." It registers a **provider**
that enumerates *many* units of *possibly many kinds*:

```rust
// contract-level trait; async desugared to BoxFuture (no async_trait macro).
pub trait UnitProvider: Send + Sync {
    fn name(&self) -> &str;

    /// Enumerate every unit this provider currently exposes, each with its
    /// kind, status, advertised verbs, and optional parent (for nesting).
    fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>, UnitError>>;

    /// Perform a canonical verb against one unit. Args/outcome are per-verb
    /// typed payloads (no opaque JSON); `Unsupported` if the unit didn't
    /// advertise the verb.
    fn invoke(&self, unit: &UnitId, verb: Verb, args: VerbArgs)
        -> BoxFuture<'_, Result<VerbOutcome, UnitError>>;
}

pub struct UnitDescriptor {
    pub id: UnitId,
    pub kind: String,
    pub status: UnitStatus,          // running/stopped/degraded/unknown …
    pub verbs: Vec<Verb>,            // capability gate
    pub parent: Option<UnitId>,      // nesting: this LXC's parent is its PVE host
}
```

Proxmox registers **one** `UnitProvider` that returns many `vm` units + many
`lxc` units across all its endpoints — resolving the earlier "per-endpoint
adapter vs one-per-kind registry" problem: each guest is just a unit, keyed by
full `UnitId`, no per-kind collision.

### Managers are units too (recursion)

`parent` lets a unit both **be** managed and **manage** others. Proxmox itself is
a `host` unit (you can `Restart`/`Update` it) *and* a `UnitProvider` enumerating
its vm/lxc units, each with `parent = <the proxmox host unit>`. Unraid the same.
The host renders/acts on a tree, uniformly.

## Registration — reuse the existing seam

No new machinery. Managed units register through the **same** `BackendDef` +
`InvokeThunk` + loader dispatch-table path every domain already uses
([`CAPABILITY-REGISTRIES.md`](./CAPABILITY-REGISTRIES.md)):

- New loader domain: `"unit"` → `register_unit_provider_backend`.
- The proxy (`FfiUnitProvider`) implements `UnitProvider` by marshaling
  `units`/`invoke` over the FFI thunk, exactly like `FfiAgentProvider` /
  `ContainerRuntimeProxy`.
- Registry keyed by **provider name** (not by kind) — a provider owns many units.
- `BackendDef.capabilities` carries provider-level flags; per-unit verbs come
  back in each `UnitDescriptor` (they vary per unit, so they can't live on the
  static descriptor).

## What folds in vs. what stays

**Folds into `ManagedUnit`:** `deploy_target` (generalized — it's the seed),
`container_runtime` (the seam already built becomes a `UnitProvider` whose units
are containers), `service` (a service is a unit exposing
Status/Start/Stop/Update/Backup/Restore/Configure), and `vms`.

**Stays separate (different shape, not lifecycle):**
- `storage` — `mount`/`unmount`/`usage`/`shares` are storage-specific; only
  `recover_stale` rhymes. Keep as its own domain (may expose `Status`/`Recover`
  as a unit facet later, but not now).
- `topology`, `cluster_roster` — pure collectors (one fetch verb), not lifecycle.
- `notifications` — write-only `emit`.
- `agents` — composition provider, unrelated shape.

These four collector/provider domains are candidates for **Axis 3** (dedupe the
identical register/proxy/registry boilerplate into one generic mechanism) — out
of scope here.

## Verb payloads (sketch)

Typed per verb — no opaque JSON (hard rule). `VerbArgs`/`VerbOutcome` are enums:

```
Status  -> () / UnitStatus (+ health, metrics)
Start/Stop/Restart -> () / ActionOutcome
Update  -> UpdateArgs{ version?/channel? } / ActionOutcome
Backup  -> BackupArgs{ dest? } / BackupArtifact
Restore -> RestoreArgs{ from: BackupArtifact } / ActionOutcome
Configure -> ConfigureArgs{ config: String } / ActionOutcome
Logs    -> LogsArgs{ tail } / String
Exec    -> ExecArgs{ cmd, stdin? } / ExecOutput
Recover -> () / ActionOutcome
Migrate -> MigrateArgs{ to: UnitId /*target manager*/ } / ActionOutcome
```

`BackupArtifact` / `ExecOutput` are reused from `service` / `containers` verbatim.

## Migration path (stepping stones)

1. **Land `contract::unit`** — the trait, `UnitId`/`UnitDescriptor`/`Verb`/typed
   payloads, registry, `register_from_def`, FFI proxy, host-side `dispatch_op`.
   (Mirror `containers::ffi`, which is the template already in-tree.)
2. **Loader** — add the `"unit"` domain arm (+ deregister). *Also fix the missing
   `service` deregister arm found in the survey.*
3. **Fold `container_runtime`** — reframe the docker/proxmox adapters as
   `UnitProvider`s. The `container_runtime` seam built this session is the direct
   predecessor; container ops map 1:1 onto canonical verbs. Keep it working until
   the fold completes, then retire it.
4. **Generalize `deploy_target`** into the unit provider (it already has the
   identity + verbs + capability gating).
5. **Fold `service` and `vms`.**
6. **Tool surface** — expose `unit.{list,status,start,stop,restart,update,backup,
   restore,configure,logs,exec}` as the single operator-facing surface; retire the
   per-domain lifecycle tools.

Each step builds green and is independently committable. Nothing is dropped —
capabilities relocate behind the canonical surface
([[abstract-means-generic-core-concrete-plugin]]).

## Bugs to fix along the way

- **`service` has no deregister arm** in the loader → stale backend on unload.
- **`container_runtime` keys registry by `RuntimeKind`** (one-per-kind); the unit
  registry keys by provider name + `UnitId`, which is what lets one provider
  expose many units (the proxmox blocker).

## Open questions

1. **Single `invoke(verb)` vs per-verb trait methods.** Single `invoke` is
   cleaner over FFI + capability-gating; per-verb methods give static typing.
   Proposal: single `invoke` with a typed `Verb`/`VerbArgs`/`VerbOutcome` enum.
2. **Does `storage` become a unit facet** (exposing Status/Recover) or stay fully
   separate? Proposal: stay separate now; revisit.
3. **Streaming** (`logs -f`, `exec` TTY) — out of scope; current verbs are
   request/response, matching today's adapters.
4. **Update semantics** — `update` a container (repull image + recreate) vs a
   service (new version) vs a host (apt/pkg) differ; the plugin owns the how, but
   we should agree the `UpdateArgs` shape (version/channel/none).
