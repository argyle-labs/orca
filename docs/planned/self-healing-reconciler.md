# Self-healing reconciler — containers + mounts

> **§1.11 applies to drift, not to failure recovery.** Drift = live
> config diverged from declared (operator approves the apply). Failure
> recovery = declared state failed at runtime (orca restores it per
> the declared procedure, automatically, with notification). Orca owns
> the mount declaration, the container declaration, and the dependency
> between them — when a declared mount goes ESTALE, restoring it and
> the declared dependents is reconciling to user intent, not drifting
> from it. The work is in declaring the procedure *up front* so the
> automatic action is safe by construction.

Goal: stop hand-restarting docker containers that silently fell into
`Created`/`Exited`, and stop hand-chasing stale NFS handles whose only
symptom is "library scans return empty" while every container reports
healthy. Orca owns the probe loop, the dependency graph, and the
ordered remediation plan; the operator owns the apply button (except
for the narrow safe-by-construction cases below).

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. The drift, concretely

Two incidents on 2026-06-12 motivate this doc; both went undetected
until a human noticed the downstream symptom.

**sabnzbd / freyr.** Container was recreated by compose, never started.
Status sat at `Created` with no logs for hours. Watchtower-style
recreation + missing start = silent outage. The container's
`RestartPolicy` was `unless-stopped` — docker itself says "this should
be running" — but docker only honors that on host boot or daemon
restart, not on recreate-without-start.

**`/mnt/pool/data` / freyr.** NFS handle to tyr went stale after the
2026-06-09 frigg→thor migration ([[project-orca-failover-nfsv4-stale-handle]]).
Every *arr container (sonarr, radarr, radarr-4k, bazarr, lidarr,
kapowarr, mylar3, lazylibrarian) had the bind held open, so docker
health checks stayed green, but new path lookups returned ESTALE.
Library scans silently returned zero results for hours. The first
band-aid probe ([[project-self-healing-freyr-baldur]]) surfaced two
stale mounts on freyr (`/mnt/willow/downloads` + `/mnt/pool/data`)
within seconds of installation.

