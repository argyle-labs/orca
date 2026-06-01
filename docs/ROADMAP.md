# Orca Roadmap

Canonical sequencing for orca development. Every `docs/planned/*.md`
doc is scope detail for one or more roadmap items here — this file
is the source of order.

---

## North star

Orca is a declarative system manager. It replaces hand-maintained
host scripts, per-host shell automation, and ad-hoc compose
orchestration with one binary running on every host, one config
repo as source of truth, and one tool surface (CLI / MCP / REST /
WASM) for every operation.

**Until parity with the existing homelab automation is reached,
nothing else is in scope.** Service-feature work, frontend polish,
the rebuy plugin, namespace consolidation, advanced PKI — all
deferred behind system lifecycle.

System lifecycle = **setup + update + maintenance** of the host
itself, the guests on it, and the storage/network plumbing under
it. That is the path to parity. Phase 2 (service surface) and
Phase 3 (deferred) cannot start until Phase 1 is closed.

See `feedback_parity_rule.md` (meerkat memory) — no retirement of
existing automation until orca passes the four-check parity test
(functional / side-effect / failure-mode / operational) on every
target host.

---

## Phase 0 — Shipped

What already works end-to-end in tree. Cite by file path so the
roadmap stays grounded.

| Capability | Location |
|---|---|
| One-binary CLI / REST / MCP / WASM tool surface via `#[orca_tool]` macro | `projects/derive` + `projects/dispatch` + `projects/contract` |
| Install + delete tools (`system.install`, `system.delete`); `uninstall` is an in-process helper (`cmd_uninstall_report`, not yet a tool surface); `pair` is a CLI helper (`cmd_pod_pair`, not `#[orca_tool]`); `doctor` not built | `projects/system/src/install.rs:191` + `projects/pod/src/cli.rs:533` + `scripts/install.sh` |
| Self-update (channels parse stable / rc / dev; mesh-relay; pinned versions; OS update wiring). **`dev` channel rejects all releases today** (`update_state.rs:69, 277`); see `project_dev_channel_plan.md`. | `projects/system/src/update.rs` (~525 LOC) |
| In-process scheduler (cron tick, periodic primitive, runs table) | `projects/system/src/scheduler.rs` + `projects/system/src/periodic.rs` |
| Host identity / status / system_info collectors | `projects/system/src/{host.rs, host_identity.rs, host_status.rs, system_info*}` |
| Daemon (HTTP 12000 / HTTPS 12443 / mesh 12002, dual-bind, runtime log levels) | `projects/system/src/daemon.rs` |
| Config store (SQLite, history, schemas, owner-routing) | `projects/db/src/config_store.rs` + `projects/db/migrations/` |
| Pod mesh: mTLS, mDNS discovery, peer pairing, dispatch, cert rotation | `projects/pod` |
| Secrets store (encrypted SQLite) + auth + PKI (CA, peer mint/rotate) | `projects/auth/src/{secrets.rs, pki.rs}` |
| Topology collector (proxmox CT/VM + docker containers, drift-aware) | `projects/system/src/topology/` |
| Proxmox API plugin (VM/LXC list, snapshot, `lxc_exec`) | `projects/plugins/proxmox` |
| NFS + SMB client plugins (mount, probe, lazy unmount, failover) | `projects/plugins/{nfs,smb}` |
| Docker / Dockge / Unraid GraphQL / Home Assistant collectors | `projects/plugins/{docker,dockge,unraid,homeassistant}` |
| Plugin host (subprocess + mTLS JSON-RPC), runtime, SDK (rust/go/ts/kotlin) | `projects/plugins/runtime` + `projects/sdk` |
| ntfy push + heartbeat | `projects/plugins/ntfy` |

---

## Phase 1 — System lifecycle parity (THE FOCUS)

Until every item below has an exit criterion met on every relevant
host, nothing in Phase 2 begins.

### 1.1 LXC + VM reconciler — declarative Proxmox guests

**Scope** — Repo `meerkat/proxmox/configs/{lxcs,vms}/*.conf` is the
desired state. Orca diffs against live `/etc/pve/{lxc,qemu-server}/*.conf`,
applies via `pct set` / `qm set` or stop→edit→start windows, with
per-key strategies (`replace`, `preserve-runtime-additions`,
`fail-on-drift`). Bind-source readiness probe (NFS stale-handle
check, tmpfs active) gates `pct start`. After start, inner-service
health probe via service-plugin `health` over `pct exec`. Restore
wraps `vzrestore` with pre-restore audit + post-restore topology
diff. Tmpfs scratch (frigg `/var/lib/orca-transcode` 8G shared) is
a host-owned systemd `*.mount` unit with per-consumer subdir +
quota floor.

