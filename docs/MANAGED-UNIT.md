# Managed Unit — the universal capability surface

> Status: **in progress** (2026-07-01). `contract::unit` landed. Fold of
> `container_runtime` in progress. Companion:
> [`CAPABILITY-REGISTRIES.md`](./CAPABILITY-REGISTRIES.md).

## Why

orca exists to **abstract away the differences between all our systems**. A VM,
an LXC, a Docker container, a service, a media library item, and the managers
themselves (Proxmox, Unraid, Sonarr) are all things you perform CRUD against.
Today each lives behind a different trait with different verb names. That
multiplies API surface and forces the host to know *which kind of thing* it's
talking to.

**Goal:** five canonical verbs that work against *any* managed thing, so adding
a new system type is "map its native ops onto the canonical ones" — not "invent
a new domain."

## Core principle: orca defines WHAT, plugins define HOW

orca core owns **what can be done** — the five-verb vocabulary and a generic
declaration toolset. A plugin owns **how it's done** — it **declares** which
verbs and actions it supports with **typed schemas**, then implements the
behavior. orca drives validation/routing/UI generically against those
declarations without hardcoding any domain. Fully typed, no exceptions.

## Five verbs

```
List   — GET collection + query params  (search, filter, log tail, …)
Detail — GET one item                   (state, metadata, logs with query params)
Create — POST new thing                 (provision VM, add media, take backup, exec)
Update — PATCH state                    (start, stop, restart, migrate, restore, configure, version-bump)
Delete — DELETE                         (destroy, remove)
```

The `action` field inside `CreateArgs`/`UpdateArgs` discriminates variants:

| domain op | verb | action |
|---|---|---|
| start VM | Update | `start` |
| stop VM | Update | `stop` |
| restart service | Update | `restart` |
| provision VM | Create | `provision` |
| take backup | Create | `backup` |
| exec in container | Create | `exec` |
| restore from backup | Update | `restore` |
| migrate to host | Update | `migrate` |
| add TV show | Create | `add` |
| metadata refresh | Update | `refresh` |
| get logs | Detail | _(query: tail, since)_ |
| search media | List | _(query: search, kind)_ |

No domain concept leaks into core. The args carry all semantics; the verb is
just the CRUD axis.

## Canonical types (landed in `contract::unit`)

```rust
pub enum Verb { List, Detail, Create, Update, Delete }

pub struct UnitId { manager, kind, id, name }

pub struct QueryArgs { search?, kind?, limit?, offset?, extra? }
pub struct ListArgs   { query: QueryArgs }
pub struct DetailArgs { id: UnitId, query: QueryArgs }
pub struct CreateArgs { action: String, payload?: String }  // payload = schema-validated JSON
pub struct UpdateArgs { id: UnitId, action: String, payload?: String }
pub struct DeleteArgs { id: UnitId }

pub enum VerbArgs { List(ListArgs), Detail(DetailArgs), Create(CreateArgs),
                    Update(UpdateArgs), Delete(DeleteArgs) }

pub struct ItemOutcome  { id: UnitId, payload: String }  // Detail / Create-with-result
pub struct ItemsOutcome { items: Vec<ItemOutcome>, total?: u64 }
pub struct ActionOutcome { changed: bool, message: String }

pub enum VerbOutcome { Items(ItemsOutcome), Item(ItemOutcome), Action(ActionOutcome) }
```

Payloads crossing the FFI boundary are JSON strings validated against the
plugin's declared schema — generic *and* typed at the boundary, never an opaque
blob inside core.

## Provider trait

```rust
pub trait UnitProvider: Send + Sync {
    fn name(&self) -> &str;
    fn declarations(&self) -> Vec<KindDeclaration>;            // sync, cheap
    fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>>;  // enumerable units
    fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>>;
}
```

One plugin registers **one or more providers** — one per resource domain.
Sonarr registers providers for `tv_show`, `season`, `episode`; proxmox registers
one provider that enumerates many `vm` + `lxc` units.

