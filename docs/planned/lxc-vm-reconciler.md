# LXC + VM reconciler — declarative Proxmox guests

> **HARD RULE — user-triggered changes only.** Orca detects drift,
> notifies, and waits. The user runs `orca apply <change-id>` (or
> accepts a UI prompt). No reconciler ever auto-applies. Applies
> symmetrically to repo-side edits, live-config drift, and
> rotation-triggered changes. See ROADMAP §1.11.

Goal: stop hand-`sed`ing `/etc/pve/lxc/<vmid>.conf` on PVE hosts. The
repo (`meerkat/proxmox/configs/lxcs/*.conf`,
`meerkat/proxmox/configs/vms/*.conf`) is supposed to be the source of
truth, but nothing reconciles it against the live PVE config — drift
is silent until restore time. Orca owns the diff-and-apply loop,
restore-aware lifecycle, bind-mount + tmpfs ownership, and inner-service
health.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. The drift, concretely

njord (CT 114, frigg) restore on 2026-06-01:

- Live `/etc/pve/lxc/114.conf` carried an `mp2` transcode tmpfs bind
  that `meerkat/proxmox/configs/lxcs/114-njord.conf` lacked.
- mp0/mp1 in the repo still pointed at the retired willow NFS server.
  Restore brought the CT up against `/mnt/willow/*` paths that no
  longer exist; cutover to `/mnt/pool/*` was a manual `sed` on the
  live config.
- After `pct start 114`, `plexmediaserver` inside the container came
  up `enabled but inactive`: bind sources weren't populated yet, and
  there was no host-side gate.
- Multiple `vzrestore` attempts from PBS; no orchestration recorded
  which snapshot landed, or that the topology had changed since that
  snapshot was taken.

This is the single biggest gap in the planned set. Every CT/VM
config in `meerkat/proxmox/configs/{lxcs,vms}/` carries the same risk.

---

## 2. Scope

### 2.1 Config reconciler — diff and apply

Repo `<vmid>-<name>.conf` is the desired state. Live `/etc/pve/lxc/<vmid>.conf`
(or `/etc/pve/qemu-server/<vmid>.conf` for VMs) is the realized
state. The reconciler:

1. Parses both into the same struct (key/value plus repeated `mp*`,
   `dev*`, `net*`).
2. Diffs by key. Each diffed key has a **strategy** that decides how
   to resolve.
3. Applies via `pct set` / `qm set` for online-mutable keys, or
   stages a `pct stop` → edit → `pct start` window for keys that
   require the guest stopped.

### 2.2 Apply strategies

Declared per key in a config-store schema row, not in the
`<vmid>.conf` itself (keeps the repo file vanilla `pct` syntax).

| Strategy | Behavior |
|---|---|
| `replace` | Repo wins. Drift is overwritten on next apply. |
| `preserve-runtime-additions` | Keys present live but not in repo are kept (e.g. operator added an `mp2` transcode bind by hand). Keys differing in value are still repo-wins. New repo keys are added. Used for `mp*` on guests that legitimately grow runtime mounts. |
| `fail-on-drift` | Any difference aborts the reconcile and surfaces an actionable diff. Used for security-sensitive keys (`features`, `unprivileged`, `lxc.apparmor.*`). |

Default for all keys is `replace`. `mp*` defaults to
`preserve-runtime-additions` because that's the njord-style failure
mode we just hit. Operator overrides per CT in the config store.

### 2.3 Bind-mount source readiness

`pct start <vmid>` blocks until every host-side bind source in the
`mp*` list passes a probe:

- Path exists on the host.
- For NFS-backed sources: mount is fresh (no stale-handle ESTALE on a
  test `stat`).
- For tmpfs sources: unit is `active`.

`pct start` does not bypass this with `--skip-mount-check` or
similar; the gate is at orca's level, not PVE's. A bind that times
out (default 60s) fails the start and emits a structured event with
the offending source.

### 2.4 Inner-service start gate

`pct start` returning 0 is not workload-up. After the CT enters
`running`, orca probes the inner service:

- For services declared with `runtime = "lxc:<vmid>"` in the config
  store, the service plugin's `health` is called inside the CT (via
  `pct exec`).
- Inner units that are `enabled` but `inactive` after the bind-source
  probe completes are restarted once, then alerted if still inactive.

njord's plex-enabled-but-inactive came from binds being empty at
service-start time. The host-side bind probe + a single inner
restart closes that loop.

### 2.5 Tmpfs scratch volumes

Declarative per-service RAM caps, owned by orca, not by `/etc/fstab`
on the PVE host.

Today on frigg: `/var/lib/orca-transcode` is an 8G fstab tmpfs shared
by CT 113 (jellyfin) and CT 114 (njord) via per-CT `mp2` binds. No
per-CT quota — either guest can fill it and starve the other.

Orca model:

```toml
# config/<pve-host>/tmpfs.toml
[[scratch]]
name      = "transcode"
mountpoint = "/var/lib/orca-transcode"
size      = "8G"
mode      = "1777"

  [[scratch.consumer]]
  vmid     = 113
  bind_to  = "/var/lib/jellyfin/transcodes"
  quota    = "4G"

  [[scratch.consumer]]
  vmid     = 114
  bind_to  = "/transcode"
  quota    = "4G"
```