**Shipped** — None of the reconcile loop. Proxmox plugin can read
state; cannot apply. Topology collector exists.

**Missing** — `pct.conf` parser/serializer, diff engine, strategy
registry, `pct set` apply path, bind-source probe, inner-service
gate, tmpfs scratch model, restore-aware wrapper, drift detection
periodic job, `orca proxmox guest {drift,reconcile,start,stop,restore}`
verbs. Retire `proxmox/lxcs/*.sh` (njord.sh etc).

**Exit criteria** — `orca proxmox guest reconcile <vmid>` is a
no-op on every CT in meerkat. `pct start` via orca only succeeds
when inner service comes up healthy. `vzrestore` recorded in audit
DB with PBS snapshot id. Drift detection has zero diverged keys
across the fleet for 7 consecutive days.

**Blocks on** — None. Config store is shipped; this is greenfield
on top.

**Detail** — `docs/planned/lxc-vm-reconciler.md`.

**Driver** — njord 2026-06-01 restore exposed the failure mode:
silent drift between repo and live, manual `sed` to re-point
willow→pool bind paths, plex came up `enabled but inactive` because
binds were empty at service-start time. This is the single biggest
parity gap.

---

### 1.2 Host update lifecycle

**Scope** — `orca host update {list,plan,apply}` per-distro
(`apt`/`apk`/`dnf`/`pacman`/`pkg`/`opkg`), declarative
`config/<host>/updates.toml` (schedule, security-apply, hold,
reboot-window), reboot/shutdown with ordered pre-hook chain
(drain caddy → stop docker → unmount nfs), distributed rolling
reboots with health gate.

**Shipped** — Orca self-update including OS-update wiring lives
in `projects/system/src/update.rs` (single `system.update` tool
covers all update concerns per `feedback_one_tool_per_resource.md`).
Host-level package-manager drivers and declarative update policy
are extension surface, not greenfield.

**Missing** — Per-distro package-manager drivers behind one verb,
TOML policy schema, reboot hook chain executor, rolling selector.

**Exit criteria** — `orca host update apply --reboot if-needed`
drains caddy, stops docker, reboots, comes back, verifies health,
on every host. `orca host reboot --selector "role=docker" --strategy rolling`
works across the fleet without manual sequencing.

**Blocks on** — None.

**Detail** — `docs/planned/host-lifecycle.md` §2–§3.

---

### 1.3 Host install hardening

**Scope** — `scripts/install.sh` + `projects/system/src/install.rs`
already do the heavy lifting. Hardening list: idempotent
re-install (re-running install does not churn pubkeys or systemd
unit), pair-token rotation (today pairing codes live in journal
grep — needs first-class storage + rotation), per-platform unit
templates (currently linux-user-systemd; Unraid uses
`/mnt/user/appdata/orca/bin/` per `project_unraid_persistence_via_appdata.md`),
release-artifact verification (sigstore/cosign vs minisign
decision still open per `project_security_hardening_v1` H1
deferred), bootstrap.toml loader for first-run repo+app provisioning.

**Shipped** — `install.sh` pull + push paths (`scripts/deploy-host.sh`),
orca service user with linger, root-owned authorized_keys via
`--admin-pubkey`, automatic `daemon install` + PKI ca-init at
end of root flow, channel pin (`~/.orca/channel`).

**Missing** — Bootstrap.toml schema + first-run flow, pair-token
table (replacing log-grep), unit-template per OS variant, signed
binary verification, `orca bootstrap doctor`.

**Exit criteria** — Fresh host bootstraps with one ssh + a
`bootstrap.toml` reference, no log-grep for pairing codes, signed
binary verified before exec, re-running install on a paired host
is a true no-op.

**Blocks on** — None. Note: `bootstrap.toml` repo discovery is
enhanced by `orca-v1-scope.md` §3.6 (GitHub App, deferred) but not
required; install + a manually-pointed `bootstrap.toml` works
without it.

**Detail** — `docs/install-runbook.md` + `docs/planned/install-bootstrap.md` +
`docs/planned/orca-v1-scope.md` §3.5–§3.6.

---

### 1.4 Drift detection

**Scope** — Periodic compare of repo vs live for everything orca
manages: LXC/VM configs (§1.1), tmpfs scratch (§1.1), driver
versions (§1.6), update policy state, NFS/SMB mount-spec, share
exports (§1.7). One event kind per resource, surfaced in
observability + `orca <noun> drift list`.

**Shipped** — Scheduler primitive ready (§Phase 0).

**Missing** — Per-noun drift checker registrations, event emission
schema, retention policy, fleet-aggregate drift view in UI.

