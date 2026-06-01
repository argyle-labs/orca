# Storage shares — native NFS/SMB/mDNS management — scope

> **HARD RULE — user-triggered changes only.** Orca detects drift,
> notifies, and waits. The user runs `orca apply <change-id>` (or
> accepts a UI prompt). No reconciler ever auto-applies. Edits to
> `/etc/exports` / `smb.conf` / Avahi / wsdd are emitted as pending
> changes for operator review. See ROADMAP §1.11.

Goal: stop hand-editing `/etc/exports`, `/etc/samba/smb.conf`, Avahi
service files, and `wsdd` units on storage hosts. Make orca own
**share definitions** as config-as-code and reconcile the on-host
daemons so a single declarative share is exposed correctly to
**macOS, Windows, and Linux** at once.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Why

- Today every share is hand-configured per host. The scottkey gateway
  (**tyr**, `10.10.10.29` — formerly pool-gw, re-exporting the
  underlying pool) had working NFS + SMB but appeared in macOS Finder
  as a generic **"PC" with no browseable shares**, while the Unraid
  boxes appear as **Mac** with full share lists — because Unraid
  auto-wires `vfs_fruit` + Avahi and orca-managed hosts do not.
- Legacy willow (`10.10.10.10`) is **retired 2026-06-01**. All
  consumers (frigg, baldur, freyr, njord) now mount through tyr at
  `/mnt/pool/*`; willow direct-mounts are rollback path only. tyr is
  canonical, willow is legacy.
- Client-side mount plugins (`projects/plugins/nfs`,
  `projects/plugins/smb`) are **shipped**. The greenfield work in
  this doc is the **server-side reconciler** for exports + smb.conf +
  Avahi + wsdd, plus mount-option policy on the client side.
- Correct cross-platform serving needs *three* moving parts kept in
  sync per share, which is exactly the kind of drift orca exists to
  eliminate:
  - **Linux** → NFSv4 export.
  - **Windows** → SMB3 share **+ `wsdd`** (Win10/11 dropped SMBv1
    browsing; without WS-Discovery the host never shows in Explorer).
  - **macOS** → SMB share **+ `vfs_fruit`** (resource forks, Finder
    metadata, `.DS_Store`, Time Machine) **+ Avahi** advertising
    `_smb._tcp` and `_device-info._tcp` (`model=` → Mac icon), plus
    `_adisk._tcp` for Time Machine targets.
- This is a storage-mesh capability: any orca node that exposes a pool
  should be able to serve it to every client OS. Generic orca ships the
  reconciler; meerkat ships the declarative share files.

---

## 2. Architecture

```
┌─────────────── consumer repo (e.g. meerkat, git source of truth) ────────┐
│  storage/shares/*.toml        ← declarative share definitions            │
└───────────────────────────────────────────────────────────────────────────┘
                                  │ git pull / import
                                  ▼
┌────────────────────── orca on the storage host ─────────────────────────┐
│  config store     ← imported shares + runtime overlay                    │
│  share reconciler ← renders + validates + reloads, per backend:          │
│      • nfs   → /etc/exports.d/orca.exports     → exportfs -ra            │
│      • smb   → /etc/samba/smb.conf.d/orca.conf → testparm + smbcontrol   │
│      • mdns  → /etc/avahi/services/orca-*.service → reload avahi         │
│      • wsdd  → manage wsdd unit + workgroup                              │
│  MCP/REST     ← share_{list,create,update,delete,status}                 │
└───────────────────────────────────────────────────────────────────────────┘
                       │ serves the same path 3 ways
        ┌──────────────┼───────────────┐
     NFSv4           SMB3+fruit       mDNS/wsdd
   (Linux)         (macOS/Windows)   (discovery)
```

Generic orca: ships the **share reconciler + backends + endpoints**.
Knows nothing about meerkat or scottkey.me.

Consumer-side: ships the **declarative share files**. Orca reads
`storage/shares/` from the repo path it is pointed at via bootstrap.

---

## 3. Config model

`storage/shares/pool.toml` (consumer repo, git-tracked):

