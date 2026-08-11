# Holistic deploy & converge — surfaces, deployables, one reconcile engine

Status: **DESIGN** (design-of-record; not yet implemented). Supersedes the
storage-only framing in [`nfs-share-model.md`](./nfs-share-model.md) by lifting
its mount/converge pattern into a domain-agnostic model. Storage is the first
instantiation we harden; container / stack / LXC / VM follow on the same seams.

## The principle

> A **deployable** declares *what kind of surface it targets*. orca knows *which
> hosts expose which surfaces*. If a surface exposes a `deploy` (and/or `adopt`)
> for that kind, orca can realize the thing on **any host advertising that
> surface**.
>
> **deploy = deployable-kind × surface-that-hosts-it × host-that-advertises-it.**
>
> **Converge** is the reconcile loop over that relation: adopt-or-deploy,
> drift-heal, and — for storage — replication-aware failover.

This is the domain unification (`pod`/`system`/`service` → generic node with
optional facets) applied to *deployment*: one engine, many domains. orca defines
*what* (the generic surface / deployable / deploy / converge contracts); plugins
define *how* (the per-surface backend). Core never learns Docker, Unraid,
Proxmox, or Syncthing specifics.

## Core concepts

### Surface
A capability a host exposes that can *host / realize* a kind of deployable. A
host advertises zero or more surfaces in the existing capability registry
(`system.detail view=capabilities`). Surfaces are the extension point — every
new backend is "a plugin that advertises a surface + exposes `deploy`/`adopt`
for the kinds it hosts."

| deployable kind | surface(s) that host it | backend owner |
|---|---|---|
| container | `docker`, `unraid-docker`, `dockge` | docker / unraid / dockge plugin |
| compose stack | `dockge`, `docker` | dockge / docker plugin |
| LXC guest | `proxmox-lxc` | proxmox plugin |
| VM | `proxmox-vm`, `kvm` | proxmox plugin |
| NFS/SMB export (server) | `unraid`, `host-fs` | unraid / system |
| mount (client) | `nfs`, `smb` | system (storage-converge) |
| replication (folder) | `syncthing` (rides any container surface) | syncthing plugin |

Note the same kind can be hosted by *multiple* surfaces (a container by
`docker`, `unraid-docker`, or `dockge`), and a host can advertise *several*
surfaces at once. **Target state: both willow and maple advertise
`unraid` + `docker` + `dockge`** (both are Unraid boxes that also run a plain
Docker engine and Dockge) — this is the substrate that makes willow↔maple
replication and storage failover real.

### Deployable
A declared *desired thing*. It names the **kind** and the **surface type** it
targets, plus a kind-specific spec. It does **not** name a host directly beyond
its placement; converge picks/realizes it on a host advertising the surface.

### Deploy verb (per surface)
A surface backend exposes `deploy` (create where absent) and, ideally, `adopt`
(discover an existing instance and bring it under management *without
clobbering*). **adopt-or-deploy is a converge property**, mirroring how
storage-converge already tolerates an already-mounted target.

### Converge engine (domain-agnostic)
The reconcile loop. For each desired deployable: resolve the owning surface
backend for the target host → `adopt` if the thing already exists, else
`deploy` → detect drift and heal → for storage, evaluate failover. The engine is
generic; per-domain logic (how to compare drift, how to elect a route) is
supplied by the domain/plugin.

| domain | desired source of truth | surfaces | converge job |
|---|---|---|---|
| **storage** | share (canonical routes) | nfs, smb (client) | mount / failover / drift-heal |
| **container/stack** | container/stack spec | unraid, docker, dockge | adopt-or-deploy / drift-heal |
| **LXC/VM** | guest spec | proxmox-lxc, proxmox-vm, kvm | adopt-or-deploy / drift-heal |
| **replication** | replication relationship | syncthing | ensure folder linked / report health |

---

## First slice (detailed): storage

### Share — the canonical storage definition
The share owns **the one routes array**. (Already true today.)

```
share {
  name, backend (nfs|smb|…), fstype,
  options (authored)  →  optionsRendered (derived),
  routes: [ Route { kind, value, port, path, enabled } ],   // ordered: primary first; enabled=false = held
  replication: { id }?,                                      // ref to a replication relationship (§ replication)
}
```

`routes` = the failover candidates. Order = preference. Single source of truth
for "where the data lives."

### Mount — a placement (owns no routes)
"Host H mounts share S at target T." References a share; carries **no routes of
its own**. Both the dead stored `mounts.routes` column (always empty, never read
— converge already elects from `share.routes`, `mount_converge.rs:102`) and the
`activeRoute` scalar are **removed**. The view **derives** routes from the joined
share and annotates each with live status; **the route self-annotates as
active** — there is no separate active field on the mount.

```
mount {
  id, name, share:{id}, host:{id}, target, remountPolicy?, enabled,
  health,                                   // stored, from converge tick
  routes: [ MountRoute {
     kind, value, port, path, enabled,      // ← from share.routes
     active:  bool,                         // is THIS route what's mounted at target now?
     options: string?,                      // live -o tokens for the active mount
     drift:   bool,                         // live options ≠ share.optionsRendered
  } ],
  multiMounted: bool,                       // >1 active route at target = anomaly
}
```