Pure query-based providers (media libraries) return `Ok(vec![])` from `units()`;
all access is via `List`/`Detail`/`Create`/`Update`/`Delete`.

## Declarations (plugin declares HOW)

```rust
pub struct KindDeclaration {
    pub kind: String,           // "vm", "tv_show", "lxc", … — free string, never a core enum
    pub verbs: Vec<VerbDecl>,
}
pub struct VerbDecl {
    pub verb: Verb,
    pub query_schema: Option<Schema>,    // for List / Detail extra params
    pub actions: Vec<ActionDecl>,        // for Create / Update variants
}
pub struct ActionDecl {
    pub action: String,                  // "start", "provision", "add", …
    pub payload_schema: Option<Schema>,  // typed args for this action
    pub response_schema: Option<Schema>, // None = ActionOutcome
}
```

## No kind is owned by a plugin

`vm`, `lxc`, `container`, `service`, `tv_show` are just kind strings. Core
defines the surface (five verbs + typed args); a plugin declares it implements
that surface for the kinds it enumerates. Proxmox provides `vm`/`lxc` today;
an Alpine host with libvirt or raw LXC could register a second provider for the
same kinds tomorrow — the host treats both uniformly. Providers are keyed by
name; units by `UnitId{manager,…}` — two managers offering `vm` units never
collide.

## Managers are units too (recursion)

`parent` in `UnitDescriptor` lets a unit both *be* managed and *manage* others.
Proxmox itself is a `host` unit (Update `restart`/`upgrade`) *and* a provider
enumerating its vm/lxc units, each with `parent = <the proxmox UnitId>`. Unraid
the same. The host renders/acts on a tree, uniformly.

## Registration — same seam as every other domain

No new machinery. `plugin-loader` already has `"unit" => register_unit_backend`
in its domain dispatch table. The `FfiUnitProvider` proxy marshals
`units`/`invoke`/`declarations` over the same `InvokeThunk` wire every domain
uses.

## What folds in

| domain | fold | notes |
|---|---|---|
| `container_runtime` | → `UnitProvider` per manager | each container = a unit; `start`/`stop`/`exec`/`logs` = Update/Detail actions |
| `deploy_target` | → `Create{provision}` + `Delete` + manager verbs | already ~80% there |
| `service` | → `UnitProvider` returning service units | backup/restore/configure map cleanly |
| `vm` / `lxc` | no fold needed | kinds provided by proxmox (and future) plugins |
| media (Plex, Sonarr, …) | `UnitProvider` per library/domain | `List{search}`, `Create{add}`, `Update{refresh}` |

**Stays separate:** `storage` (mount/unmount/shares), `topology` + `cluster_roster`
(pure collectors), `notifications` (write-only emit), `agents` (composition
provider). These are candidates for Axis 3 (generic registration boilerplate
dedup) — out of scope here.

## Migration path

1. ✅ **Land `contract::unit`** — five verbs, typed args/outcomes, provider trait,
   registry, FFI proxy.
2. ✅ **Loader** — `"unit"` domain arm.
3. 🔄 **Fold `container_runtime`** — docker/proxmox adapters become `UnitProvider`s;
   retire `container_runtime` seam when green.
4. **Fold `deploy_target`** — generalize into provider; `provision`/`destroy` are
   now `Create`/`Delete`.
5. **Fold `service`** — app-level provider.
6. **Type consolidation** — `service::BackupArtifact` → `unit`; `containers::ExecOutput`
   → `unit`; `WorkloadSpec` → `contract`.
7. **Tool surface** — `unit.{list,detail,create,update,delete}` replaces per-domain
   lifecycle tools.

## Open questions

- **Streaming** (`logs -f`, `exec` TTY) — out of scope v1; verbs are req/response.
- **`storage` as unit facet** — Detail/Update(recover) possible later; separate now.
