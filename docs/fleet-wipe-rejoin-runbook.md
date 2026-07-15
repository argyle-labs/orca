# Runbook: fleet wipe-and-rejoin onto clean UUIDv7 identities

This is the one-time collapse that re-keys every host onto a genuine minted
UUIDv7 identity (the [id hard rules](#the-id-hard-rules)). It is destructive to
mesh trust by design — you tear the pod down and rebuild it — so run it as a
coordinated fleet operation, not host-by-host over time.

## The id hard rules

- Every id is a **minted UUIDv7**. No truncation, no `/etc/machine-id` bare-hex,
  no derived/composed ids.
- **No prefixes.** Never `system:` / `local:` / `peer.` / `claim:…`. An id is
  just the UUIDv7.
- The whole tree is **knowable and walkable by id**. Peers and every claimed
  child (VM/LXC/container/stack) carry a stable UUIDv7; you can paste any id
  from the tree back as a selector.
- Natural fields (provider, hostname, native id, mac, …) are **searchable
  attributes** used to find/correlate a node — they are never baked into the id.

## Why a full collapse (not a rolling re-key)

Two facts from the leave/join/recover paths make a rolling re-key unsafe:

1. **`pod leave` preserves local identity.** It deletes `pod_peers` / `pod_trust`
   / offers / discovery and clears `pod_self`, but it does **not** delete the
   on-disk `machine_id` file or the bootstrap key. So a host that merely leaves
   and rejoins comes back with the **same** id. To mint a fresh UUIDv7 you must
   delete `<app_dir>/machine_id` so `load_or_generate_machine_id` re-mints.
2. **No auto-drop of a stale peer on rejoin.** When a host returns with a *new*
   id, other hosts do not automatically retire its old `pod_peers` row — there
   is no "same machine, new id" reconciliation (the old id is, by definition, a
   different key). A rolling re-key therefore leaves a **ghost row** on every
   other peer, cleaned only by an explicit `pod forget <old_id>` fan-out.

Collapsing the whole pod at once sidesteps both: every host wipes its identity
and roster together, so there are no ghost rows to chase.

## Preconditions

- The UUIDv7 identity build is published as an rc and **already deployed to
  every host** (over the mesh — see [mesh self-update](force-update-runbook.md)).
  Re-keying requires the new `load_or_generate_machine_id`; do the code rollout
  first, the identity collapse second.
- You have a way to run commands on each host (mesh `pod/exec`, or local shell).
- Pick one host to be the **first inviter** (the seed the others re-pair to).

## Procedure

For each host, `<app_dir>` is the orca data dir (`/var/lib/orca/.local/share/orca`
on Linux service installs; `pod_detail` / logs show the resolved path).

1. **Leave the pod** (best-effort broadcast; ignore failures — the pod is coming
   down anyway):
   ```
   orca pod leave
   ```
2. **Stop the daemon** so nothing rewrites identity mid-wipe:
   - systemd: `sudo systemctl stop orca`
   - OpenRC: `sudo rc-service orca stop`
3. **Delete the persisted identity** so a fresh UUIDv7 is minted on next start:
   ```
   rm -f <app_dir>/machine_id
   ```
   Optionally also rotate the bootstrap key (regenerates the pinned fp) if you
   want a fully fresh trust anchor; leave it in place to keep re-pairing cheap.
4. **Start the daemon.** On boot, `host_identity::init` mints + persists a new
   UUIDv7; verify:
   ```
   orca pod detail    # note the new peer_id — must be a dashed UUIDv7
   ```
5. **Re-pair.** Bring up the seed host first, then join each other host to it via
   the bootstrap accept flow:
   - Seed offers a code (`orca pod` invite/offer path).
   - Joiner: `orca pod join --action accept --code <6-char>`.
   The joiner's CSR CN is now its full UUIDv7; the inviter signs it and writes
   the `pod_peers` row keyed by that UUIDv7.

## Verification (whole fleet)

- `pod_list` / `pod_instances` on any host: every `peer_id` and every `id` is a
  bare dashed UUIDv7 — no `system:` / `local:` / `peer.` prefixes, no bare-hex.
- `inventory.tree` / `network_topology_view`: every node id (peers **and** claim
  children) is a bare UUIDv7; parent↔child is walkable purely by id.
- Pick a claim node id from the tree and pass it back to `inventory.detail` —
  it must resolve (round-trippable selector).
- Targeting works by id: `system_update(peer=<uuidv7>)` resolves on every host.
- No ghost rows: `pod_instances` shows no `departed`/stale duplicates.

## Restore / resiliency notes

- **Single-host loss (not a collapse):** if one host is rebuilt and rejoins with
  a fresh id, the others keep its old row. Run `pod forget <old_id>` (fan-out) to
  purge it, or `pod recover <id>` if a live peer was wrongly marked departed.
- **Identity is the source of truth on disk:** `<app_dir>/machine_id` is the only
  thing that pins a host's id across restarts. Back it up if you want a host to
  keep its UUIDv7 across an OS reinstall; delete it to intentionally re-key.
- **Claim ids survive restarts** via `db::claim_identity` (minted once, keyed by
  the natural attributes). Wiping the DB re-mints them — expected during a
  collapse, since we backfill fresh and drop the old `claim:` ids.