**Exit criteria** — A repo edit not yet reconciled shows up in
drift within one tick (≤10 min). Operator can see "12 hosts have
driver drift, 3 CTs have config drift" at a glance.

**Blocks on** — §1.1 (first concrete consumer); §1.6 driver state.

---

### 1.5 Inner-service health probes

**Scope** — `pct start <vmid>` returning 0 ≠ workload up. After
container/VM `running`, orca delegates to the service plugin's
`health` over `pct exec` / `docker exec` / SSH. If unit is
`enabled` but `inactive` post bind-probe, single restart attempt,
then alert. Same primitive serves §1.1 (LXC reconcile) and §1.2
(post-reboot validation).

**Shipped** — Service collectors per plugin can report state
(sonarr/plex/etc via openapi). The gating loop after lifecycle
events is missing.

**Missing** — Generic `service.health(runtime = lxc:N | docker:X | host:Y)`
trait, post-lifecycle gate (single retry + alert), wiring into
reconcile + reboot paths.

**Exit criteria** — A CT restart by orca never returns success
when the inner service is `enabled but inactive`. Plex/jellyfin/
sonarr/etc. all report through one verb.

**Blocks on** — §1.1.

---

### 1.6 Driver lifecycle

**Scope** — `orca host driver {list,install,update,remove,status}`
with declarative `config/<host>/drivers.toml`. DKMS-aware kernel
coordination (post-kernel-upgrade rebuild + verify load).
Container Toolkit hook into docker daemon config. NVIDIA / AMD
(ROCm) / Intel (compute-runtime, media-driver) as the v1 set.

**Shipped** — Nothing yet.

**Missing** — Everything. NVIDIA first (highest churn, biggest
operator pain). DKMS verification step is the load-bearing piece —
silent failed rebuilds today produce ghost outages.

**Exit criteria** — Driver pin survives kernel upgrade. Status
shows kernel-module-loaded indicator. Drift catches mismatch.

**Blocks on** — Phase 1.2 (kernel upgrade is a host-update event;
pin coordination needs both sides).

**Detail** — `docs/planned/host-lifecycle.md` §1.

---

### 1.7 Storage-gateway server-side reconciler

**Scope** — Today `projects/plugins/{nfs,smb}` are client-only.
Tyr (10.10.10.29) needs declarative NFS exports + smb.conf + Avahi
+ wsdd as a single share spec: one TOML row produces NFS export +
SMB share with fruit + mDNS advertise + WSD broadcast for
cross-platform discovery (Mac/Win/Linux).

**Shipped** — Client side.

**Missing** — Server-side reconciler for exports/smb.conf/Avahi/wsdd,
share TOML schema, gateway-mode detection (a host is both client
and server for different roots), declarative SMB+fruit defaults.

**Exit criteria** — Tyr's `/srv/pool/*` exports + shares + mDNS +
wsdd are reconciled from `config/tyr/shares.toml` with zero
hand-edited config files on the host.

**Blocks on** — None.

**Detail** — `docs/planned/storage-shares.md`. See meerkat memory:
`project_tyr_storage_gateway.md`, `project_crossplatform_shares.md`,
`feedback_storage_abstraction.md` (never name hosts in targets,
reference pool names).

---

### 1.8 Backup plugin + native-API-first

**Principle (hard rule)** — Orca **controls**, it does not
reinvent. Every backup uses the service's own native mechanism
where one exists; orca only schedules, fetches, stores, and
verifies. Concretely:

- **Proxmox VMs / LXCs** → PBS (Proxmox Backup Server) via its API.
  Never tar a guest's rootfs.
- **arr stack** (sonarr/radarr/lidarr/prowlarr/readarr/whisparr) →
  `POST /api/v3/system/backup` then download the zip. Never tar
  the config volume.
- **Home Assistant** → `POST /api/hassio/backups/new/full` then
  download the snapshot. Never tar `.storage`.
- **Other services with native endpoints** (audiobookshelf,
  zigbee2mqtt, immich DB dump, plex, jellyfin) → native API.
- **Volume-tar is the last resort**, only for services with no
  native option, and even then only after explicit per-service
  decision.

Per-service adapter table lives in `docs/planned/backup-restore.md`
§2.1. Every native source ships with a matching restore + drill
fixture (`feedback_native_backup_apis.md`).

**Scope** — `projects/plugins/pbs/` (PBS client + sync-job API
wrapper). Per-service native backup verbs registered into the
canonical `orca <service> backup` / `orca <service> restore`
surface. Drill harness in CI.

**Shipped** — Nothing under `projects/plugins/pbs/`. Per-service
backup endpoints are reachable via existing arr OpenAPI plugins
but not orchestrated.

