# Backup + restore — unified story

> **HARD RULE — user-triggered changes only.** Orca detects drift,
> notifies, and waits. The user runs `orca apply <change-id>` (or
> accepts a UI prompt). No reconciler ever auto-applies. Scheduled
> backup *jobs* are user-declared schedules (still operator-defined);
> *restores* are always explicit verbs. See ROADMAP §1.11.

One backup story across two concerns that historically lived
separately:

1. **Managed-service backups** — what the meerkat `backup-configs.sh`,
   `pbs-backup-hook.sh`, `restore-config.sh`, and per-host
   `backup-appdata.sh` scripts do today. Becomes orca verbs.
2. **Orca's own state** — config store SQLite, secrets store SQLite,
   pod CA + revocation set, materialized config-repo checkout,
   audit DB, metrics/logs DBs (selectively). Currently not backed
   up at all. Gap.

Both flow through the same orca-backup framework. Same verbs, same
storage targets, same restore path. No second backup system.

This doc supersedes scattered references in
[orca-as-logic-layer.md](orca-as-logic-layer.md) §3.3 and the gap-3/gap-5
notes that were previously stubbed. The homelab-specific instances
(which exact paths to back up, retention windows, offsite targets)
stay in the consuming config repo — see meerkat's
`docs/planned/offsite-backup.md` and `docs/backup-gaps.md` as the
canonical example.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Goals

- **One framework**: a single orca subsystem handles "make a backup,"
  "list backups," "verify a backup," "restore from a backup," for
  every concern.
- **Declarative job definitions**: backup jobs live in the config
  repo as TOML, reconciled by the scheduler. Adding a backup is a
  PR, not a cron edit.
- **Pluggable targets**: local disk, NFS share, PBS, S3-compatible,
  rsync.net, ssh+rsync, restic/borg repos. Add a target =
  implement a trait.
- **Pluggable sources**: docker volumes, service appdata
  directories, SQLite DBs (with `.backup` semantics), Proxmox
  guests (via PBS API), Unraid shares, orca's own state.
- **Survives loss of any one host**, including baldur.
- **Verifiable**: every backup carries a manifest with file count,
  total bytes, content hashes per file. Verify reads the manifest
  and checks; doesn't require a full restore.
- **Restore is a first-class verb**, exercised regularly.

---

## 2. Backup job model

`compose/backups/jobs/<name>.toml` (or wherever the config repo
puts them):

```toml
[[job]]
name = "appdata-baldur"
host = "baldur"                 # where the source lives
schedule = "0 3 * * *"          # cron expression; uses orca scheduler

[job.source]
kind = "docker_volumes"         # source plugin id
include = ["immich-pgdata", "navidrome-config", "..."]
exclude_glob = ["*/cache/*", "*/tmp/*"]
pre_hook = "orca docker stop --label backup=stop"
post_hook = "orca docker start --label backup=stop"

[job.target]
kind = "pbs"                    # target plugin id
endpoint = "pbs.scottkey.me"
datastore = "main"
namespace = "baldur"
retention = { keep_daily = 7, keep_weekly = 4, keep_monthly = 12 }

[job.verify]
on_success = true               # verify after each backup
on_schedule = "0 6 * * 0"       # extra deep-verify weekly

[job.notify]
on_success = "ntfy:backups"
on_failure = "ntfy:alerts"
```

Job runs:

1. Resolve source and target plugins.
2. Run pre-hook (with timeout).
3. Source plugin emits a stream of (path, bytes, metadata) records.
4. Target plugin consumes the stream, writes the backup, returns a
   manifest.
5. Run post-hook.
6. Optionally verify.
7. Apply retention policy on the target.
8. Notify.

Failures at any step: post-hook still runs (cleanup is not
optional), manifest marked failed, alert fires, **previous successful
backup is not touched**.

### 2.1 Source kinds — native APIs first

**Hard rule** (see `feedback_native_backup_apis`): where a service
exposes a native backup API, use it.
Don't tar the docker volume. Native backups are app-consistent
(the service flushes its own buffers, closes its own write
transactions, knows which paths are throwaway caches), produce a
clean restorable archive, and are the path the upstream maintainers
actually test against. Falling back to volume-tar gives you a
crash-consistent snapshot of half-written SQLite WAL files and a
restore procedure nobody has rehearsed.

Hierarchy of source kinds, in priority order:

| Tier | Kind | When to use |
|---|---|---|
| 1 — Native API | Service has its own backup endpoint that produces a restorable archive | Always prefer when available |
| 2 — DB dump | Service uses Postgres/MySQL/SQLite and exposes credentials | Use when no native API but the DB is the only stateful surface |
| 3 — App-aware filesystem | Service documents which paths must be backed up consistent (e.g., quiesce-then-snapshot) | When neither of the above and the service docs are explicit |
| 4 — Volume tar | Everything else | Last resort. Stop the container first; back up; start. |

