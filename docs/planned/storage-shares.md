# Storage shares — native NFS/SMB/mDNS management — scope

Goal: stop hand-editing `/etc/exports`, `/etc/samba/smb.conf`, Avahi
service files, and `wsdd` units on storage hosts. Make orca own
**share definitions** as config-as-code and reconcile the on-host
daemons so a single declarative share is exposed correctly to
**macOS, Windows, and Linux** at once.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Why

- Today every share is hand-configured per host. The scottkey gateway
  (`pool.scottkey.me`, re-exporting willow storage) had working NFS +
  SMB but appeared in macOS Finder as a generic **"PC" with no
  browseable shares**, while the Unraid boxes (maple/willow) appear as
  **Mac** with full share lists — because Unraid auto-wires `vfs_fruit`
  + Avahi and orca-managed hosts do not.
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
- **Boot ordering:** re-export hosts must order `nfs-server`
  `RequiresMountsFor=` their upstream mounts (the gateway hit exactly
  this — nfsd raced ahead of its willow mounts and came up dead). The
  reconciler should emit that drop-in automatically for re-export
  shares.

---

## 6. Endpoints

`share_list`, `share_create`, `share_update`, `share_delete`,
`share_status` (per-backend health: exportfs state, smbd share
visibility, avahi advertisement present, wsdd active). mTLS like the
rest of the orca MCP/REST surface.

---

## 7. Cross-refs

- [orca-as-logic-layer.md](orca-as-logic-layer.md) — umbrella migration.
- [backup-restore.md](backup-restore.md) — backups consume these shares.
- [host-lifecycle.md](host-lifecycle.md) — package install (avahi/wsdd).
- [schema-evolution.md](schema-evolution.md) — parity before retiring
  hand-rolled configs.