**Missing** — pbs plugin (CRUD over VM/CT snapshots, retention,
prune), service-native backup verbs registered into the canonical
`orca <service> backup` surface, restore drill harness, offsite
sync.

**Exit criteria** — Every service in meerkat has `orca <name>
backup` + `orca <name> restore` working, with a drill fixture in
CI. PBS verb covers every CT/VM in the fleet. Volume-tar is
deprecated except for services with no native API.

**Blocks on** — §1.1 (restore-aware lifecycle wraps `vzrestore`).

**Detail** — `docs/planned/backup-restore.md`.

---

### 1.9 Topology / observability minimum

**Scope** — Enough observability to **prove lifecycle worked**:
SQLite-backed metrics with per-host retention policy
(`project_db_size_and_retention.md` — logs/metrics go to files,
not rows), status tree on `parent_peer_id` (host → CT/VM →
service), drift counts, scheduler-run history.

Replaces uptime-kuma + ntfy for the lifecycle-validation use case.
Push-based pod subscribe for realtime (`feedback_optimistic_ui_updates`,
`project_data_ownership_and_realtime`); kills the 60s host_status
puller.

**Shipped** — `host_status`, `scheduler_runs`, topology collectors,
ntfy push. UI tree view roadmap'd in `project_ui_topology_views.md`.

**Missing** — Per-host retention policy enforcement (today metrics
can grow unbounded), drift aggregate view, lifecycle-event timeline
(install → pair → update → reboot → restore → reconcile, one
chronological feed).

**Exit criteria** — Operator can answer "did the lifecycle event
succeed?" from one screen for any host. db_size_bytes stays under
the per-host policy.

**Blocks on** — §1.4 (drift events) for full coverage.

---

### 1.10 Schema-evolution discipline

**Scope** — Already practiced (per `project_db_squash`, the v2
baseline migration). Make sure docs reflect: in-repo migrations
for schema changes, never down-migrations that re-insert removed
names, parity rule before any retirement.

**Shipped** — Practice. Migrations live at `projects/db/migrations/`.

**Missing** — Doc alignment only.

**Exit criteria** — `docs/planned/schema-evolution.md` matches
shipped behavior. Hard rules live in this ROADMAP's "Cross-cutting standing rules" section.

---

### 1.11 Envs + secrets — one projection surface, two trust levels

> **HARD RULE — user-triggered changes only.** Orca **never**
> auto-applies changes to envs, secrets, or system state. Drift
> detection + notification only; the operator runs `orca apply
> <change-id>` (or accepts a UI prompt). Applies symmetrically to
> backend-side rotations, repo-side edits, and host-side drift.
> No self-healing, no auto-reproject, no scheduled apply.

**Scope.** One projection surface, two trust levels. Non-secret
envs (cleartext, freely synced, visible in UI) and secrets
(handle-only in git, resolved at projection time, never logged)
ride the same adapters onto targets. All four backends are v1
first-class: orca-native, 1Password (personal tenant only),
Bitwarden, Vaultwarden. Per-node toggles (`secrets.backends`,
`store_local`, `sync_peers`) determine where secrets materialize;
mesh resolution routes around nodes without local backend access.
Bundle-handle pattern (`op://Orca/automations.maple` resolves the
whole item) is the primary declaration shape. `[plugin.secrets]`
in `orca-plugin.toml` lets Tier 2 plugins declare what they need.

**Shipped.** `SecretBackend` trait stub (inline only) at
`projects/auth/src/secrets.rs`; config store rows in `projects/db`;
per-host mTLS execution channel.

**Missing.** Promoted orca-native backend; 1Password / Bitwarden /
Vaultwarden adapters; two-tier schema (envs vs secrets); per-node
toggles + mesh resolution path; declaration schema (TOML in config
repo); per-target projection adapters (LXC / VM / bare metal /
docker host); drift readers per target; restart policies wired to
inner-service health (§1.5); audit log surface; `[plugin.secrets]`
parser + projection wiring; `orca secret migrate` cross-backend.

**Exit criteria.** Every `.env` in `meerkat/compose/*/` is declared
in the config store and the on-disk file is generated. Plaintext
secret in git is a P0. A node with both toggles `off` can drive
any orca command via mesh resolution. `orca env list` / `orca
secret list` show declared vs current with a drift count (secret
view never reveals values). Out-of-band rotation (in any backend)
emits a pending-change notification listing affected consumers;
nothing projects until `orca apply`.

**Blocks on.** §1.1 (first concrete consumer); unblocks §1.2, §1.3,
§1.5, §1.8 — all need real envs/secrets.

