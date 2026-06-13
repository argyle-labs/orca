# NFS re-export resilience

Capabilities orca needs to handle the Unraid SHFS + mover + NFS
re-export ESTALE cascade class. Grounded in the 2026-06-12 freyr
incident (see memory `project-unraid-mover-estale-cascade`).

> Scope detail for **Phase 1.1** alongside
> [`self-healing-reconciler.md`](self-healing-reconciler.md) and
> [`storage-shares.md`](storage-shares.md). This doc adds the
> reexport-gateway role + the SHFS class of failures those two
> reference but don't fully specify.

## The class of failure

A gateway host (e.g. tyr) NFS-mounts an Unraid SHFS share
(`/mnt/user/X`) and re-exports it to the fleet. Unraid's mover
periodically migrates files between the cache pool and the array.
SHFS rewrites the underlying inode/fileid on every move. The
gateway's cached file handles become invalid; its kernel emits
`NFS: server <ip> error: fileid changed`; downstream clients see
ESTALE on `stat` and never auto-recover. Every container with a
bind under the affected mount silently breaks until the gateway-
side mount is forcibly recycled.

Observed in this incident:
- ~12s lag between mover run and downstream ESTALE.
- Without intervention: indefinite outage (mount stays stale).
- With watchdog band-aid: ~71s recovery (60s detection + serial
  container restart + remount).
- `noac` on the gateway upstream **does not** prevent it — it
  reduces frequency but the SHFS fileid mutation is structural.

This is the supported failure mode of NFS-reexporting any
SHFS-backed Unraid share. It will recur on every mover cycle.

## R1. Reexport-gateway role

orca's [`storage-shares.md`](storage-shares.md) topology already
models share *consumers* and *exporters*. Add a third role:
**reexport-gateway** — a host that consumes upstream NFS and
re-exports to the LAN.

Declared on the gateway host:
```yaml
storage.reexport:
  upstream:
    - source: 10.10.10.10:/mnt/user/data
      mount: /srv/pool/data
      kind: unraid-shfs              # <-- new
      options: [vers=4.2, soft, softreval, nconnect=16, noac]
  exports:
    - path: /srv/pool/data
      to: [10.10.10.0/24, 100.64.0.0/10]
      fsid: 11
      options: [rw, async, all_squash, anonuid=99, anongid=100]
```

The `kind: unraid-shfs` flag tells orca that fileid mutation is
expected and steers the rest of the stack accordingly.

## R2. Gateway-side fileid-mutation detection

A `projects/mounts/` probe on the gateway tails kernel ring for
`NFS: server * error: fileid changed`, attributes events to the
matching upstream by `fsid:devid`, and emits a structured event:

```json
{ "type": "nfs.fileid_mutation",
  "gateway": "tyr",
  "upstream": "10.10.10.10:/mnt/user/data",
  "fsid": "0:55",
  "count_60s": 3 }
```

This is the canonical *trigger* — the gateway sees it before any
downstream client does. orca uses it to pre-warn downstream
reconcilers (R3) so they can shorten their detection latency from
~60s (cron-tick polling) to ~1s (push event).

## R3. Downstream pre-warned remediation

When a downstream `projects/mounts/` reconciler receives an
upstream `nfs.fileid_mutation` event for a mount it depends on,
it begins **eager remediation** without waiting for its own
ESTALE probe:

1. Acquire mount lock (flock equivalent).
2. Stop dependent containers **in parallel**, not serially. (8
   containers × 3s serial = 24s; parallel = ~3s.)
3. `umount -lf`; re-mount with original opts.
4. Start dependents in parallel.
5. Release lock.

Target downtime budget for the SHFS-mover class: **≤ 15s** from
upstream event to all dependents healthy. (vs ~71s with the
band-aid watchdog.)

Dependency graph: same one storage-shares.md / self-healing-
reconciler.md already specify (`bind source ↦ container set`).

## R4. Mover-window awareness

The gateway already knows it's serving an `unraid-shfs` upstream
(R1). It can query the upstream's mover schedule (read
`/boot/config/share.cfg` via the willow plugin) and:

- Surface the schedule as a planned outage window: `next mover
  cascade at 04:00 ±90s`.
- Optionally **pre-stop** writers (sabnzbd / qbit) ~5s before
  the scheduled mover run and resume after the cascade clears.
- Refuse to schedule overlapping orca-owned maintenance during
  that window.

This converts an unavoidable structural cascade into a known,
declared, brief outage instead of a silent recurring failure.

## R5. Configuration linter

orca's `system_detail` / preflight checker should flag:

- A mount with `kind: unraid-shfs` re-exported without `noac` on
  the gateway: **error** (noac is necessary even if not
  sufficient; without it, cascades hit ~10×/day instead of 1×).
- Hourly mover schedule (`shareMoverSchedule="0 */1 * * *"`)
  upstream of a reexport gateway: **warn** with recommendation
  to drop to off-peak frequency. Hourly mover + reexport = 24
  ESTALE cascades/day, a known anti-pattern.
- A mount with `kind: unraid-shfs` *consumed directly* by any
  fleet host (not via the gateway): **error**. Bypassing the
  gateway sacrifices the R3 pre-warned recovery and forces every
  consumer to run its own cascade-handling logic.

## R6. Escalation: bypass SHFS

For workloads where the R3 ≤15s cascade is still unacceptable,
support upstream-side mitigations:

- **`/mnt/user0` instead of `/mnt/user`**: array-only union, no
  mover involvement, stable fileids. Loses cache-write benefit.
- **`/mnt/disk*` per-disk mounts**: bypass SHFS entirely. Needs
  disk inventory and per-disk mountpoint plumbing on the gateway.

The reexport-gateway config supports a per-upstream
`bypass: user0 | disks | none` switch. orca's storage manager
handles the remount on the gateway. Default = `none` (R3 is good
enough for typical *arr workloads).

## Work breakdown

| ID | Scope | Size |
|----|-------|------|
| R1 | Add `reexport-gateway` role + `kind: unraid-shfs` to storage-shares config schema | S |
| R2 | Gateway-side `dmesg` tail → `nfs.fileid_mutation` event in `projects/mounts/` | M |
| R3 | Downstream pre-warned remediation (event → parallel-stop → umount → mount → parallel-start) | M |
| R4 | Mover schedule introspection + pre-stop scheduler hook | M |
| R5 | Preflight lint rules (`noac` required, hourly-mover warn, direct-consume reject) | S |
| R6 | Gateway-side bypass switch (`user0` / `disks`) + remount orchestration | L |

R1–R3 retire the freyr/baldur band-aid watchdog
(`/usr/local/sbin/orca-watchdog.sh`); they're the minimum bar to
remove it.

## Cross-references

- [`self-healing-reconciler.md`](self-healing-reconciler.md) §2.2
  defines the generic mounts reconciler; this doc specifies the
  SHFS class within it.
- [`storage-shares.md`](storage-shares.md) defines the share
  topology model; this doc adds the reexport-gateway role.
- `project-unraid-mover-estale-cascade` (memory) — the 2026-06-12
  incident this plan is grounded in.
- `project-self-healing-freyr-baldur` (memory) — band-aid that
  R1–R3 retire.
- `project-orca-failover-nfsv4-stale-handle` (memory) — orthogonal
  failover-class stale handle issue; same plumbing (mount lock,
  dependent graph) but different trigger.