```toml
[[share]]
name      = "data"
path      = "/srv/pool/data"
# which backends to expose this share through
backends  = ["nfs", "smb"]
discovery = ["mdns", "wsdd"]   # advertise to Mac + Windows

  [share.access]
  allow_cidrs = ["10.10.10.0/24", "100.64.0.0/10"]
  read_only   = false
  squash      = "all"          # → NFS all_squash + SMB force user
  anon_uid    = 99
  anon_gid    = 100
  valid_users = ["pool"]       # SMB auth principals

  [share.macos]
  fruit         = true          # vfs_fruit + streams_xattr + catia
  time_machine  = false         # true → _adisk._tcp + fruit:time machine

[host.identity]
# drives _device-info model so Finder shows a Mac icon, and SMB server string
model        = "RackMac"
workgroup    = "WORKGROUP"
min_protocol = "SMB3"
```

Reconciler maps one `[[share]]` to all selected backends so they can't
drift apart. `squash`/`anon_*`/`allow_cidrs` are expressed once and
translated per backend (NFS export opts vs SMB `force user`/`hosts
allow`).

---

## 4. Backends (work breakdown)

| # | Item | Size | Notes |
|---|------|------|-------|
| 4.1 | **NFS backend** | M | Render `/etc/exports.d/orca.exports`; `exportfs -ra`; support `crossmnt` (required when the export path is itself an NFS re-mount, as on the gateway); fsid allocation. |
| 4.2 | **SMB backend** | L | Render `smb.conf.d/orca.conf` (global + per-share); `testparm` gate before reload; `smbcontrol reload-config`; manage `pdbedit` users / map `valid_users`; `vfs objects = catia fruit streams_xattr` + sane `fruit:*` defaults when `macos.fruit`. |
| 4.3 | **mDNS/Avahi backend** | M | Install/ensure `avahi-daemon`; render `_smb._tcp` + `_device-info._tcp` (model) + optional `_adisk._tcp`; reload. This is what fixes "shows as PC". |
| 4.4 | **wsdd backend** | S | Install/manage `wsdd` unit + workgroup so Win10/11 discover the host. |
| 4.5 | **Identity/global** | S | `host.identity` → SMB server string, workgroup, min protocol, device model. |

---

## 5. Reconcile + safety

- **Validate before apply:** `testparm` (SMB) and an `exportfs` dry
  pass; never reload on a config that fails to parse.
- **Idempotent renders** into `*.d/orca.*` drop-ins — never clobber a
  hand-written base; orca owns only its drop-in files.
- **Parity rule** ([schema-evolution.md](schema-evolution.md)): don't
  retire the hand-maintained config until orca-rendered output is
  byte-verified equivalent and all three clients (a Mac, a Windows box,
  a Linux NFS client) are confirmed to mount/browse.
- **Boot ordering (re-export hosts):** a host that is BOTH an NFS
  client (mounting upstream backends) AND NFS server (re-exporting
  them) hits a cycle: `nfsd-generator` adds implicit
  `RequiresMountsFor=` for every export path, while `_netdev` mounts
  pull into `remote-fs.target` which `nfs-server` is `Before=`. No
  combination of drop-ins / automount / noauto on its own breaks it.
  **The reconciler must emit a custom orchestrator unit** (Type=oneshot
  After=network-online.target) that explicitly mounts upstreams,
  `exportfs -ra`s, then `systemctl start nfs-server`, with the fstab
  entries set to `noauto` and `nfs-server.service` *disabled* (the
  orchestrator owns its lifecycle). Verified on tyr 2026-05-29.

---

## 6. Endpoints

`share_list`, `share_create`, `share_update`, `share_delete`,
`share_status` (per-backend health: exportfs state, smbd share
visibility, avahi advertisement present, wsdd active). mTLS like the
rest of the orca MCP/REST surface.

---

## 7. Current state (2026-06-01) — what orca must subsume

This section captures the manual state the reconciler needs to be
**bit-for-bit compatible with** at takeover, so we can verify parity
([[schema-evolution.md]]) before retiring the hand-rolled configs.

### Server side (tyr, 10.10.10.29)

Canonical export list lives in `reference_tyr_exports`. Exports today:

```
/srv/pool/data       → 100.64.0.0/10, 10.10.10.0/24
/srv/pool/backups    → 100.64.0.0/10, 10.10.10.0/24
/srv/pool/downloads  → 100.64.0.0/10, 10.10.10.0/24
/srv/pool/orca       → 100.64.0.0/10, 10.10.10.0/24
/srv/pool/isos       → 100.64.0.0/10, 10.10.10.0/24
```

Not yet served from tyr but in scope per §1: `vfs_fruit` + Avahi
`_smb._tcp` / `_device-info._tcp` / `_adisk._tcp`, and `wsdd` for
Windows browsing. Tracking under [[project_crossplatform_shares]].