The shared shape: declared desired state ("container should be
running", "this mount should be live") failed at runtime and nothing
restored it. The compounding factor — "you can't `umount -lf` while
seven *arr containers hold fds open" — is real but solvable when orca
already owns both halves of the declaration. Because orca declares the
mount, the container, and the bind that ties them together, it knows
the safe order: stop dependents → drop the stale handle → remount →
start dependents. The dep graph isn't inferred at incident time, it's
asserted at declaration time.

---

## 2. Scope

### 2.1 Containers reconciler — `projects/containers/`

A plugin (see [[project-plugin-full-lifecycle-principle]]) that owns
docker container lifecycle reconciliation per host.

**Source of truth.** The container's own `HostConfig.RestartPolicy`:

- `unless-stopped` or `always` → desired state = running.
- `no` or `on-failure` → desired state = whatever it is; do not touch.
- Label `orca.skip=true` → never touch regardless of policy.

**Probe.** Every N seconds (default 30s, configurable per host —
[[project-polling-rate-too-slow]]): `docker ps -a` + per-container
inspect. For each container with desired=running and observed in
`{created, exited, dead}`, take action.

**Action — auto.** Any container with desired=running in state
`{created, exited (clean), dead}` is started. Notification dispatched
([[feedback-notifications-backend-agnostic]], info-level).

**Action — auto with circuit breaker.** Exited non-zero or crashloop
(>3 restarts in 5 min): start it, but on the next failure within the
window, hold and escalate. The breaker prevents orca from masking a
deeper failure mode (full disk, broken config) by restarting forever;
it does *not* require operator approval for the first recovery.

**Action — held.** Containers labeled `orca.heal=manual` are never
auto-restarted; orca only notifies. Escape hatch for explicitly
diagnostic states.

**Cross-host.** Per [[project-colocated-api-collectors]], the
collector runs on the host where docker lives. Other peers query via
mesh dispatch ([[project-universal-peer-dispatch]]).

Sizing: **M**. Hardest part is the crashloop classifier; the
auto-start loop itself is ~50 lines.

### 2.2 Mounts reconciler — `projects/mounts/`

A plugin that owns mount health + dependent-restart orchestration.

**Probe.** For each mount in `mount -t nfs,nfs4` plus each autofs
trigger path: `timeout 5 stat <mp>`. Failure modes classified as
`ESTALE`, `ETIMEDOUT`, `EACCES`, or `other`. Probe runs in a separate
process per mount so one stuck mount doesn't block the others.

**Dependency graph — declared, not inferred at incident time.** The
mount spec ([[storage-shares.md]]) declares the mount; the container
spec declares the bind; orca persists the (mount → dependents) edge
at declaration time. Same for LXC `mp*` lines, systemd
`RequiresMountsFor=`, VM disk paths. Sources used to build the graph:

- Docker bind mounts (`docker inspect`, keyed by `Source` prefix).
- LXC `mp*` lines from `/etc/pve/lxc/<vmid>.conf` via
  [[lxc-vm-reconciler.md]].
- systemd `RequiresMountsFor=` for host services.
- VM disk paths for QEMU guests.

The graph is rebuilt on every declaration change and cached. At
incident time, the lookup is `O(1)` per mount, not a fresh inspect
sweep.

**Remediation — auto, per declared procedure.** When a mount fails
its probe and stays failed past the cooldown
([[project-orca-stale-mount-auto-recovery]]), orca executes:

1. **Upstream health gate.** Source host's `nfs-server` /
   `smbd` responsive; source filesystem mounted on the source host;
   network path OK. If upstream is down, the mount can't be restored
   — log, notify, do not flap. Re-check on backoff.
2. **Activity drain.** Per-dependent, query open writes to the
   affected mount (`lsof +D` scoped, or container exec). If a write
   is in progress, wait up to the declared drain window (default 30s).
   Active write-heavy dependents (declared via
   `orca.heal.drain=long`, e.g. *arr import jobs) get longer windows.
3. **Stop dependents** in reverse-dep order.
4. **Drop the stale handle.** Per declared mount type:
   - **autofs-managed** (preferred default for NFS, see
     [[storage-shares.md]]): `umount -lf <mp>`; autofs re-mounts on
     next access. No explicit remount step needed.
   - **fstab/static**: `umount -lf <mp> && mount <mp>`.
   - **systemd `.mount` unit**: `systemctl restart <unit>`.
5. **Start dependents** in forward-dep order.
6. **Post-checks.** Re-probe mount; re-probe each dependent's health
   endpoint; verify the same fd is now resolvable.
7. **Notify** on the result (success → info, partial → warn, upstream
   down → crit with backoff).

**Why this is safe to auto-apply.** Every step above is declared:
the mount spec, the dep graph, the drain window per dependent, the
restart order. Orca isn't inventing a recovery plan at incident time
— it's executing the plan the operator already wrote when they
declared the mount and the container. Operator approval at apply time
adds latency without adding safety; the safety is in the declaration.

**Circuit breaker.** Same as containers — N failures within window
holds the loop and escalates. Prevents masking a real upstream
problem (tyr genuinely dead) as a flapping mount.

**Ambiguous cases that *do* prompt.** Three narrow ones:

- A dependent not previously declared as a mount consumer turns up
  holding the fd open (someone exec'd a shell, ran rsync). Orca won't
  kill an undeclared writer; operator decides.
- The mount spec on disk doesn't match the live mount options
  (manual remount with different flags). Drift — §1.11 applies.
- An autofs map references a retired host (e.g. `auto.willow`
  pointing at `10.10.10.10` post-retirement). Stale-config drift, not
  runtime failure — §1.11 applies; operator removes the entry.
- A mount source violates a declared topology invariant (e.g. a host
  other than the gateway mounts the primary storage directly). The
  mounts reconciler should flag and reject these at declaration time.
  Example: post-2026-06-12, only tyr may mount `10.10.10.10/.11`;
  every other fleet host mounts `10.10.10.29:/srv/pool/*`. See
  [[storage-shares.md]] for the invariant declaration shape.
- A graph edge implies a restart that violates a declared SLO
  (e.g. `orca.heal=preserve-uptime` on a primary service).

**Soft-mount default.** Per [[project-storage-failover-edges]],
orca-managed mounts default to `soft,softreval,timeo=50`. A live
mount missing these options is drift — operator confirms the
remount.

Sizing: **L**. Dependent-graph construction is the bulk; remediation
planner is ~200 lines.

### 2.3 Notifier integration

Both reconcilers dispatch through `projects/notifications/`
([[project-session-handoff-2026-06-10]] §9.x). Three event classes:

- **Auto-applied** (`info`): "started container `sabnzbd` on freyr
  (policy=unless-stopped, was=created)". Logged, not paged.
- **Pending change** (`warn`): "stale mount `/mnt/pool/data` on
  freyr, 8 dependents, run `orca apply mnt-2026-06-12-a` to remediate".
  Paged.
- **Probe failure escalation** (`crit`): "mount `/mnt/pool/data`
  stale 30+ min, upstream tyr unreachable". Paged with backoff.

### 2.4 Replication reconciler — `projects/replication/`

A third plugin completes the picture for storage. Syncthing is the
declared replication path between **symmetric storage peers** —
willow + maple today, see
[[project-willow-retired-tyr-sole-consumer]] and
[[project-syncthing-willow-maple-hardening]]. Orca should treat the
syncthing pair the same way it treats mounts and containers: declare
desired state, probe live state, alert on drift.

**Symmetric model — both peers write.** willow is primary day-to-day;
maple must retain full write capability so it can take over all
duties under failover. The conflict-avoidance pattern is **subdir per
host**, not folder-level direction:

- `backups/appdata/willow/` written by willow; `backups/appdata/maple/`
  written by maple. Same syncthing folder, disjoint subtrees.
- `backups/nas/unraid_usb_willow/` vs `backups/nas/unraid_usb_maple/`.
- Either peer reads any subtree; only the owning peer writes to its
  own. Same shape as the `nas/unraid_usb_<host>/` convention.

Folder type stays `sendreceive` on **both** peers. `receiveonly` on
the not-currently-primary peer breaks failover (its own backup
plugins write to the receiveonly tree → syncthing reverts → backup
silently lost). The 2026-06-12 audit demonstrated this: maple was
flipped to receiveonly, then reverted in-session when the symmetric
model was clarified.

**Probe (per peer's REST API).** Every N seconds (default 60s):

- `GET /rest/system/connections` — both declared peers `connected:
  true`; alert if disconnected > 5 min.
- `GET /rest/db/status?folder=<id>` — each declared folder
  `state in {idle, syncing, scanning}` with `errors == 0`; alert on
  any other state or `errors > 0`.
- `needBytes > 0` for > N minutes on a folder that should be in sync
  → alert. Distinguishes "syncing 28 TB after a restore" (expected)
  from "stuck at 4 GB for an hour" (broken).
- `GET /rest/system/error` — any non-empty list is paged.

**Declared invariants.** Per-folder config the reconciler enforces:

- Folder type = `sendreceive` on every declared peer. A peer in
  `receiveonly` is a config bug for symmetric-peer folders — flag and
  surface as a pending change.
- Versioning policy = `staggered` on every folder, every peer. Either
  side can recover from a deleted/corrupted file regardless of who
  caused the change.
- Subdir-per-host convention: writers declare which subtree they
  own; orca cross-checks against the local `appdata.backup`-style
  destinations and flags drift (e.g. maple's plugin writing to
  `appdata/willow/`).
- Both peers know each other; no orphan device entries.
- Host-local supporting scripts (`fix-backup-ownership` etc.) must
  match across peers — divergence (one peer aggressive, the other
  conservative) is drift. See `project_syncthing_willow_maple_hardening.md`
  for the willow/maple precedent: maple's aggressive chown was
  realigned to willow's `-type f -uid 0` version 2026-06-12 so LXC
  container-backup overwriteability is preserved under failover.

**Bootstrap helper.** New folder setup is repetitive: introduce on
both peers, accept device, set `sendreceive` + staggered versioning
+ register the per-host subdir layout. Orca should template this
(`orca replication add --peers willow,maple --folder <name>
--host-subdir-key <name>`) so new shares don't drift from the
invariants by accident.

**Failover ties to mounts reconciler (§2.2).** When the day-to-day
primary (willow) is down, the fleet's normal mount path through tyr
(`10.10.10.29:/srv/pool/*`) fails — tyr's backend is willow. The
mounts reconciler must know that maple's direct exports
(`10.10.10.11:/mnt/user/*`) are a declared failover target, and on
sustained tyr outage emit a pending change to repoint dependent
mounts. The replication reconciler verifies the failover would be
safe (maple in sync, lagBytes acceptable) before the mounts plan can
proceed. See [[storage-shares.md]] HA model.

**Why bother automating instead of just trusting syncthing?** Today's
audit (2026-06-12) found maple was healthy by syncthing's metrics
but had **zero versioning** and a `fix-backup-ownership` script
aggressive enough to break LXC container-backup overwrites under
failover. Syncthing's own metrics say "everything is fine" because
the folder is doing exactly what it's configured to do — but the
configuration violates the operator's intent. Orca's job is to
encode the intent.

### 2.5 What this does *not* cover

- Inner-container service health (e.g. sonarr's own HTTP /api/health).
  That's [[observability.md]] territory.
- Host-level OOM, kernel panics, hardware failure — orca needs the
  containers/mounts reconcilers up first; host self-healing is Phase 3.
- LXC/VM restart on stale mount — covered by
  [[lxc-vm-reconciler.md]]; the mounts plugin emits the trigger,
  lxc-vm-reconciler executes.

---

## 3. Migration from the band-aid

The 2026-06-12 band-aid ([[project-self-healing-freyr-baldur]])
ships `/usr/local/sbin/orca-watchdog.sh` + crontab on freyr+baldur.
It deliberately:

- Auto-starts containers per the rules in §2.1 (safe).
- Logs stale mounts only — does not remediate (would need §2.2's
  dep graph to be safe).

Cutover sequence when §2.1 + §2.2 ship:

1. Land §2.1 in `projects/containers/`. Roll out to freyr+baldur.
   Verify the plugin catches the same `Created` cases the band-aid did
   for 2 weeks side-by-side.
2. Remove the container-reconcile half of the band-aid script.
3. Land §2.2 in `projects/mounts/`. Roll out. Verify dep-graph is
   accurate against the live bind-mount inventory on freyr.
4. Remove the stale-probe half of the band-aid; remove crontab; remove
   `/usr/local/sbin/orca-watchdog.sh`; remove `/var/log/orca-watchdog.log`.

The band-aid is intentionally cheap to remove — single script, single
crontab line — so steps 2 and 4 are one ssh each.

---

## 4. Work breakdown

| ID | Slice | Size | Depends on |
|----|-------|------|---|
| C1 | `projects/containers/` skeleton + plugin registration | S | plugin-architecture |
| C2 | docker inspect collector (RestartPolicy + State per container) | S | C1 |
| C3 | Reconciler: auto-start safe cases; notify | M | C2, notifications |
| C4 | Crashloop classifier + pending-change emission | M | C3 |
| M1 | `projects/mounts/` skeleton + plugin registration | S | plugin-architecture |
| M2 | Probe loop (nfs/nfs4/autofs) with per-mount isolation | M | M1 |
| M3 | Dependent-graph builder (docker + LXC + systemd + VM) | L | M2 |
| M4 | Remediation planner + `orca apply` integration | L | M3 |
| M5 | Soft-mount default + drift detection vs declared mount spec | M | M2, storage-shares |
| R1 | `projects/replication/` skeleton + syncthing API client | S | plugin-architecture |
| R2 | Per-folder probe (connections, db/status, errors) + alerting | M | R1, notifications |
| R3 | Invariant enforcement: sendreceive on all peers, staggered versioning, subdir-per-host layout, no orphan devices | M | R2 |
| R4 | `orca replication add` bootstrap helper (symmetric-peer template) | M | R3 |
| R5 | Failover handoff: probe lag, sign off to mounts reconciler that peer-N takes over writes | M | R3, M4 |
| X1 | Cutover from band-aid (drop watchdog.sh + crontab on each host) | S | C3 + M4 live for 2 weeks |

---

## 5. Cross-references

- [[storage-shares.md]] — declarative share spec the mounts
  reconciler consumes.
- [[lxc-vm-reconciler.md]] — LXC/VM dependents of a stale mount go
  through that reconciler's restart path, not raw `pct restart`.
- [[notifications.md]] — dispatch surface for all three event classes.
- [[observability.md]] — inner-service health rollup that complements
  the container State check.
- [[plugin-architecture.md]] — `containers/`, `mounts/`, and
  `replication/` all follow the host-plugin contract.
- [[backup-restore.md]] — replication invariants feed the unified
  backup framework's "is my backup actually a backup?" check.