**Detail.** Full design — backend matrix, topology patterns A/B,
mesh resolution, bundle convention, projection adapters,
rotation-detection per backend, `[plugin.secrets]` contract — lives
in [`docs/planned/secrets-identity.md`](planned/secrets-identity.md).
Plugin-side contract details: [`docs/planned/plugin-architecture.md`](planned/plugin-architecture.md) §4.

---

### 1.12a Discovery + enrollment (basic — no escrow)

**Scope** — Phases 2 + 3 of host onboarding (phase 1 is §1.3).
mDNS broadcast (zero-config discovery on a trusted L2 segment),
out-of-band one-time enroll token (operator pastes into `orca pod
add`), `orca pod discover` known/unenrolled flags. **No escrow
recovery** — a re-installed host appears as fully new.

**Shipped** — mDNS discovery + mTLS pairing + cert rotation +
pair-token mint in `projects/pod`.

**Missing** — Documented TXT record fields (`peer_id`, `enrolled`,
`os`, `arch`, `release`); `orca pod discover --unenrolled / --known`
flags; `orca pod add` enrollment-side wrapper verb; `--no-mdns`
install flag for hostile networks; first-class `orca system
pair-token {show,rotate}` (replaces journal-grep).

**Exit criteria** — An operator can take a fresh host from
`system.install` to "in the pod" via paste-token in under 60
seconds. `orca pod add <host> --token <oob>` is the only enrollment
step.

**Blocks on** — §1.3 (install must emit the OOB token).

**Detail** — `docs/planned/discovery-enrollment.md`.

---

### 1.12b Pod rejoin (escrow recovery)

**Scope** — `orca pod rejoin` recovers a host's identity-derived
secret-store key from k-of-n escrow after re-install. A wiped +
reinstalled host with surviving `/etc/machine-id` appears under
`pod discover --known` and `rejoin` restores escrowed state.

**Shipped** — Nothing.

**Missing** — Identity-key escrow at enrollment (k-of-n across
peers); `orca pod rejoin` verb; escrow-quorum safety gate.

**Exit criteria** — A wiped + reinstalled host can recover its
identity-anchored secrets without operator re-paste.

**Blocks on** — §1.8 §4.4 (escrow infrastructure), §1.12a (basic
enrollment).

**Detail** — `docs/planned/discovery-enrollment.md`; escrow in
`docs/planned/backup-restore.md` §4.4.

---

### 1.13 Host decommission

**Scope** — Inverse of install + enrollment. `orca pod remove
<host>` revokes peer cert, removes the roster entry (CRDT
tombstone for audit), cleans up dependent reconciler entries
(projected envs/secrets, share `valid users` membership, backup
schedules owned by the host), and re-shards escrowed identity-key
shares if the host held any. `--wipe` extends with on-host data
purge.

**Shipped** — Nothing first-class. Today decommission is
hand-driven.

**Missing** — `orca pod remove <host>` verb; reconciler hook for
"host left the pod" cascade; share re-distribution for escrowed
keys; safety gate (refuse if removing the host drops escrow
quorum below k-of-n).

**Exit criteria** — Removing a host is a single verb that leaves
zero orphaned references in any other reconciler's state. Audit
shows when/why/by-whom.

**Blocks on** — §1.12a (enrollment surface defines the shape of
decommission); §1.12b for the escrow-re-share path.

---

### 1.14 Network reconciler — DNS / firewall / DHCP

**Scope** — Declarative DNS (Adguard) records, OPNsense firewall
rules, DHCP reservations. Dual-stack (A + AAAA) per
`project_dns_dualstack`. Gateway-monitoring config per
`feedback_opnsense_gateway_monitoring` (don't ship `monitor_disable=1`
defaults). Specific records beat wildcards.

**Shipped** — Nothing yet. Adguard + OPNsense plugins are Tier 1
slots (`projects/plugins/{adguard,opnsense}/`) — not yet created.

**Missing** — Both plugins (Tier 1 in-process), declarative TOML
schemas under `config/<host>/{dns,firewall,dhcp}.toml`, dual-stack
validation, drift surface.

**Exit criteria** — Every record currently in Adguard + every
OPNsense rule is declared in the config repo. `pool.scottkey.me`
resolves dual-stack from declaration. Adding a new host gets DNS
+ DHCP + firewall holes in one operator-driven apply.

**Blocks on** — §1.11 (needs secret backend for OPNsense API tokens).

---

### 1.15 NTP / clock management