#### Native API sources to implement

| Source kind | Services | API shape |
|---|---|---|
| `arr_native` | sonarr, radarr, lidarr, prowlarr, readarr, whisparr | `POST /api/v3/system/backup`, then `GET /api/v3/system/backup` to list, `GET /api/v3/system/backup/<id>/download` to fetch. Returns a zip. |
| `home_assistant_snapshot` | Home Assistant (Supervised / OS) | `POST /api/hassio/backups/new/full` (or partial), poll `GET /api/hassio/backups/<slug>` for ready, download `GET /api/hassio/backups/<slug>/download`. |
| `zwave_js_dump` | zwave-js-ui, zwave2mqtt | NVM backup endpoint; produces a binary blob restorable to a fresh controller. |
| `zigbee2mqtt_backup` | zigbee2mqtt | MQTT request `zigbee2mqtt/bridge/request/backup`; response carries a base64 payload of the coordinator NVRAM + config. |
| `unifi_controller_backup` | UniFi network controller | `POST /api/s/<site>/cmd/backup`, fetch the generated `.unf` file. |
| `pbs_remote` | Proxmox Backup Server itself | PBS already produces the backups; this source copies snapshots between PBS datastores via the native sync-job API. Don't tar the PBS datastore. |
| `proxmox_vzdump` | Proxmox VMs / LXCs without PBS | `POST /nodes/<node>/vzdump` with target storage; pulls a `.vma` / `.tar` per guest. |
| `unraid_flash_backup` | Unraid OS itself (USB flash config) | Unraid GraphQL exposes the flash backup endpoint; produces a zip of `/boot/config`. |
| `paperless_export` | Paperless-ngx | `document_exporter` management command; produces a restore-via-`document_importer` archive that survives major-version bumps. |
| `vaultwarden_export` | Vaultwarden | `db.sqlite3` + attachments dir, but the supported path is the export endpoint that handles WAL checkpoint. |
| `gitea_dump` / `forgejo_dump` | Gitea, Forgejo | `gitea dump` (or REST equivalent) emits a single tarball with DB + repos + LFS. |
| `nextcloud_occ_export` | Nextcloud | `occ maintenance:mode --on` → DB dump + data dir → `occ maintenance:mode --off`. |
| `audiobookshelf_backup` | audiobookshelf | Has a native backup endpoint that emits a zip of the SQLite DB + metadata; user libraries stay on disk. |
| `immich_db_dump` | immich | Documented procedure: `pg_dump` the immich DB + filesystem snapshot of upload paths. (Native one-shot backup endpoint hasn't landed upstream as of last check — track and migrate when it does.) |
| `pg_dump` / `mysql_dump` / `sqlite_backup` | Generic DB sources | For any service whose only state is "the DB" and which doesn't ship a higher-level backup endpoint. |

#### When native APIs aren't enough

Some services keep state outside the API's reach: large media
libraries, user-uploaded blobs, generated thumbnails. A complete
backup is usually **native API archive + an asset-tree backup** of
the data the API doesn't cover. The job model handles this as two
sources in one job:

```toml
[[job]]
name = "immich-full"
host = "baldur"
schedule = "0 3 * * *"

[[job.source]]
kind = "immich_db_dump"           # tier 1: native DB dump

[[job.source]]
kind = "directory"                # tier 4: complement for asset blobs
path = "/srv/immich/library"
exclude_glob = ["*/thumbs/*", "*/encoded-video/*"]

[job.target]
kind = "replicated_nfs"
storage = "primary-nas"
```

Restore reverses the order: DB first, then asset tree, then service start.

#### Per-service plugins, not a giant match statement

Each native-API source is implemented as a small adapter behind
the `Source` trait. They live in `integrations/<service>/backup.rs`
co-located with the rest of that service's integration code, not
in a single mega-module. This keeps the surface manageable as new
services land and means a service's backup logic is owned by
whoever owns its integration.

#### Restore parity is mandatory

A native-API backup is only worth taking if you've **proven you
can restore it**. Each native source ships with:

1. The complement restore verb that consumes its archive shape.
2. A restore drill fixture (per §7) that creates a minimal target,
   restores, and asserts a known invariant (count of items, hash
   of a known record, smoke-test of an endpoint).
3. Documentation of the restore prerequisites (target service
   version compatibility — most native restores are
   forward-compatible within a major version but not always).

A source kind that produces an archive nobody knows how to restore
is worse than no backup at all; it gives false confidence.

---

## 3. Concern 1: managed-service backups (meerkat scripts → orca verbs)

The 9 backup-relevant scripts in meerkat (verified 2026-06-01):

| Meerkat path | Replacement | Notes |
|---|---|---|
| `scripts/backup-configs.sh` | `[[job]] kind = "config_snapshot"` source + `kind = "git_commit"` target | Currently writes into `backups/configs/` and the repo. Keep the on-disk layout exactly; parity-check per [schema-evolution.md](schema-evolution.md). |
| `scripts/restore-config.sh` | `orca backup restore --job <name> --from <snapshot>` | Same flag shape it had as a script. |
| `scripts/pbs-backup-hook.sh` | `[[job]] target = "pbs"` with pre/post hooks (§2) | **Needs `projects/plugins/pbs/`** — there is no PBS plugin in orca today. New plugin work. |
| `scripts/pbs-mount-watchdog.sh` | Folds into the NFS client reconciler in [storage-shares.md](storage-shares.md) — not a backup job per se, but it gates whether backup jobs can run. |
| `scripts/baldur/backup-appdata.sh` | `[[job]] kind = "docker_volumes"` on baldur | Native-API-first per [§2.1]; only the volumes that lack a native endpoint stay on docker-volume tar. |
| `scripts/freyr/backup-appdata.sh` + the `scripts/freyr/{backup,restore,update,restart}` wrappers | `[[job]]` per service; the arr stack uses `arr_native` (§2.1) not volume tar | Wrappers retire when verbs cover the surface. |
| `scripts/thor/backup-zigbee2mqtt.sh` | `[[job]] kind = "zigbee2mqtt_backup"` (native source — see §2.1 table) | |
| `scripts/maple/appdata-backup-chown.sh` + `scripts/maple/fix-backup-ownership.sh` | `[job.source.permission_fix]` option (folds into `utils/fs/perms`) | |
| `scripts/willow/appdata-backup-chown.sh` + `scripts/willow/fix-backup-ownership.sh` | Retire with willow (2026-06-01 cutover); for maple keep the same successor as above. |

### 3.1 New work: `projects/plugins/pbs/`

PBS integration is **not** shipped — `projects/plugins/` has docker,
dockge, nfs, smb, ntfy, proxmox, unraid, homeassistant, arr, graphql,
openapi, llm, agents, runtime, mcp, db, but no `pbs`. The PBS
backup-hook flow above depends on it. New plugin work:

- `pbs.datastore.list` / `pbs.datastore.detail`
- `pbs.snapshot.list` / `pbs.snapshot.delete`
- `pbs.sync_job.list` / `pbs.sync_job.run`
- `pbs.hook.{pre,post}` (the hook surface this doc consumes)
- `pbs.client.install` (host-side: install PBS client + key material)

Lives under `projects/plugins/pbs/` alongside the other integrations.

### 3.2 Namespace — `backup_job`

The config-store noun for jobs in this doc is **`backup_job`**, not
`job`. `projects/server/src/jobs/` is already taken by **agent
execution** (chat+tool loop). Two unrelated concepts; pick distinct
names. `[[backup_job]]` in TOML, `system.backup.job.*` in tool defs.

Each retirement follows the [schema-evolution.md](schema-evolution.md)
parity workflow. Default operational cycle for backups is **two
nightly runs** of dual-run before shadowing the old script.

---

## 4. Concern 2: orca's own state

The state orca depends on to function. If baldur dies and you stand
up a new baldur, what does the new daemon need to come back to the
same pod identity, same routes, same secrets?

### 4.1 What must be backed up

| Source | What | Why | Sensitivity |
|---|---|---|---|
| `${ORCA_DIR}/config.db` | Config store (routes, schedules, host records, plugin registrations) | Recreating from the config repo is partial — runtime overlay is lost. | Medium |
| `${ORCA_DIR}/secrets.db` | Secrets store (encrypted SQLite) | Without this, every integration credential has to be re-entered. | Highest — encrypted at rest, restoring needs the master key |
| `${ORCA_DIR}/pki/` | Pod CA private key + cross-sign material (founding peer only) | Lose the CA key and you cannot rotate certs, pair new peers, or revoke. Pod must be rebuilt. | Highest |
| `${ORCA_DIR}/audit.db` | Audit log | Compliance + forensics. 1-year retention by policy. | Medium |
| `${ORCA_DIR}/repos/<id>/` | Materialized config-repo working tree | Recreatable from the git provider, but useful for fast restore + offline starts. | Low |
| `${ORCA_DIR}/metrics.db`, `logs.db` | Time-series data | Useful for postmortems; loss is annoying but not fatal. | Low (selective) |

### 4.2 Backup mechanics

- SQLite DBs backed up via `VACUUM INTO` or the online `.backup` API
  to avoid corruption during writes.
- Secrets DB is **never** decrypted during backup — the encrypted
  blob is what gets stored. The master key must be backed up
  *separately* (different target, ideally air-gapped).
- CA key on the founding peer: the highest-stakes item. Default
  backup target is a hardware token (if available) plus an
  air-gapped encrypted copy. Never lives only on baldur's disk.
- Audit DB has its own retention floor (per
  [observability.md](observability.md) §4.3) — backup honors that.

### 4.3 Built-in job: `orca-self-backup`

Ships with orca, auto-enabled at install if a target is configured.
Source: `kind = "orca_state"` (collects the above). Default cadence:
hourly for config + secrets, daily for everything else.

### 4.4 Key inventory + escrow

**Hard rule**: every encryption key orca generates is exportable
to a recovery vault. If something is encrypted, there is a known
recovery path to decrypt it that doesn't depend on the originating
host being alive.

The keys orca tracks:

| Key | Purpose | Lost = |
|---|---|---|
| Secrets-store master key | Encrypts the secrets SQLite | Every integration credential lost — unrecoverable without escrow |
| Pod CA private key | Signs peer certs | Cannot pair new peers or rotate certs; pod must be rebuilt |
| Revocation-signing key | Signs the revocation set | Manageable — re-issued by CA |
| Per-job offsite encryption keys | Encrypts backups before they leave the host | Every offsite backup unreadable — unrecoverable without escrow |
| Per-host disk-encryption passphrase (if managed) | Unlocks LUKS / equivalent on managed hosts | Host requires manual unlock at boot — recoverable but disruptive |
| TLS server keys (Caddy, etc.) | Public-facing TLS | Re-issued by ACME; not escrowed |

### 4.5 Escrow targets

The default and recommended target for single-operator pods is
**1Password**, accessed via the `op` CLI. Other targets are
supported for ops who want them.

| Target | When | How |
|---|---|---|
| **1Password** (default) | Single operator, 1P already in use | `op item create` per key, stored in a designated vault (`Private/orca-keys` by default). `orca` shells out to `op` with a short-lived service-account token. |
| Bitwarden | Same as 1P, different password manager | `bw` CLI equivalent. |
| Shamir secret share | Multi-operator pod, no shared password manager, or extra paranoia | Key split into N shares, M required to reconstitute. Shares written to physical media / distributed to operators. |
| Hardware token (YubiKey) | CA key especially | Key generated on-device where possible; otherwise key encrypted to a PIV slot. Pair with an escrow target above for the "lost the YubiKey" case. |
| Air-gapped encrypted file | Cold-storage fallback | Written to a USB drive, kept somewhere offline. Bootstrap an escrow even for keys that have other targets. |

**Multiple targets are encouraged.** A key escrowed only in 1P
fails if you lose 1P access. A key escrowed only on a YubiKey fails
if you lose the token. Default policy: at least two independent
targets per key, where one of them is an offline fallback.

> **Handle grammar.** All `op://` references in this section
> follow [`secrets-identity.md`](secrets-identity.md) §2.1 — three
> segments for 1Password (`vault/item/field`), no path-style
> namespacing. Per-host orca keys live as item `orca-keys.<host>`
> in the `Orca` vault; per-host backup keys as `backup-keys.<host>`;
> escrow shares as `escrow.<scope>` with field-per-share.

### 4.6 Escrow operations

```sh
# Manual export of a single key
orca pki key export --key secrets-master --to op://Orca/orca-keys.secrets-master/key
orca pki key export --key pod-ca         --to op://Orca/orca-keys.pod-ca/key

# Bulk export of all tracked keys (writes one item per key in the Orca vault)
orca pki key export --all --vault Orca

# Import / restore from escrow
orca pki key import --key secrets-master --from op://Orca/orca-keys.secrets-master/key

# Verify every tracked key has a current escrow entry
orca pki key audit
```

`audit` walks the key inventory and confirms each key has at least
one valid escrow entry that was last verified within a configurable
window (default 30 days). Stale or missing escrows fail loudly:

```
orca pki key audit
  ✓ secrets-master    op://Orca/orca-keys.secrets-master/key   (verified 3d ago)
                      shamir://operators/3-of-5                 (verified 14d ago)
  ✗ pod-ca            op://Orca/orca-keys.pod-ca/key            MISSING
                      yubikey://serial-12345/piv-slot-9c        (verified 31d ago, STALE)
  ✓ offsite-baldur    op://Orca/orca-keys.offsite-baldur/key    (verified 1h ago)
exit 1: 1 key has no valid escrow, 1 key has stale escrow
```

This is one of the checks the GitOps reconciler runs on every
apply; key-escrow gaps surface as alerts the same way overdue
backups do.

### 4.7 Re-encryption on key rotation

When a key rotates, the escrowed copy must update at the same
time. The rotation flow is:

1. Generate new key.
2. Re-encrypt the data the key protects with the new key.
3. **Update every escrow target** with the new key.
4. Verify escrow targets read back correctly.
5. Destroy old key.

A rotation that completes step 1-2 but fails step 3 leaves the
system unrecoverable. Rotation aborts on escrow-write failure and
rolls back to the previous key.

### 4.8 Per-store, independently backed up

The `orca_state` source breaks down into **independent data stores**,
each addressable by name. Backups, restores, and vault sync all
operate per-store; you can restore the secrets DB without touching
the config DB, push only a specific store to a vault, etc.

| Store | Default vault path | Notes |
|---|---|---|
| `config` | `op://Orca/backup-keys.<host>/config_db_enc` | Encrypted blob. |
| `secrets/store` | `op://Orca/backup-keys.<host>/secrets_db_enc` | Encrypted blob (master key escrowed separately, §4.4). |
| `secrets/items` | `op://Orca/backup-keys.<host>/secrets_items` | Per-secret items (see §4.9). |
| `audit` | `op://Orca/backup-keys.<host>/audit_db_enc` | 1-year retention floor. |
| `pki/ca` | `op://Orca/orca-keys.<host>-ca/key` | Founding peer only. |
| `pki/revocation` | `op://Orca/orca-keys.<host>-revocation/signed` | Replicated via mesh; vault copy is the fallback. |
| `repos/<id>` | (not vault) | Materialized config repo — recoverable from git, not escrowed. |
| `metrics`, `logs` | (not escrowed) | Time-series; expendable. |

Verbs operate per-store:

```sh
orca backup push  --store secrets/store --target op://Orca/backup-keys.<host>/secrets_db_enc
orca backup pull  --store secrets/store --from   op://Orca/backup-keys.<host>/secrets_db_enc
orca backup list  --store secrets/store
orca backup verify --store secrets/store
```

A `--store all` form bulk-applies for full-system DR.

### 4.9 Vault as a backup destination (not just escrow)

Vaults (1Password, Bitwarden) are first-class **backup targets**, not
only key-escrow targets. The same backend serves both concerns; the
distinction is just what gets written:

- **Escrow** writes a key (binary blob in an `op` item field).
- **Backup** writes an encrypted store blob (binary blob in a
  larger `op` item with metadata, manifest, timestamp).
- **Inventory export** (new, see §4.10) writes a *schema only* —
  the list of secret keys a host needs, no values.

A backup target is configured per host or per pod:

```toml
# config/<host>/backup.toml
[target.vault]
kind        = "1password"
vault       = "Orca"
item        = "backup-keys.maple"      # 3-segment grammar: op://Orca/backup-keys.maple/<field>
mode        = "automatic"        # automatic | manual | inventory-only
on_change   = true               # push when source store changes (with debounce)
schedule    = "0 */6 * * *"      # plus a periodic full push every 6h
```

`mode = "automatic"` means orca pushes encrypted blobs on its own
schedule. `mode = "manual"` requires `orca backup push` to fire.
`mode = "inventory-only"` is §4.10.

### 4.10 Inventory-only export — bootstrap an empty vault

The case the operator described: stand up a new host (e.g., maple)
that needs secrets, but the secret *values* don't exist yet. Orca
writes a **schema** to the vault listing every secret the host
needs, with empty placeholder values. Operator fills in the
values in 1Password. Orca then pulls back the filled-in values
into the local secrets store.

```sh
# 1. Orca writes the inventory schema to the vault
orca backup inventory export --host maple --to op://Orca/secrets-inventory.maple

# Created in 1Password (item `secrets-inventory.maple`, one field per secret):
#   field GITHUB_TOKEN          (empty)
#   field CF_API_TOKEN          (empty)
#   field UNRAID_API_KEY        (empty)
#   field PIA_USERNAME          (empty)
#   field PIA_PASSWORD          (empty)
#   field SMB_GUEST_PASSWORD    (empty)

# 2. Operator fills in values in 1Password by hand.

# 3. Orca pulls populated values into the local secrets store
orca backup inventory import --host maple --from op://Orca/secrets-inventory.maple

# Verifies each declared secret now has a value; reports gaps.
orca backup inventory verify --host maple
  ✓ GITHUB_TOKEN
  ✓ CF_API_TOKEN
  ✗ UNRAID_API_KEY    (empty in vault)
  ✓ PIA_USERNAME
  ✓ PIA_PASSWORD
  ✗ SMB_GUEST_PASSWORD (empty in vault)
exit 1: 2 secrets missing values
```

This is also the **bootstrap flow** for a fresh host: install
orca, point it at a vault, run `inventory export` once, fill the
values, run `inventory import`. Host is fully populated without
ever copying secrets via the shell.

The secret inventory is derived from the config repo: each
service / integration declares the secrets it needs (`requires =
["GITHUB_TOKEN"]`), and `inventory export` collects that list per
host.

### 4.11 Non-secret state snapshots → the config repo itself

The config repo (meerkat or equivalent) is git, and git is already
a versioned store. Use it. Every non-secret artifact orca generates
on a deploy gets committed back to the repo on a known branch, so
the repo holds both:

- **Desired state**: operator-authored TOML on `main` (PRs land here).
- **Realized state**: orca-authored snapshots on `state/` (committed
  by the daemon after a successful apply).

This makes the repo a complete picture of current deployed state —
rollback is `git revert`, audit is `git log`, diff-between-deploys
is `git diff`, no separate state-store to manage.

#### What gets snapshotted

Non-secret only. Anything sensitive (`/etc/shadow` hashes, API
tokens, the encrypted secrets blob) goes to vault / target, never
to the repo.

| Artifact | Source | Path in repo |
|---|---|---|
| Rendered Caddyfile | caddy plugin reconciler | `state/<host>/caddy/Caddyfile.generated` |
| Generated systemd units | host service reconciler | `state/<host>/systemd/*.service` |
| Generated cron / OpenRC scripts | same | `state/<host>/cron.d/` etc. |
| Resolved per-host package list | docker/nfs/pbs reconcilers | `state/<host>/packages.lock.toml` |
| Active fstab | NFS reconciler | `state/<host>/fstab` |
| Effective users (UIDs, groups, **no shadow**) | users reconciler | `state/<host>/users.passwd-like` |
| Caddy route table (resolved peer addrs) | caddy reconciler | `state/cluster/caddy-routes.toml` |
| Host inventory (peer_id, IPs, OS, capabilities) | mesh | `state/cluster/inventory.toml` |
| Per-host applied-config hash + orca version | reconciler | `state/<host>/applied.toml` |
| Backup manifests (file lists, sizes, hashes — no contents) | backup framework | `state/<host>/backups/<job>.manifest.json` |

Secrets-store inventory (key *names* only, no values — see §4.10)
also lands here under `state/<host>/secrets.schema.toml` so
operators can review what's expected without exposing values.

#### Commit behavior

- Orca commits to a `state/auto` branch (or per-host
  `state/<host>/auto`) — never to `main`. Operators can fast-forward
  `main` from state if they want the realized state to become
  desired (rare; usually you want desired ahead of state).
- Each commit message includes: orca version, peer_id, applied
  config SHA from `main`, the operation that triggered the commit,
  duration, outcome.
- Commits go through the git-provider API per the existing rule
  ([memory: git provider rule](feedback_git_provider_api.md)).
- Auto-versioning: commit message format includes a deploy
  sequence number per host (`baldur/deploy-00742`). Tags optional
  for milestone deploys.
- Squash policy: state branches grow forever otherwise. Periodic
  squash (default: monthly, configurable) collapses old per-deploy
  commits into a "month rollup" commit while preserving the
  reachable SHAs for the rollback window.

#### Where snapshots land — storage abstraction + git mirror

There are two kinds of artifact, and they go to different places:

1. **Lightweight non-secret configs** (rendered Caddyfile, generated
   systemd units, host inventory, applied-config hashes — the small
   text artifacts in the table above) → committed to the **config
   repo itself** on a `state/` branch. Git is the right tool for
   diff-able, versioned text.
2. **Bulk state** (encrypted data-store blobs, manifests with file
   hashes, larger generated artifacts) → written to a **storage
   target**, which is an abstract destination with replication
   enforced by orca.

Storage is the target. Hosts that provide storage (willow, maple,
…) are providers behind the abstraction, not direct targets — orca
picks where to write based on the storage definition and replicates
between replicas on its own. Switching from one provider to another
should not require touching every snapshot config.

```toml
# config/cluster/storage.toml
# Define logical storage pools once; everything else references them.

[[storage]]
name        = "primary-nas"
kind        = "replicated_nfs"
primary     = { provider = "willow", export = "/mnt/user/orca-state" }
replicas    = [
  { provider = "maple", export = "/mnt/user/orca-state" },
]
sync        = "continuous"        # continuous | scheduled
sync_rpo    = "60s"               # max staleness between primary + replicas
fail_over   = "automatic"         # if primary unreachable, promote a replica
verify      = "daily"             # checksum-compare primary vs replicas

[[storage]]
name        = "offsite"
kind        = "s3"
endpoint    = "s3.us-east-1.amazonaws.com"
bucket      = "orca-state"
prefix      = "scottkey-pod/"
encryption  = "per_object"        # uses the per-job offsite keys, §4.4

# Snapshot policy references storage pools by name.

[snapshot.lightweight]
target = "config_repo"            # always the config repo
branch_prefix = "state/"
squash_after = "30d"

[snapshot.bulk]
primary  = "primary-nas"          # name from the [[storage]] table above
mirrors  = ["offsite"]            # additional destinations kept in sync
```

#### Replication semantics

When storage is `kind = "replicated_nfs"` (or any other replicated
kind), orca owns the replication. Writes go to `primary`; orca
streams the same write to each `replicas` entry. The replication
target — RPO — is `sync_rpo`. If a write to a replica is delayed
beyond that, alerts fire.

Verification (`verify = "daily"`) is the trust check: orca walks
primary + replicas and compares hashes. Drift between copies is
treated as a hard failure — silent divergence between copies you
*think* are mirrors is worse than no replication at all.

Failover (`fail_over = "automatic"`): if the primary becomes
unreachable, orca promotes the most-recently-verified replica to
primary and keeps serving writes. When the old primary returns,
it becomes a replica and resyncs from the new primary. No host
names are baked into snapshot configs — only the storage pool
name — so failover is transparent to snapshot consumers.

#### Multiple destinations

`mirrors` keeps additional storage pools in sync with primary. The
default homelab shape is "replicated NFS as primary, S3 as the
offsite mirror," giving on-site fast access plus offsite
durability. Snapshot failures on a mirror don't block the deploy
or the primary write, but stale-mirror alerts fire if the mirror
falls behind.

#### Why this matters

Combined with the rest of the framework:

| Layer | What | Where it lives |
|---|---|---|
| Desired state | Operator-authored TOML | `main` of the config repo |
| Realized state | Orca-generated snapshots | `state/` branch of the same repo (or alternative target) |
| Encrypted data stores | Backup blobs (config.db, audit.db, etc.) | PBS / S3 / NFS — never the repo |
| Keys | Master key, CA key, offsite keys | Vault (1Password / Shamir / YubiKey) |
| Secret values | Integration credentials | Vault item bodies; never the repo |
| Secret schema | Key *names* only | `state/<host>/secrets.schema.toml` in the repo |

The repo is now the always-up-to-date, human-readable, git-diffable
record of "what's deployed where." Vault holds what you can't put
in git. Backup targets hold what's too big or too sensitive for
either.

### 4.12 First-install flow

The install script ([install-bootstrap.md](install-bootstrap.md))
walks new operators through escrow setup at pod creation:

1. Choose primary escrow target (1Password / Bitwarden / Shamir).
2. Authenticate to the target (1P sign-in, etc.).
3. Choose a secondary fallback (defaults to air-gapped file the
   operator copies to USB).
4. Orca generates the master key + CA key, writes them to both
   targets, verifies readback.
5. Operator confirms they can read each back before the install
   script returns success.

No install completes without the escrow round-trip succeeding.
This is the moment the operator most needs to think about
recovery, and the install script is the natural enforcement point.

---

## 5. Restore

`orca backup restore` is the inverse verb. Two modes:

### 5.1 Service restore

```sh
orca backup restore --job appdata-baldur --to baldur --snapshot 2026-05-20T03:00:00Z
orca backup restore --job appdata-baldur --to baldur --snapshot latest --dry-run
```

`--dry-run` prints the file diff without writing. Default mode
refuses to restore over existing live data without `--overwrite`.

### 5.2 Orca self-restore (disaster recovery)

```sh
# On a new host that will become the next baldur:
orca install --pair-token NONE --restore-from <target>:<snapshot-id>
```

Sequence:

1. Install orca binary (no daemon start yet).
2. Fetch backup blob from target.
3. Prompt for master key (Shamir reassembly).
4. Restore config + secrets + audit DBs to `${ORCA_DIR}/`.
5. Restore CA key from escrow (separate prompt, separate Shamir
   set).
6. Start daemon. Daemon comes up as the original peer_id, with all
   prior config + secrets + CA.
7. Mesh other peers reconcile and accept the recovered host —
   peer_id matches, cert chain matches (just rotated under the
   restored CA).

If the lost host was **not** the founding peer, the CA-key step is
skipped (other peers already hold the CA). Faster, lower-stakes
restore.

---

## 6. HA / baldur-SPOF mitigation

Backup-restore is *the* recovery path for SPOF mitigation in v1.
True HA (multiple peers running Caddy in active-active, leader
election for the config-store) is **deferred** and tracked
separately. The mitigation we ship:

- `orca-self-backup` runs frequently enough (hourly config/secrets,
  daily everything) that the recovery point is small.
- Restore tested regularly — see §7.
- Caddy config (per [caddy-plugin-scope.md](caddy-plugin-scope.md))
  is fully reproducible from the config repo; restoring just means
  pointing DNS at a new host running orca with the repo.

The remaining SPOF is **time-to-recover**: how fast can a new
baldur stand up? Target: < 30 minutes from "old baldur dead" to
"routes back up on new IP," assuming the operator is present and
escrow material is accessible.

---

## 7. Verification + drills

Every backup carries a manifest. Verification reads the manifest
and the backup target, confirms file presence and content hashes.

Two verification cadences:

| Mode | Trigger | What |
|---|---|---|
| Light | After every backup | Manifest matches what target reports. Bytes match. ~minutes. |
| Deep | Weekly | Manifest + content hash recompute. Slower. Detects bitrot on the target. |
| Restore drill | Quarterly | Actually restore the backup to a scratch location, run integrity checks. Manual or scheduled. |

Restore drills surface in the orca UI as a tracked obligation: the
last successful drill timestamp is shown per job; jobs without a
recent drill fire a soft warning.

---

## 8. Offsite

Local backups protect against single-host loss. Offsite protects
against site loss (fire, theft, ransomware that walks the LAN).

Same job model — just a target with `kind = "s3"`,
`kind = "rsync_net"`, `kind = "restic_repo"`, etc. Pre-existing
homelab plans (meerkat's `offsite-backup.md`) become consumers of
this framework.

Hard rule: offsite targets receive **encrypted** blobs only. The
encryption happens on the source orca host, before transmission.
Target credentials in offsite providers can be compromised without
exposing backup contents.

---

## 9. Work breakdown

| # | Item | Size |
|---|---|---|
| BK1 | Job model: TOML schema + scheduler integration | M |
| BK2 | Source trait + first impl (`docker_volumes`) | M |
| BK3 | Target trait + first impl (`pbs`) | M |
| BK4 | Manifest format + verify (light) | M |
| BK5 | Retention policies (keep-daily/weekly/monthly) | S |
| BK6 | `orca backup restore` + `--dry-run` | M |
| BK7 | `orca_state` source: config + secrets + audit DBs (SQLite-safe) | M |
| BK8 | Key inventory + escrow framework (multi-target) | M |
| BK8a | 1Password target via `op` CLI | S |
| BK8b | Bitwarden target via `bw` CLI | S |
| BK8c | Shamir target (split + reconstitute) | M |
| BK8d | YubiKey / PIV target | M |
| BK8e | Air-gapped file target + USB workflow | S |
| BK8f | `orca pki key audit` + GitOps gate on missing/stale escrow | S |
| BK9 | First-install escrow round-trip enforcement | S |
| BK9a | Per-key rotation flow with escrow update + rollback | M |
| BK9b | Restore: import keys from escrow at recovery time | M |
| BK10 | Additional sources: `nfs_share`, `directory`, `proxmox_guest_via_pbs`, `unraid_share`, `sqlite_db`, `config_snapshot` | L |
| BK11 | Additional targets: `local`, `nfs`, `s3`, `rsync_net`, `restic`, `borg` | L |
| BK12 | Deep verify + bitrot detection | M |
| BK13 | Restore drill scheduling + UI obligation surface | S |
| BK14 | Offsite encryption wrapper | M |
| BK15 | Parity check + retirement of each meerkat backup script | M each |

BK1–BK6 are the MVP that replaces one shell script end-to-end.
BK7–BK9 are the orca-state story. BK10–BK14 are breadth.

---

## 10. Open questions

- **Backup of metrics/logs DBs**: include in `orca_state`, or treat
  as expendable? Lean expendable — the data exists in the live
  systems and can be re-collected, and the storage cost of backing
  them up is meaningful.
- **Encryption key for offsite**: per-job or per-pod? Per-pod is
  simpler; per-job is safer (compromise of one job's key leaks
  less). Lean per-job with a per-pod default.
- **Restore drill execution target**: a scratch VM that gets torn
  down? A dedicated drill host? Either way it should run from
  orca, not a hand-procedure.
- **PBS namespace per host**: keeps backups clean but PBS namespace
  count has limits. Confirm at fleet scale.
- **Backup of the config repo itself**: the repo lives on
  GitHub/Gitea — those have their own backup story, but for the
  "GitHub is down for a week" case we may want a daily mirror push
  to a self-hosted Gitea instance as the backup-of-the-backup.

---

## 11. Relationship to other planned docs

- [orca-as-logic-layer.md](orca-as-logic-layer.md) §3.3 — meerkat
  backup scripts listed for migration; this doc is where they
  migrate to.
- [pki-lifecycle.md](pki-lifecycle.md) — CA key backup is referenced
  there; this doc owns the mechanism.
- [observability.md](observability.md) — audit DB retention floor.
- [schema-evolution.md](schema-evolution.md) — parity rule
  governs each meerkat-script retirement.
- [install-bootstrap.md](install-bootstrap.md) — master-key Shamir
  share happens at first install; restore prompts during install.
- Consuming-repo example: meerkat's `docs/planned/offsite-backup.md`
  and `docs/backup-gaps.md` describe the scottkey-homelab instance
  of these jobs.