Reconciler:

- Owns the systemd `*.mount` unit for the host-side tmpfs. Replaces
  the fstab line.
- For each consumer, writes the `mp*` bind into the CT config under
  the §2.2 strategies.
- Enforces per-consumer quota via per-bind subdirectory + project
  quota or (initially) a directory size watchdog. Quota enforcement
  is the gap to flag; subdir-per-consumer is the v1 floor.

### 2.6 Restore-aware lifecycle

Wraps `vzrestore` / `qmrestore`:

1. Operator (or scheduler) names a PBS snapshot to restore.
2. Orca records the snapshot id, restore start time, restoring
   operator/identity, and target VMID into the audit DB **before**
   invoking PBS.
3. After the underlying restore succeeds, orca diffs the
   just-restored live config against the repo and the current storage
   topology — willow paths that have since cut to pool, GPU passthroughs
   that moved, network bridges renamed. Mismatches surface as a
   pre-start blocker.
4. Operator (or automation) applies the topology fix as a normal
   reconcile pass, not as ad-hoc `sed`.
5. Only then `pct start`, gated by §2.3 + §2.4.

Side effect: the audit trail finally answers "what PBS snapshot is
this CT actually running off of?" — today there is no record.

### 2.7 Drift detection (background)

A periodic job (default every 10 min, ride the scheduler at
`projects/system/src/scheduler.rs`) walks repo vs live for every CT/VM
on every PVE node and emits a single `proxmox.drift` event per
diverged key. Drift is actionable diff, not silent: surfaced in
observability and in `orca proxmox drift list`. Ties to the parity
rule — nothing retires the repo files until reconcile passes drift-free
on every host.

---

## 3. CLI

```
orca proxmox guest list                       # all CT/VM with drift state
orca proxmox guest drift <vmid>               # repo vs live diff, per key
orca proxmox guest reconcile <vmid>           # apply repo → live (respects strategy)
orca proxmox guest reconcile <vmid> --dry-run
orca proxmox guest restore <vmid> --snapshot <pbs-ref>
orca proxmox guest start <vmid>               # gated by §2.3 + §2.4
orca proxmox guest stop <vmid>
orca proxmox tmpfs list
orca proxmox tmpfs reconcile <name>
```

`orca proxmox guest start --force` exists for the rare case the
operator wants to bypass the bind/inner-service gate, audit-logged.

---

## 4. Work breakdown

| # | Item | Size |
|---|------|------|
| LR1 | `pct.conf` / `qemu-server.conf` parser + serializer (preserve key order, comments) | M |
| LR2 | Diff engine + strategy registry (`replace`, `preserve-runtime-additions`, `fail-on-drift`) | M |
| LR3 | `orca proxmox guest drift` + scheduler-driven periodic check | M |
| LR4 | `orca proxmox guest reconcile` apply path (`pct set` / restart window) | L |
| LR5 | Bind-source readiness probes (host path / NFS staleness / tmpfs active) | M |
| LR6 | Inner-service health gate (delegate to service plugins via `runtime = lxc:N`) | M |
| LR7 | Tmpfs scratch model: declarative `*.mount` unit + per-CT subdir bind + quota floor | M |
| LR8 | Restore-aware lifecycle: pre-restore audit record + post-restore topology check | M |
| LR9 | `orca proxmox guest start/stop` wrappers honoring gates + `--force` audit | S |
| LR10 | Parity check + retire `proxmox/lxcs/*.sh` shell helpers (njord.sh etc.) | M |

LR1+LR2+LR3 are the MVP that surfaces drift without applying.
LR4 turns it from observation into action. LR5+LR6 close the
"started but not up" loop. LR8 closes the restore loop.

---

## 5. Cross-refs

- [host-lifecycle.md](host-lifecycle.md) §1.5 — pointer here for the
  Proxmox-guest slice of host lifecycle.
- [storage-shares.md](storage-shares.md) — `mp*` sources resolve
  through the storage abstraction; willow→tyr cutover should be a
  pool-name change, not 30 path edits.
- [backup-restore.md](backup-restore.md) — `proxmox_vzdump` /
  `pbs_remote` are the source kinds for these guests; restore-aware
  lifecycle integrates with the `backup restore` verb.
- [schema-evolution.md](schema-evolution.md) — parity rule governs
  retirement of every hand-edited `pct` workflow.

---

## 6. Open questions

- **Where do `lxc.*` raw entries live?** PVE config supports
  pass-through `lxc.apparmor.profile = unconfined` etc. Default to
  `fail-on-drift` for any `lxc.*` key (security surface), with
  explicit opt-in per-CT to `replace`.
- **GPU passthrough re-detection.** `dev0: /dev/dri/card0,gid=44`
  on frigg is tied to local device numbering. On a different node
  the renderD/card index can shift. Reconciler should treat `dev*`
  as `replace` only on the originating node; cross-node moves
  surface a hard-block until operator confirms.
- **VM scope vs LXC scope.** Same reconciler, different on-disk
  format. Land LXC first (more drift today); VMs after.
- **Who owns the `onboot` flag?** Repo or runtime? Default repo
  (predictable post-reboot state), but operators sometimes flip
  `onboot: 0` mid-maintenance. Pair with `preserve-runtime-additions`
  scoped to `onboot` alone.