**Scope** — Install (§1.3) brings up chrony or systemd-timesyncd
as part of daemon prerequisites. This item makes clock state a
first-class resource: monitor offset, emit drift events when offset
exceeds threshold (default 500ms), declarative peer/server list per
host, fail-closed checks on cert validity + scheduler ticks + audit
ordering + CRDT causality.

**Shipped** — Nothing; install doesn't even ensure NTP today.

**Missing** — Install-side prereq landing (covered in §1.3); host
status surface for offset; alert on excessive drift; declarative
NTP server config per host.

**Exit criteria** — Every host reports clock offset; offsets >500ms
fire a drift event; cert / scheduler / audit code can rely on
"clock is good" instead of defensively re-checking.

---

### 1.16 UPS-coordinated shutdown

**Scope** — Already designed in `host-lifecycle.md` §4 (NUT
listener on UPS-USB host; broadcast low-battery → ordered shutdown
across the fleet → UPS-host shuts down last). Surfacing it here
because it's invisible in the current roadmap.

**Shipped** — `host-lifecycle.md` §4 design only.

**Missing** — NUT integration plugin (Tier 1), per-host
declarative shutdown order, dry-run mode, integration with
update-lifecycle pre-hook chain (§1.2).

**Exit criteria** — Pulling the wall power on the UPS triggers an
orderly fleet shutdown in declared order; the UPS-host is last;
power restoration drives the inverse boot order via Wake-on-LAN
where available.

**Blocks on** — §1.2 (shares the pre-hook executor); §1.5
(post-boot health gate).

**Detail** — `docs/planned/host-lifecycle.md` §4.

---

### 1.17 Storage replication policy

**Scope** — Per `feedback_storage_replication_policy`: users +
configs replicate freely; media/series does **not** replicate by
default; secure data does **not** replicate at all. Storage manager
(§1.7 + Phase 2) needs a policy field per pool/share that the
replication engine enforces. Full failover requires two-way data
sync for the replicated tiers only.

**Shipped** — Nothing; today every consumer makes its own
replication decision ad-hoc (Syncthing for some paths, manual
rsync for others).

**Missing** — Policy schema on share definitions, replication
engine that honors it (per `project_orca_storage_mesh.md`), drift
detection between primary + replica for replicated tiers.

**Exit criteria** — Every share in the storage layer has an
explicit replication policy. Replicated tiers stay in sync (drift
catches divergence). Failover for replicated tiers works without
manual data movement.

**Blocks on** — §1.7 (share-side schema), §1.11 (replication
credentials).

**Detail** — `docs/planned/storage-replication.md`.

---

### 1.18 `orca system doctor`

**Scope** — Health-check verb: peer reachability, cert expiry,
drift summary, secret-backend reachability. Pre-flight check
before any reconciler `apply`. Sizing: M.

**Shipped** — Nothing first-class. Today health is surfaced
piecemeal (`orca pod peers`, install-report, manual log grep).

**Missing** — `system.doctor` `#[orca_tool]` with per-check
registry, JSON output, severity levels, integration into
`orca apply` preflight.

**Exit criteria** — `orca system doctor` returns a single
pass/fail per host with itemized failures; reconciler `apply`
refuses to start with `doctor` failures unless `--force`.

**Blocks on** — None (consumes shipped primitives).

---

### 1.19 `orca system uninstall`

**Scope** — Promote the existing `cmd_uninstall_report` helper
(`projects/system/src/install.rs:191`) to a proper `#[orca_tool]`
surface. Pairs with §1.13 host decommission. Sizing: S.

**Shipped** — In-process helper exists; no tool surface.

**Missing** — `#[orca_tool]` registration, CLI verb wiring,
audit-emitting wrapper, `--keep-state` flag.

**Exit criteria** — `orca system uninstall` is callable across
all four surfaces; symmetric with `system.install`.

**Blocks on** — None.

---

## Cross-cutting cleanup (Phase 1)

Tracked-but-not-yet-fixed code issues. **No code edits without
a roadmap discussion** — items here are roadmap entries only.

### CC.1 — Remove meerkat hostnames from orca core test fixtures

Sizing: S. Pure rename. Files:

- `projects/db/src/plugin_tools.rs` L221-265 — `sonarr-willow`,
  `radarr-maple`, `sonarr-maple`.
- `projects/db/src/plugin_types.rs` L141-155 — same names.
- `projects/pod/src/caller_token.rs:246` — `"baldur"`.

Replace with neutral `host-a`, `host-b`, etc. Enforces
`feedback_no_rebuy_or_meerkat_in_orca.md`.

### CC.2 — Move `projects/plugins/ntfy/` out of `plugins/`

