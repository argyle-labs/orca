# Backup + restore — unified story

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

---

## 3. Concern 1: managed-service backups (meerkat scripts → orca verbs)

| Meerkat script | Replacement | Notes |
|---|---|---|
| `backup-configs.sh` | `[[job]] kind = "config_snapshot"` source + `kind = "git_commit"` target | Currently writes into `backups/configs/` and the repo. Keep the on-disk layout exactly; parity-check per [schema-evolution.md](schema-evolution.md). |
| `restore-config.sh` | `orca backup restore --job <name> --from <snapshot>` | Same flag shape it had as a script. |
| `pbs-backup-hook.sh` | `[[job]] target = "pbs"` with the pre/post hooks above | PBS integration grows hooks API surface. |
| `pbs-mount-watchdog.sh` | Folds into the NFS health loop (see [orca-as-logic-layer.md](orca-as-logic-layer.md) §3.3) — not a backup job per se, but it gates whether backup jobs can run. |
| Per-host `backup-appdata.sh` (baldur, freyr, willow, maple) | `[[job]] kind = "docker_volumes"` per host | The chown-fix wrappers (`appdata-backup-chown.sh`, `fix-backup-ownership.sh`) become `[job.source.permission_fix]` options. |

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

### 4.4 Master-key + CA-key escrow

These two are special: lose them, you cannot restore. The escrow
plan:

- **Master key** (for secrets DB): printed at first orca init
  (Shamir-shared into N parts), each part stored separately
  (password manager, paper in a safe, second operator). Re-entered
  on restore.
- **CA key**: same Shamir-shared model on the founding peer. Plus
  hardware-token storage when available.

The install script ([install-bootstrap.md](install-bootstrap.md))
walks new operators through this exactly once at pod creation.

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
| BK8 | Master-key Shamir share at install + restore prompt | M |
| BK9 | CA-key escrow + restore for founding-peer recovery | L |
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