### Client side (post-cutover 2026-06-01)

All downstream consumers point at the gateway, not willow directly
([[feedback_clients_default_gateway]]):

| Host           | Method                          | Mount paths                                                  |
| -------------- | ------------------------------- | ------------------------------------------------------------ |
| frigg (PVE)    | fstab + `x-systemd.automount`   | `/mnt/pool/{data,backups}`                                   |
| baldur (Alpine)| autofs `/etc/autofs/auto.pool`  | `/mnt/pool/{data,backups}`                                   |
| freyr (Alpine) | autofs `/etc/autofs/auto.pool`  | `/mnt/pool/{data,backups,downloads}`                         |
| njord (LXC)    | PVE bind via `mp0`/`mp1` on host| `/mnt/pool/data` → `/mnt/data`, `/mnt/pool/backups/njord` → `/mnt/backups` |

Canonical NFS mount options (across all client autofs/fstab):

```
vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30
```

Plus fstab-only: `_netdev,nofail,x-systemd.automount,x-systemd.idle-timeout=0`.
Plus autofs-only: `--timeout=60 --ghost` (or `--timeout=0` for storage hosts).

### 7a. NFS mount-option policy

Today there is no policy — willow direct-mounts used
`rsize/wsize=1048576`; pool mounts (above) use `262144`. Same host,
same kernel, different numbers, no rationale.

Reconciler picks **one** baseline per use-case and applies it
uniformly through the share/mount layer:

| Use-case | Workload | `rsize/wsize` | Other |
|---|---|---|---|
| `data` (media, appdata reads + writes) | Mostly large sequential reads, occasional config writes | `1048576` | `nconnect=4`, `actimeo=30` (the willow-era numbers — media benefits from the larger I/O window) |
| `backups` | Append-heavy, large writes, low concurrency | `1048576` | `nconnect=2` (don't burn slots on a low-concurrency target), `actimeo=60` (attr churn on backup dirs is wasteful) |
| `config` / `orca` (small files, attr-sensitive, latency-critical) | Lots of small reads, frequent stat() | `262144` | `nconnect=4`, `actimeo=5` (need fresh attrs on config) |

Decision: keep `rsize/wsize=1048576` for data and backups (the
willow-era setting is right for sequential workloads), drop to
`262144` only for the config/orca share where attr-cache pressure
dominates. Pool's current global `262144` undersells data throughput;
fix at takeover.

Common across all: `vers=4.2,soft,softreval,timeo=50,retrans=2`.

### Client reconciliation — needed but not in §3 yet

Today only the **server** side is in scope. The cutover surfaced that
clients also drift (stale handles, missing tyr fstab, autofs maps
forgotten on new hosts). The reconciler should also own:

1. A `pool_mount` resource on each consumer (declarative `/mnt/pool/*`
   bindings → autofs map or systemd mount, OS-appropriate).
2. Stale-handle detection (the frigg `/mnt/pool/data` outage on 2026-06-01
   was a stale NFS handle that took manual `umount -f` + automount
   restart). dpinger-style watchdog needed.
3. Failover: when tyr's primary backend (willow) is down, the gateway
   should re-export from maple ([[feedback_storage_abstraction]],
   [[feedback_storage_replication_policy]]). Re-export logic landed
   in nfs-monitor (commit 7c1ad09); failover behaviour not yet
   validated under fault injection.

### Related parity work

- **Syncthing → orca replication**: maple ↔ willow uses Syncthing
  today ([[project_tyr_consolidation_syncthing]]). Cross-host UID
  mismatch surfaced as chmod-permission-denied pull errors; mitigated
  by `ignorePerms=true` ([[feedback_syncthing_ignore_perms]]). Orca's
  replacement must default to ignore-perms semantics or do real
  UID-mapping on receive.
- **OPNsense gateway monitoring**: not storage-specific but the same
  class of "silent backend failure" — see
  [[feedback_opnsense_gateway_monitoring]]. Pattern: every routing or
  re-export gateway needs an explicit liveness probe; "interface up"
  is not "tunnel/export healthy".

---

## 8. Cross-refs

- [orca-as-logic-layer.md](orca-as-logic-layer.md) — umbrella migration.
- [backup-restore.md](backup-restore.md) — backups consume these shares.
- [host-lifecycle.md](host-lifecycle.md) — package install (avahi/wsdd).
- [schema-evolution.md](schema-evolution.md) — parity before retiring
  hand-rolled configs.