Sizing: S. Per `projects/plugins/ntfy/src/lib.rs` L1-2: "no
plugin scaffolding — this is a library." Move to
`projects/utils/ntfy/` or fold into `projects/app-kit/`. Update
workspace `Cargo.toml` + consumers. Drop ntfy from Tier 1
enumeration in `docs/planned/plugin-architecture.md` (done in
this audit).

---

## Phase 2 — Service surface parity

Begins only after Phase 1 closes. Each meerkat script + plugin +
compose stack maps to its orca successor. Inventory comes from
`docs/planned/orca-as-logic-layer.md` §3 (the retirement table).

Headline items, in roughly the order they unblock fleet operation:

- **Caddy plugin** — routes from `compose/caddy/routes/*.toml`,
  mTLS to orca upstreams, fan-out across edge hosts. Detail:
  `docs/planned/caddy-plugin-scope.md`.
- **Service catalog** — unified per-instance catalog (plex /
  jellyfin / arr / HA / dockge / syncthing), runtime adapters
  (LXC / Docker / Dockge / Unraid / systemd / bare). Detail:
  `project_service_catalog.md` (orca memory).
- **Compose stack reconciler** — `compose/*/docker-compose.yml`
  becomes declarative state orca applies. Today every stack is a
  bare yml + per-host overrides.
- **Storage manager** — cross-OS mount manager (NFS/SMB/S3/SSHFS)
  + Unraid GraphQL surface. Detail: `project_storage_manager.md`.
- **Syncthing replacement** — per `project_tyr_consolidation_syncthing.md`,
  future orca-managed share primitive replaces Syncthing for the
  willow→maple replication path.
- **OSS media plugins** — arr stack / qBittorrent / SABnzbd as
  first-party plugins under a separate identity per
  `feedback_oss_media_terminology.md`, `feedback_oss_media_separate_identity.md`.
- **Unified credentials** — SMB login == orca login, 1Password
  backend, baseline password rotation. Detail:
  `docs/planned/secrets-identity.md` + `feedback_unified_credentials.md`.

---

## Phase 3 — Deferred until parity

Explicitly out of scope until Phase 1 is closed. Listed so we
can say "no" with a reason.

| Item | Reason deferred |
|---|---|
| Caddy plugin first-class implementation | Phase 2. Routes today are operator-managed; not on the parity critical path. |
| Rebuy plugin | `feedback_no_rebuy_or_meerkat_in_orca.md` — rebuy is a second consumer of the plugin contract; only useful once the contract is hard. Detail: `docs/planned/rebuy-plugin-scope.md`. |
| Namespace consolidation (`docker-runtime.*` → `docker.runtime.*`, etc) | Cosmetic. Touching every call site costs more than the readability win. `docs/planned/namespace-consolidation.md`. |
| Frontend polish (Mantine strip, a11y audit) | `project_frontend_deferred_todos.md`. UI must reflect server state (`feedback_ui_must_reflect_server_state.md`) — that's the only frontend rule that matters during Phase 1. |
| Advanced PKI revocation, CRL, CT log | Mutual trust + cert rotation already shipped; revocation lift is post-parity. `docs/planned/pki-lifecycle.md`. |
| iOS / Android standalone | `project_mobile_as_standalone_orca.md`. UniFFI plan locked; not blocking lifecycle. |
| `dev:<branch>` channel | `project_dev_channel_plan.md`. Locked 2026-05-12, deferred. |
| 100% test coverage ratchet | `project_test_coverage_100.md`. Floor=51 in CI, ratchets per touched-files rule (`feedback_touched_files_100_coverage.md`). Aim, not gate. |

---

## Cross-cutting standing rules

These apply at every phase. Drawn from orca + meerkat memory.

- **User-triggered changes only** — orca **never** auto-applies
  changes to envs, secrets, system state, or host config. Drift
  detection + notification only; the operator decides when and
  what to apply. No self-healing, no auto-reproject, no scheduled
  apply, no "while you were away" reconciliation. Applies to all
  Phase 1 work.
- **Parity rule** — no retirement of existing automation until orca
  passes the four-check parity test on every target host:
  functional / side-effect / failure-mode / operational.
  (meerkat `feedback_parity_rule.md`)
- **Personal 1Password only** — homelab + orca secrets go in
  personal 1Password; never the rebuy/work account.
  (meerkat `feedback_personal_1password_only.md`, orca
  `feedback_op_personal_only.md`)
- **Native backup APIs first** — service-native endpoints before
  volume-tar; restore + drill fixture per source.
  (meerkat `feedback_native_backup_apis.md`)
- **Clients default to the gateway** — mount `pool.scottkey.me`
  (failover), never willow/maple directly.
  (meerkat `feedback_clients_default_gateway.md`)
