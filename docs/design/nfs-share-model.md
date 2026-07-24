# NFS share model — retire autofs, orca-managed native mounts

Status: in progress (branch `nfs-share-model`). This is PR 1 (orca core). PR 2 is
the net-new `argyle-labs/nfs` plugin.

## Why

The storage-mount API is per-host-local and drifts: the same two shares are
registered three different ways across the fleet (`pool-*` on freyr, `willow-*`
on loki, bare on baldur), each re-declaring `source` + `failover_sources` +
the full option string. loki silently lost `softreval,nconnect=4,actimeo=30`
from a hand-edited option string — nothing validated or defaulted it.

autofs itself keeps failing, and the reason is telling: orca already **disables
autofs's two headline features** — `timeout=0` kills idle-unmount, and
`map_line_for` pins a single elected source (orca owns source election), so the
multi-location failover is bypassed too. We pay for `automountd` + the
bind-vs-idle races for features we don't use, on top of re-implementing health
probing, election, and stale recovery ourselves.

## Principles (decided)

1. **orca core is generic; NFS/SMB are separate plugins** (`argyle-labs/nfs`,
   `argyle-labs/smb`). Core has zero protocol-specific option code.
   `mount(2)` is fstype-agnostic, so core mounts *anything* given
   `(source, target, fstype, options_string)`.
2. **Mesh data is eventually consistent.** Fleet-scoped state replicates
   pod-wide (reuse the `host_status_replica` subscription/watermark pattern);
   each node converges its own slice. No per-host islands (the drift bug), no
   central SPOF.
3. **Two entities, not one flat row.**
   - **Share** (pod-wide, replicated): the NFS share itself, defined once.
   - **Mount** (per-host desired placement, replicated): "host X mounts share
     Y at target Z." Each host materializes only its own rows.
4. **Options are a typed, self-documenting object** owned by the plugin;
   rendered to the kernel option string at the edge. ms per the ms rule.
5. **uuidv7 identity** for every Share and Mount row; `name` is descriptive.
6. **Render at declare time, apply to the host** — the plugin renders the
   concrete option string when a mount is authored; core stores desired state
   and materializes it on the host via a native mount. No rendered string kept
   as central data.

## Data model (core, generic)

Both tables replicate pod-wide, LWW-converged on `updated_at`.

### `shares`
| col | type | note |
|---|---|---|
| id | uuidv7 | identity |
| name | text | canonical role: `data` / `backups` / `downloads` |
| backend | text | `nfs` (which plugin owns rendering/validation) |
| fstype | text | `nfs4` |
| sources | json | ordered `["host:/export", …]`, index 0 = primary, rest failover |
| options | json | opaque to core; the plugin's typed option object |
| options_rendered | text | the kernel option string the plugin produced at declare time |
| credential | text (secret) | optional SecretRef |
| updated_at | int | LWW clock |

`options` (typed, for edit/round-trip) + `options_rendered` (what core feeds
`mount`) travel together; core never interprets `options`, only emits
`options_rendered`.

### `mounts`
| col | type | note |
|---|---|---|
| id | uuidv7 | identity |
| share_id | uuidv7 | → shares.id |
| host | text | peer_id this placement targets |
| target | text | absolute mountpoint |
| enabled | bool | |
| remount_policy | json? | per-placement (not share) |
| updated_at | int | LWW clock |

## Apply / maintain (core, generic)

Replace autofs entirely.

- **Convergence loop** (grow `storage_selfheal` from "recover stale only" into
  the lifecycle owner): every tick, for the mounts assigned to *this* host —
  ensure present + healthy. Missing → mount. Stale/unreachable → remount,
  advancing through `sources[]` (primary → failover). Disabled/removed →
  unmount. This is the single owner of on-host mount state.
- **Native mount**, root side only. The existing privilege boundary stays:
  unprivileged daemon plans, `sudo -n orca admin storage-apply` executes. The
  root helper's action changes from "write auto.orca + restart autofs" to
  `mount` / `umount`.
  - Mechanism: exec the host's native `mount`/`umount`
    (`mount -t <fstype> -o <opts> <source> <target>`). Robust, portable, uses
    the kernel's own NFS mount helper (handles NFSv4 negotiation), trivially
    loggable. `nix::mount(2)` is a viable pure-syscall alternative (sources are
    raw IPs + pinned `vers`, so the negotiation `mount.nfs` does is
    unneeded) — isolated in the applier, easy to swap.
  - Path/target allowlist stays (traversal-proof), same security posture.
- **No `auto.orca`, no `automountd`, no `timeout=0`.**

## Plugin boundary (`argyle-labs/nfs`, PR 2)

- Owns `NfsOptions` (typed: `version` enum, `failureMode` soft|hard,
  `revalidateOnSoftError`, `timeout` ms, `retransmits`, `connections`,
  `attributeCache` ms, `extra[]`) with canonical `Default`.
- Renders `(fstype, options_string)` + validates at declare time; writes the
  Share/Mount desired state into core (on the target host's daemon).
- Core calls nothing NFS-specific at apply time — it emits `options_rendered`.

## Migration (gated, not auto-applied)

Collapse the live per-host regs into pod-wide shares + per-host mounts:
- `data` — sources `[10.10.10.10:/mnt/user/data, 10.10.10.11:/mnt/user/data]`
  (willow → maple, both Syncthing-replicated). Mounts on freyr/loki/baldur at
  `/mnt/data`.
- `backups` — same shape at `/mnt/backups`.
- `downloads` — **single source** `[10.10.10.10:/mnt/user/downloads]`, **no
  maple failover** (verified not Syncthing-replicated). Mount on freyr only.

Applying to the fleet is a separate, explicitly-gated step.

## Sequencing (PR 1)

1. Generic `mount`/`umount`/health native applier module (self-contained). ← start
2. Replicated `shares` + `mounts` tables (host_status_replica pattern, LWW).
3. Convergence loop (grow storage_selfheal).
4. Swap the root helper from autofs-write to native mount; retire autofs render.
5. Surface (`storage_share.*`, `storage_mount.*`), OpenAPI.
6. Migration tool (gated).