- Normally exactly one route has `active=true`.
- `options`/`drift` live on the active route (per-live-mount facts). So "frigg is
  still `timeo=50`" surfaces as the active route with `drift=true`.

### Multi-mount — block on write, tolerate on reconcile
- **Write blocks by default:** `mount.create`/`update` refuses a second placement
  that would stack the same share on the same host+target, unless an explicit
  override.
- **Reconcile tolerates reality:** if converge observes the target mounted more
  than once, it does **not** choke — sets `multiMounted=true`, `health=degraded`,
  emits a **warning**, and exposes a **gated remediation** (unwind the extraneous
  mounts down to the single elected route). Remediation is operator-triggered by
  default (deny-by-default), not silently aggressive.

### Converge — replication-aware failover (the crux)
- Candidates from `share.routes`. Election: first healthy enabled route (primary),
  else next enabled.
- **Failover safety gate:** only swap route A→B when **replication is confirmed
  healthy** between A and B. If replication is unknown/unhealthy, **hold on
  primary + warn** — failing over to unreplicated/stale data is worse than
  waiting.
- Drift-heal (remount on option drift) and multi-mount unwind ride the same tick.

---

## Replication — generic in core, populated by the syncthing plugin

Split **config** (what *should* be replicated) from **observed health** (is it
*actually* synced) — per on-demand-not-poll-and-cache.

```
// CONFIG (replicated): a relationship shares reference
replication { id, provider:"syncthing", folder, members:[host…] }

// OBSERVED (local, on read/event — not a poll-cache):
ReplicationStatus { provider, healthy, lastSyncMs?, detail? }   // "willow↔maple 100% (idle)"
```

A **separate `replication` generic** that shares reference (one relationship can
back a folder used by multiple shares), not a field buried on the share.

### The syncthing plugin
Owns **provision** and **status**, and — critically — **does not deploy
containers itself**. It declares a desired Syncthing *container deployable* and
lets **container-converge** realize it on the surface that owns each member host:

- **Adopt-first.** willow (Unraid) already runs Syncthing → container-converge
  resolves to **adopt** (read its config/API, manage in place, never clobber).
- **Deploy where missing.** maple gets a Syncthing instance via its owning
  surface (`unraid-docker`, `docker`, or `dockge`).
- **Link the folder** across the member hosts of a share's routes (willow↔maple),
  forming the `replication` relationship.
- **Report** folder sync state → feeds `ReplicationStatus.healthy` → the converge
  failover gate consults it.

Flow to make a share failover-safe:
**share has 2+ routes → syncthing plugin declares a Syncthing container on each
route's host → container-converge adopts/deploys it via the owning surface →
folder linked → replication healthy → converge is now permitted to fail over.**

---

## Container / stack / LXC / VM — same seams (spec now, build later)

All four are deployables over surfaces; container-converge / guest-converge are
the same engine with domain-specific drift comparison:

- **container** over `docker` | `unraid-docker` | `dockge` — each host advertises
  which surface owns its containers; a desired container is declared once and
  dispatched to the owning backend. **Unraid's Docker is its own backend** (its
  own daemon/paths/templates), *not* "plain Docker."
- **compose stack** over `dockge` | `docker`.
- **LXC guest** over `proxmox-lxc`; **VM** over `proxmox-vm` | `kvm` — the
  proxmox plugin owns these surfaces.
- adopt-or-deploy applies uniformly: does the desired guest/container already
  exist on the surface? adopt : deploy.

---

## Holistic view (horizon)

One projection joins the domains:

> **service → its containers (which surface, which host) → its mounts (which
> routes, which active) → replication (healthy? last sync).**

e.g. *"jellyfin@frigg (dockge) → /mnt/backups ← share `backups` ← willow(primary,
active, timeo=150) | maple(replica, held) ← syncthing healthy, synced 2m ago."*

Storage first; the same join extends to container/guest/network as those
domains land on the shared engine.

---

## Implementation ordering

1. **Storage routes model** — remove `mounts.routes` + `activeRoute`; derive
   MountRoute[] from the share with `active`/`options`/`drift` self-annotation;
   multi-mount block-on-write + tolerate-on-reconcile (+ gated remediation).
2. **Replication generic + failover-safety gate** — `replication` relationship,
   `ReplicationStatus` on read, converge gate that only fails over when healthy.
3. **Surface substrate on willow + maple** — both advertise
   `unraid` + `docker` + `dockge`; adopt existing instances (willow Syncthing
   first).
4. **syncthing plugin** — adopt-or-deploy Syncthing via container-converge;
   report status feeding the failover gate.
5. **Generalize converge** — express container/stack/LXC/VM on the shared
   engine; the holistic-view projection.

## Open decisions
1. Replication config as a separate generic (leaning yes) vs a field on the share.
2. Multi-mount remediation gated/operator-triggered (leaning yes) vs converge
   auto-unwind.
3. Does `replication` reference the *share* or the *routes' host-pair*? (A
   relationship is fundamentally between hosts for a folder; shares reference it.)

## Principles carried from prior design
- Core generic; NFS/SMB/Docker/Proxmox/Syncthing are plugins with zero
  protocol/vendor code in core.
- Mesh config eventually consistent; observed health is local + on-demand, never
  a poll-cache.
- Deny-by-default for anything mutating/irreversible (multi-mount unwind,
  deploy-over-existing).
- Adopt before deploy; never clobber a running instance.