- **Storage abstraction, no host names in targets** — backup /
  snapshot targets reference storage-pool names, not hosts.
  (meerkat `feedback_storage_abstraction.md`)
- **In-repo migrations** — schema changes ship as migrations in
  `projects/db/migrations/`. No down-migrations that re-insert
  removed personal/banned names. (orca `feedback_no_data_migrations_for_name_cleanups.md`,
  `project_db_squash.md`)
- **No "meerkat" or "rebuy" strings in orca core** — those are
  separate consumers. (orca `feedback_no_rebuy_or_meerkat_in_orca.md`)
- **One tool per resource** — `system.update` is the single update
  surface; no per-verb tool families. (orca
  `feedback_one_tool_per_resource.md`)
- **Orca self-updates without sudo** — manual ssh + sudo by an
  agent is an orca bug, not a peer problem. (orca
  `feedback_orca_self_updates_no_sudo.md`)
- **Never blind-trust caller identity** — recipient verifies role
  from its own replicated users data. (orca
  `feedback_zero_trust_no_blind_trust.md`)

---

## Open decisions blocking the roadmap

1. **Release signing — cosign vs minisign.** H1 from
   `project_security_hardening_v1` deferred. Install hardening
   (§1.3) cannot close until the verifier is wired into install.sh.
2. **Bootstrap.toml repo discovery vs GitHub App.** Detail in
   `orca-v1-scope.md` §3.5–§3.6. Fresh-host first-run flow needs
   one or the other resolved.
3. **Drift policy granularity per LXC.** `preserve-runtime-additions`
   on `mp*` is the njord-style default — is there a CT where we
   want `fail-on-drift` on `mp*` instead? Operator override exists;
   default could go either way.
4. **Tmpfs quota enforcement.** Subdir-per-consumer is the v1
   floor. Real per-bind project quota requires XFS/ext4 prjquota
   — opt in per host? Default off?
5. **GPU passthrough cross-node moves.** `dev0: /dev/dri/card0,gid=44`
   is node-local. Reconciler default = `replace` only on
   originating node, hard-block on cross-node moves. Confirm.
6. **Coordinator host for UPS-triggered shutdown.** Today the host
   with the UPS USB attached is the obvious choice but shuts down
   last. Define per pod.
7. **Auto-security updates.** `mode = "auto-security"` is opt-in
   per host today. Should it ever be the fleet default?
8. **Topology of session-state for ephemeral mesh.** db_size policy
   says metrics go to files, not rows — but lifecycle events do
   need to be queryable. Where is the boundary?

---

## Linear work order

Strict sequence. Don't parallelize past where a downstream item
genuinely needs an upstream's exit criteria.

1. **Step 0 — brain-era cleanup.** Non-negotiable. `docs/legacy/`
   established 2026-06-01. Anything still linking into the legacy
   set is a bug to fix as encountered.
2. **§1.3 — install hardening** (universal install verb, host-identity
   key, OOB enroll token, NTP + firewall prereqs, idempotent
   re-install). Everything else needs a host in the pod.
3. **§1.12a — discovery + enrollment (basic)** (mDNS TXT fields,
   `orca pod add` / `discover`, no escrow). Pairs with §1.3 and
   lands second.
4. **§1.11 — envs + secrets projection** (orca-native promoted,
   1Password backend, per-node toggles, mesh resolution,
   `[plugin.secrets]` wiring). Every other reconciler needs this.
5. **§1.1 — LXC + VM reconciler** (njord-driven; first concrete
   consumer of the env+secret layer + the user-triggered apply
   pattern).
6. **§1.4 — drift detection** (per-noun checkers, event schema,
   aggregate view). Becomes meaningful once §1.1 emits events.
7. **§1.5 — inner-service health probes** (post-lifecycle gate
   for §1.1 + §1.2).
8. **§1.2 — host update lifecycle** (per-distro drivers, reboot
   hook chain, rolling selector).
9. **§1.6 — driver lifecycle** (DKMS-aware, blocks on §1.2).
10. **§1.7 — storage-gateway server-side reconciler** (tyr exports
    + smb.conf + Avahi + wsdd from `config/tyr/shares.toml`).

Then in priority order without strict blocking:

- §1.8 backup plugin + native-API-first
- §1.12b pod rejoin (escrow recovery — needs §1.8 §4.4)
- §1.13 host decommission
- §1.14 network reconciler (DNS / firewall / DHCP)
- §1.15 NTP / clock management surface
- §1.16 UPS-coordinated shutdown
- §1.17 storage replication policy
- §1.9 topology / observability minimum
- §1.10 schema-evolution docs

Phase 2 (service surface) begins only after the top-10 sequence
above is closed.
