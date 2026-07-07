# Orca Roadmap

Canonical sequencing for orca development. This file is the source
of order for one or more roadmap items here.

---

## North star

Orca is a declarative system manager. It replaces hand-maintained
host scripts, per-host shell automation, and ad-hoc compose
orchestration with one binary running on every host, one config
repo as source of truth, and one tool surface (CLI / MCP / REST /
WASM) for every operation.

**Until parity with the existing homelab automation is reached,
nothing else is in scope.** Service-feature work, frontend polish,
the example downstream plugin, namespace consolidation, advanced PKI — all
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
| Topology collector (proxmox CT/VM + docker containers, drift-detecting — never applies; notification only per the user-triggered-changes rule) | `projects/system/src/topology/` |
| Proxmox API plugin (VM/LXC list, snapshot, `lxc_exec`) | `projects/plugins/proxmox` |
| NFS + SMB client plugins (mount, probe, lazy unmount, failover) | `projects/plugins/{nfs,smb}` |
| Docker / Dockge / Unraid GraphQL / Home Assistant collectors | `projects/plugins/{docker,dockge,unraid,homeassistant}` |
| Jellyfin + Plex media-server plugins (server/library detail + transcode HW-vs-software diagnosis) | `projects/plugins/{jellyfin,plex}` |
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
diff. Tmpfs scratch (delta `/var/lib/orca-transcode` 8G shared) is
a host-owned systemd `*.mount` unit with per-consumer subdir +
quota floor.

**Shipped** — None of the reconcile loop. Proxmox plugin can read
state; cannot apply. Topology collector exists.

**Missing** — `pct.conf` parser/serializer, diff engine, strategy
registry, `pct set` apply path, bind-source probe, inner-service
gate, tmpfs scratch model, restore-aware wrapper, drift detection
periodic job, `orca proxmox guest {drift,reconcile,start,stop,restore}`
verbs.

**Exit criteria** — `orca proxmox guest reconcile <vmid>` is a
no-op on every CT in meerkat. `pct start` via orca only succeeds
when inner service comes up healthy. `vzrestore` recorded in audit
DB with PBS snapshot id. Drift detection has zero diverged keys
across the fleet for 7 consecutive days. `proxmox/lxcs/*.sh`
(media-a.sh etc.) retired under the parity rule once exit criteria
are met on every meerkat CT.

**Blocks on** — None. Config store is shipped; this is greenfield
on top.

**Driver** — media-a 2026-06-01 restore exposed the failure mode:
silent drift between repo and live, manual `sed` to re-point
alpha→pool bind paths, plex came up `enabled but inactive` because
binds were empty at service-start time. This is the single biggest
parity gap.

---

### 1.2 Host update lifecycle (including drivers)

**Scope** — Single `system.update` tool surface covers **all**
update-class work on a host per the one-tool-per-resource rule
(`feedback_one_tool_per_resource.md`):

1. **Orca self-update** (shipped).
2. **OS package updates** — per-distro drivers
   (`apt`/`apk`/`dnf`/`pacman`/`pkg`/`opkg`) behind one verb,
   declarative `config/<host>/updates.toml` (schedule,
   security-apply, hold, reboot-window).
3. **Drivers** — GPU + accelerator drivers under the same
   surface, declarative `config/<host>/drivers.toml`. DKMS-aware
   kernel coordination (post-kernel-upgrade rebuild + verify
   load — the load-bearing piece; silent failed rebuilds today
   produce ghost outages). Container Toolkit hook into docker
   daemon config. NVIDIA / AMD (ROCm) / Intel (compute-runtime,
   media-driver) as the v1 set; NVIDIA first (highest churn,
   biggest operator pain).
4. **Reboot / shutdown** with ordered pre-hook chain (drain
   caddy → stop docker → unmount nfs), distributed rolling
   reboots with health gate via §1.5.

Kernel upgrade is the integration point between (2) and (3) —
package-manager bumps kernel, driver pin must rebuild + verify
load before the host comes back into rotation.

**Shipped** — `system.update` tool surface; orca self-update
path inside it (`projects/system/src/update.rs`). Per-distro OS
drivers, driver-lifecycle subsystem, policy schema, and reboot
orchestration are the gap.

**Missing** — Per-distro package-manager drivers; driver
subsystem (DKMS rebuild + verify, Container Toolkit hook, status
with kernel-module-loaded indicator); TOML policy schemas for
updates.toml + drivers.toml; reboot hook chain executor; rolling
selector; drift catches driver/kernel mismatch.

**Exit criteria** — `orca host update apply --reboot if-needed`
drains caddy, stops docker, reboots, comes back, **verifies
driver load against pin**, verifies inner-service health, on
every host. Driver pin survives kernel upgrade. `orca host
reboot --selector "role=docker" --strategy rolling` works across
the fleet without manual sequencing.

**Blocks on** — None.

---

### 1.3 Host install hardening

**Scope** — `scripts/install.sh` + `projects/system/src/install.rs`
already do the heavy lifting. The whole first-run story is
**install + (optionally) restore from backup** — no separate
`bootstrap.toml` declarative pre-config file. A fresh host runs
install to get into the pod, and either:
(a) starts empty and is configured by an operator running normal
    orca commands (envs/secrets project in, reconcilers apply,
    etc.), or
(b) restores from a §1.8 backup of a prior host of the same role.

Hardening list:

- Idempotent re-install (re-running install does not churn
  pubkeys or systemd unit).
- Pair-token rotation (today pairing codes live in journal grep
  — needs first-class storage + rotation).
- Per-platform unit templates (currently linux-user-systemd;
  Unraid uses `/mnt/user/appdata/orca/bin/` per
  `project_unraid_persistence_via_appdata.md`).
- Release-artifact verification (sigstore/cosign vs minisign —
  see open decision #1).
- NTP prereq landing (chrony or systemd-timesyncd; surface made
  first-class in §1.15).

**Shipped** — `install.sh` pull + push paths
(`scripts/deploy-host.sh`), orca service user with linger,
root-owned authorized_keys via `--admin-pubkey`, automatic
`daemon install` + PKI ca-init at end of root flow, channel pin
(`~/.orca/channel`).

**Missing** — Pair-token table (replacing log-grep), unit-template
per OS variant, signed binary verification, `orca bootstrap
doctor`, NTP prereq landing.

**Exit criteria** — Fresh host bootstraps with one ssh, no
log-grep for pairing codes, signed binary verified before exec,
re-running install on a paired host is a true no-op. A fresh
host of an existing role can be restored from §1.8 backup
(install → `pod add --token` → `orca <role> restore <snapshot>`)
with zero hand-edited config files.

**Blocks on** — None.

**Detail** — `docs/install-runbook.md`.

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

**Blocks on** — §1.1 (first concrete consumer); §1.2 driver state.

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
when the inner service is `enabled but inactive`. The generic
primitive exists, with **at least one service wired as proof**
(plex on media-a is the natural first consumer). Full
plex/jellyfin/sonarr/etc. coverage is Phase 2 service-surface
work that consumes this primitive.

**Blocks on** — §1.1.

---

### 1.6 Resource nicknames + grouping + exposure

**Scope.** Every orca-managed resource (host, service,
instance, data row) has:

1. An editable **nickname**, unique across the pod, default =
   app/host name, multi-instance disambiguation by **purpose**
   not numbering. Full rule in
   `feedback_resource_nicknames_and_fqdn`.
2. Optional **group membership.** Resources can be grouped
   (`media`, `proxmox`, `storage-gateways`, etc.) or stand
   alone. Groups can be queried as a single unit
   (`config.list --group media`) or exposed collectively
   (one route fronting all media services) or individually.
   Same data, two views.
3. **Local-by-default exposure on every surface.** Every piece
   of orca data — nicknames included — is added to the local
   config store and immediately available on **CLI, MCP, REST,
   and WASM** without any DNS / FQDN / reverse-proxy work. The
   mesh dispatch surface already handles cross-host access by
   `peer_id` + nickname; no domain required.
4. **FQDN as optional, opt-in exposure path.** If the operator
   wants `sonarr.example.com` to resolve publicly, that's a
   separate **opt-in** layer on top — §1.14 DNS reconciler +
   Phase 2 reverse-proxy publish the same resource externally
   via `<nickname>.<fqdn.domain>`. Resources are fully
   functional with **no FQDN configured** — orca core does not
   require an operator domain to operate.

**`config` shape** (per §1.10 one-tool-per-resource):

- `config.update <resource>.nickname <name>` — rename.
- `config.update <resource>.group <group>` (or
  `--groups a,b,c` for multi).
- `config.update <resource>.expose.fqdn true|false` — opt in
  to external FQDN publishing (default `false`).
- `config.update fqdn.domain <value>` — operator's domain.
  Empty by default; orca works without it.
- `config.list [--nicknames] [--group <g>] [--exposed]` —
  read-only views.

Uniqueness is enforced for nicknames; groups are free-form.
Renames cascade atomically: any active FQDN publish, mDNS
advertise, reverse-proxy route, UI label all rewrite per
change_id.

**Driver lifecycle note.** Slot previously was the
"drivers folded into §1.2" stub. Drivers still live under
§1.2 `system.update`. Slot repurposed for the
naming/grouping/exposure layer since that's a real
cross-cutting resource concern.

**Shipped.** Peer IDs + container IDs in the config store +
topology collector + mesh dispatch (CLI/MCP/REST already work
by peer_id today). No nickname layer, no group layer, no
exposure toggle, no FQDN derivation.

**Missing.** Nickname + group columns on every resource row;
uniqueness constraint on nickname; default-assignment logic
(new host → host name; new service → app name; multi-instance
→ operator disambiguates by purpose); group membership +
group-aware listing; `expose.fqdn` toggle wired to §1.14;
optional FQDN derivation from `<nickname>.<fqdn.domain>`;
cascade-on-rename across all active exposure surfaces.

**Exit criteria.** Every resource in `config.list` has a
unique nickname and (optionally) one or more groups. Every
resource is callable on CLI/MCP/REST/WASM by nickname
**without any FQDN configured**. With `fqdn.domain` set + a
resource's `expose.fqdn = true`, `<nickname>.<fqdn.domain>`
resolves dual-stack via §1.14 and the reverse-proxy route
exists. Renaming is one `config.update` + one `config.apply`,
propagates everywhere automatically. Orca core test fixtures
use `host-a` / `host-b` (covers prior CC.1).

**Blocks on.** §1.11 (apply flow), §1.10 (config-as-code so
nicknames + groups + exposure persist into the repo).

**Unblocks.** §1.14 DNS reconciler (consumes nicknames for
opt-in FQDN), §1.7 storage-share advertise (consumes host
nicknames), Phase 2 caddy plugin (reverse-proxy routes keyed
on nicknames; respects `expose.fqdn`), Phase 2 service
catalog (display layer; uses groups for collapsing).

**Detail.** `feedback_resource_nicknames_and_fqdn` memory
(canonical — to be expanded with grouping + exposure rules).

---

### 1.7 Storage server-side — declarative shares + runtime health

**Scope** — Full storage-server surface, not just declaration.
Today `projects/plugins/{nfs,smb}` are client-only and meerkat
ships a shell `nfs-monitor` script (`compose/nfs-monitor/`) that
covers gateway re-export health. Orca absorbs both:

1. **Declarative reconciler** — one share-spec TOML row produces
   NFS export + SMB share (fruit defaults) + Avahi advertise +
   WSD broadcast for cross-platform discovery (Mac/Win/Linux).
   Gateway-mode detection (a host can be both client and server
   for different roots).
2. **Runtime health + failover** (was nfs-monitor) — periodic
   mountpoint liveness on backing pools, `exportfs -ra` refresh
   on change, read-only failover when a backend pool dies, drift
   event when re-export state diverges from desired. Same
   primitive runs on nas-a today; the containerized-orca deploy
   target (Phase 2) runs the same binary, so the runtime-health
   surface is inherited unchanged — no second implementation.

**Shipped** — Client side (`projects/plugins/{nfs,smb}`). Runtime
health is a meerkat shell script (`nfs-monitor`), not in orca.

**Missing** — Server-side reconciler for exports/smb.conf/Avahi/wsdd,
share TOML schema, gateway-mode detection, declarative SMB+fruit
defaults, runtime mountpoint probe + auto-re-export refresh +
read-only failover, drift event emission to §1.20.

**Exit criteria** — nas-a's `/srv/pool/*` exports + shares + mDNS +
wsdd are reconciled from `config/nas-a/shares.toml` with zero
hand-edited config files on the host. Backend-pool death triggers
read-only failover within one tick and emits a `requires_ack`
event via §1.20. `compose/nfs-monitor/` retired under the parity
rule.

**Blocks on** — None for the reconciler. Runtime health emits
through §1.20 (notifications) — degrades gracefully if §1.20 not
yet shipped (logs only).

**Detail** — See meerkat memory:
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

Every native source ships with a matching restore + drill
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
`orca <service> backup` surface, restore drill harness. (Offsite
*destination* lives in §1.21 orca-cloud; §1.8 owns producing the
streams, §1.21 owns receiving them.)

**Exit criteria** — Every service in meerkat has `orca <name>
backup` + `orca <name> restore` working, with a drill fixture in
CI. PBS verb covers every CT/VM in the fleet. Volume-tar is
deprecated except for services with no native API.

**Blocks on** — §1.1 (restore-aware lifecycle wraps `vzrestore`).

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
ntfy push. UI tree+table+network-map views are designed;
network-map renders the §1.21 relationship graph including cycles.

**Missing** — Per-host retention policy enforcement (today metrics
can grow unbounded; retention is **per-system config**, not global
— see `feedback_per_system_history_retention`; Systems-dashboard
top-level selector is the *default for newly-paired systems*,
overridable per-system on its settings page), drift aggregate
view, lifecycle-event timeline (install → pair → update → reboot
→ restore → reconcile, one chronological feed). Network-map view
must support pan/zoom (mouse + touch), drag, optional
collapse/expand of subtrees, edge-kind filtering — all without
breaking on §1.21 cycles.

**Exit criteria** — Operator can answer "did the lifecycle event
succeed?" from one screen for any host. db_size_bytes stays under
the per-host policy.

**Blocks on** — §1.4 (drift events) for full coverage.

---

### 1.10 Config-as-code — bidirectional git sync (provider-agnostic)

**Core goal.** Orca is the system; the operator's specific
implementation lives in a **thin configs-as-code git repo**
(meerkat is one such repo — the user's own instance — but orca
itself is generic; any operator runs their own equivalent).
Bidirectional sync between orca state and the repo is a
first-class capability.

**`config` is just another orca resource.** Same one-tool-per-
resource shape as `system`, `env`, `secret`, etc. — callable
over the mesh from any peer:

- `orca config detail <key>`
- `orca config upsert <key> <value>` (or `--file …` for a TOML
  blob) — create-or-replace by key
- `orca config delete <key>`
- `orca config list [--host <h>] [--drift]`
- `orca config apply <change_id>` — the user-triggered apply
  per the HARD RULE.

All of these resolve to the same underlying `config_store`
rows that already ship (`projects/db/src/config_store.rs`); the
git remote is just the durable mirror.

**Provider-agnostic.** Any git remote works: GitHub, Gitea,
GitLab, self-hosted bare git, file://, ssh://. A future
self-hosted Gitea instance running on the pod is a first-class
target — the same code path that talks to GitHub talks to
Gitea. Provider-specific features (webhooks, app tokens, PR
APIs) live behind a `GitProvider` trait (see meerkat memory
`feedback_git_provider_api.md`); the **core sync loop uses
plain git** (libgit2 / git CLI) so the offline / airgapped /
DR paths never depend on a hosted provider being reachable.

**Two directions:**

1. **orca → repo** — Every operator-triggered `config update`
   or `config delete` (via CLI / MCP / UI / WASM) that modifies
   declared state writes a commit to the linked repo. Each
   commit carries the operator identity, the change reason,
   and the `change_id` from the §1.11 apply flow. Push happens
   via plain git over the configured remote URL — no provider
   API required.
2. **repo → orca** — A new commit on the linked branch is
   detected (provider webhook *if available*, polled `git
   fetch` on a tick otherwise), diffed against current declared
   state, and surfaced via `orca config list --drift`. **No
   auto-apply** per the HARD RULE — drift detection + §1.20
   notification only; operator runs `orca config apply
   <change_id>`.

The repo holds **thin configs only** — no data, no cleartext
secrets (handle references only per §1.11), no per-stack
volumes. Backups (§1.8) handle data; the repo handles intent.

**Scope:**

- Remote linkage as a config row itself: `orca config update
  remote.url <git-url>` + `remote.auth <secret-handle>` —
  no new `repo add` verb. Provider is auto-detected from the
  URL (github.com → GitHub, gitea.* → Gitea, etc.) but defaults
  to "generic git" — sync works even when provider detection
  fails.
- One-way bootstrap: `orca config import <path>` accepts an
  existing repo of compose stacks + per-host configs (today's
  meerkat) and walks it into declared state.
- Outbound writer: every apply persists the resulting state
  diff as a commit, pushed via plain git. Atomicity per
  change_id.
- Inbound watcher: webhook receiver where the provider supports
  it; polled `git fetch` every N minutes as the universal
  fallback. Polling is the default — webhooks are optimization.
- Conflict policy: external push and an in-flight orca apply
  both touching the same key → §1.20 ack-required event;
  operator picks a side.
- Repo-side schema: `config/<host>/*.toml` is the canonical
  layout (consistent with §1.2 updates.toml, §1.7 shares.toml,
  §1.14 dns/firewall/dhcp, §1.16 power.toml).
- Identity: commits use the orca operator identity (§1.11
  unified user identity), not a service account, so audit
  trails carry across to whichever provider hosts the repo.
- `GitProvider` trait for provider-specific surfaces (PR open,
  webhook register, deploy-key mint) — orthogonal to the core
  sync loop. Implementations: github, gitea, gitlab, generic
  (no-op for everything beyond plain-git push/fetch).

**Future: self-hosted Gitea.** Running our own Gitea instance
(orca-managed, on the pod) is a goal but not a prerequisite
for §1.10 — the provider-agnostic core means a future
Gitea-on-pod just slots in as another `GitProvider` impl, and
the same configs-as-code repo can be migrated by changing
`remote.url`. Tracked as a Phase 2 item.

**Shipped** — Local config store rows (`projects/db/src/config_store.rs`).
No remote sync, no git wiring, no provider trait.

**Missing** — `config` tool surface (`get`/`update`/`delete`/
`list`/`apply`) over the mesh, outbound commit writer over plain
git, polled inbound fetcher, optional provider webhook
receivers, `GitProvider` trait + per-provider impls,
conflict-resolution UX, `config import` for existing repos,
schema for repo-side TOML layout.

**Exit criteria** — `orca config update remote.url <url>`
links a repo on any git provider (or a bare git endpoint).
Every operator-triggered `config update`/`delete` commits a
corresponding change to that repo via plain git push. A direct
push to the repo surfaces as drift in `orca config list
--drift` within one tick (polled fetch) and emits a §1.20
event. `orca config import` walks today's meerkat repo into
orca's config store without losing fidelity. A self-hosted
Gitea instance and a GitHub-hosted repo are interchangeable
from orca's perspective. Every `config` verb is callable over
the mesh from any peer.

**Blocks on** — §1.11 (apply flow + secret handle resolution
for git auth), §1.20 (drift notifications + apply prompts).
Hard prerequisite for retiring hand-edited meerkat under the
parity rule.

**Detail** — See `feedback_git_provider_api.md`
(provider trait vs libgit2 split).

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
Bundle-handle pattern (`op://Orca/automations.echo` resolves the
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

**Blocks on.** Nothing — §1.11 ships standalone. §1.1 is the
first concrete consumer (per linear work order, §1.11 lands
*before* §1.1). Unblocks §1.1, §1.2, §1.5, §1.8, §1.14 — all
need real envs/secrets.

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

**Blocks on** — §1.8 (escrow infrastructure), §1.12a (basic
enrollment).

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
"host left the pod" cascade; **send-side re-share** of escrowed
key fragments held by the departing host (the receive-side rejoin
counterpart lives in §1.12b); safety gate (refuse if removing the
host drops escrow quorum below k-of-n).

**Exit criteria** — Removing a host is a single verb that leaves
zero orphaned references in any other reconciler's state. Audit
shows when/why/by-whom.

**Blocks on** — §1.12a (enrollment surface defines the shape of
decommission); §1.12b for the escrow-re-share path.

---

### 1.14 Network reconciler — DNS / firewall / DHCP / switches / APs

**Scope** — Declarative network state across every device with
an API:

- **OPNsense** (router) — firewall rules, gateway list,
  routing table, ARP/NDP, DHCP reservations.
- **Adguard** (DNS) — A/AAAA/CNAME records. Dual-stack
  required (per `project_dns_dualstack`). Specific records
  beat wildcards.
- **MikroTik (RouterOS)** — managed switch + secondary router
  surface. REST API (v7) / legacy API protocol. Bridge port +
  wireless tables for §1.21 `connects` edges.
- **UniFi (Network Controller)** — managed APs + UniFi
  switches. HTTP API. SSID config, port profiles, client →
  AP mappings.

Gateway-monitoring policy per `feedback_opnsense_gateway_monitoring`
(don't ship `monitor_disable=1` defaults). All four plugins
also feed §1.21 relationship graph as their primary edge
source.

**Shipped** — Nothing yet. All four plugins are Tier 1 slots
(`projects/plugins/{opnsense,adguard,mikrotik,unifi}/`) — not
yet created.

**Missing** — All four plugins (Tier 1 in-process), declarative
TOML schemas under `config/<host>/{dns,firewall,dhcp}.toml`
plus `config/network/{switches,aps}.toml`, dual-stack
validation, edge emission into §1.21, drift surface.

**Exit criteria** — Every record currently in Adguard + every
OPNsense rule is declared in the config repo. Every nickname
from §1.6 resolves dual-stack as `<nickname>.<fqdn.domain>`.
Adding a new host gets DNS + DHCP + firewall holes in one
operator-driven apply.

**Blocks on** — §1.6 (nickname → FQDN mapping), §1.11 (secret
backend for OPNsense API tokens). Apply prompts route through
§1.20 (firewall changes are the canonical `requires_ack` event
class).

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

### 1.16 UPS ecosystem — power monitoring, ordered shutdown, recovery boot

**Current state** — UPSs are USB-attached directly to **echo**
and **alpha**. Each host runs its own apcupsd / NUT instance
and shuts itself down on low battery. Other hosts on the same
UPS circuits have no awareness — they hard-die when AC drops.
Battery window per UPS is ~30 min, so there is real time to
shut the fleet down cleanly if the orchestration exists.

**Scope** — A full UPS ecosystem in three layers:

1. **Power topology map** — declarative `config/power.toml`
   describes which UPS feeds which hosts, which host has the
   UPS-USB link (the "UPS coordinator" for that circuit), and
   the per-host **shutdown order** + **boot order** (workloads
   stop first; storage gateways second; coordinator last;
   inverse on boot). Reconciler validates the graph (every host
   maps to at least one UPS, every UPS has exactly one
   coordinator, no cycles).
2. **NUT listener + broadcast** — coordinator runs NUT (or
   apcupsd), watches battery state, broadcasts events
   (`ac_lost`, `battery_low`, `runtime_below_threshold`) over
   the mesh to all peers on the same circuit. Peers act on
   their declared role: workloads start drain hooks, storage
   gateways flush + go read-only, etc. Coordinator self-shuts
   last when runtime drops below the per-circuit floor (default
   3 min reserve so the shutdown command itself has headroom).
3. **Recovery boot** — when AC returns and the coordinator boots,
   it issues Wake-on-LAN to its dependents in declared boot
   order, with a §1.5 inner-service health gate between tiers.
   Hosts that can't WoL (laptops, hosts with WoL disabled in
   BIOS) get flagged in the topology map as "manual recovery"
   so the operator sees what's missing.

The 30 min battery window is the design budget: drain hooks +
ordered shutdown across the fleet must complete with reserve
to spare. Dry-run mode (`orca power simulate ac-loss`) walks
the graph and reports projected runtime cost per host without
actually shutting anything down.

**Shipped** — Per-host apcupsd/NUT on echo + alpha (meerkat
shell + systemd). No cross-host awareness. `host-lifecycle.md`
§4 has the original design sketch — superseded by this item's
scope.

**Missing** —

- Power-topology TOML schema + reconciler.
- NUT-integration plugin (Tier 1; one driver shared by apcupsd
  + NUT since both expose similar event streams).
- Mesh event class for power events (rides on the dispatch
  surface that already exists; adds `power.*` event kinds).
- Per-host drain-hook executor — shared with §1.2 reboot hook
  chain.
- WoL sender + per-host MAC + interface declaration; "manual
  recovery" flag for non-WoL hosts.
- `orca power {status,simulate,test-shutdown}` verbs.
- Audit trail: every shutdown/boot triggered by UPS events is
  recorded with the precipitating UPS state.

**Exit criteria** — Pulling the wall plug on the echo UPS
shuts the echo-circuit fleet down in declared order within
budget (workloads → gateways → coordinator), with no hard
power-offs. AC restoration brings the same hosts back via WoL
in inverse order, each tier gated on §1.5 health. The alpha
circuit behaves identically. `orca power status` shows AC
state + estimated runtime + dependent-host list per circuit.
Per-host apcupsd shell config retired under the parity rule.

**Blocks on** — §1.2 (shared drain-hook executor), §1.5
(post-boot health gate). Power-topology map can be drafted
in parallel with §1.1.

**Open decisions** — see open decision #6 (coordinator
selection per circuit); add: should the coordinator role
failover if the UPS-USB host itself is offline at AC-loss
time? (Today: hard-die. Future: secondary peer watches
heartbeat and acts as backup coordinator — needs design.)

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
credentials). Hard prerequisite for §1.21 orca-cloud (offsite
replica enforces the same policy).

---

### 1.18 `orca system doctor`

**Scope** — Health-check verb: peer reachability, cert expiry,
drift summary, secret-backend reachability. Pre-flight check
before any reconciler `apply`.

**Shipped** — Nothing first-class. Today health is surfaced
piecemeal (`orca pod peers`, install-report, manual log grep).

**Missing** — `system.doctor` `#[orca_tool]` with per-check
registry, JSON output, severity levels, integration into
`orca apply` preflight.

**Exit criteria** — `orca system doctor` returns a single
pass/fail per host with itemized failures; reconciler `apply`
refuses to start with `doctor` failures unless `--force`. A
failing check is the canonical `requires_ack` event class for
§1.20 (notifications).

**Blocks on** — None (consumes shipped primitives).

---

### 1.19 `orca system uninstall`

**Scope** — Promote the existing `cmd_uninstall_report` helper
(`projects/system/src/install.rs:191`) to a proper `#[orca_tool]`
surface. Pairs with §1.13 host decommission.

**Shipped** — In-process helper exists; no tool surface.

**Missing** — `#[orca_tool]` registration, CLI verb wiring,
audit-emitting wrapper, `--keep-state` flag.

**Exit criteria** — `orca system uninstall` is callable across
all four surfaces; symmetric with `system.install`.

**Blocks on** — None.

---

### 1.21 Resource relationships + network/topology graph

**Scope.** A directed, cycle-tolerant **typed-edge graph**
across every nicknamed resource (per §1.6). Edge kinds:
`hosts`, `connects`, `routes`, `exposes`, `depends_on`,
`replicates`, `backs_up`, `escrows` (extensible). Walkable
both ways. **Strange loops are first-class** — orca explicitly
models cases like "golf hosts the opnsense VM which routes
golf" without breaking. Full data model in
`feedback_resource_relationships_and_graph`.

This drives:

- **Network map UI** — node-link diagram; physical + logical
  layers; filter by edge kind; cycles drawn honestly.
- **Failure-domain queries** — "if X dies, what goes with it?"
  Preflight for any reconciler apply that takes a node offline.
- **Ancestry walks** in either direction — "what exposes
  sonarr?" / "what does opnsense reach?" — exposed on
  CLI/MCP/REST/WASM (no domain needed, per §1.6 local-by-default
  rule).

Edges come from:

- **Plugins first** — every network device with an API gets a
  Tier 1 plugin that emits edges directly. MikroTik (RouterOS
  REST + bridge/wireless tables → `connects`), UniFi (Network
  Controller API → AP/switch/client mappings → `connects` +
  `routes`), OPNsense (REST → `routes` + ARP/NDP), Adguard
  (DNS cross-ref). Plus the shipped Proxmox / Docker / Unraid
  collectors for `hosts`. §1.17 contributes `replicates`,
  §1.8 contributes `backs_up`, §1.12b contributes `escrows`.
- **Manual operator declaration** *only* for things no plugin
  can observe (unmanaged dumb switches, specific physical
  cable runs the operator wants documented). Edges carry a
  provenance field — plugin-emitted edges refresh on each
  tick; manual edges are never auto-deleted.

**`config` shape** (one-tool-per-resource):

- `config.update relationship.<id> kind=<k> from=<nickname> to=<nickname> [meta…]`
- `config.delete relationship.<id>`
- `config.list --relationships [--kind <k>] [--from <n>] [--to <n>]`
- `config.walk <nickname> --direction <down|up> [--kinds <k1,k2>]`
  — bounded walk with cycle detection, returns the visited
  sub-graph as typed edges.

**Shipped.** Nothing as a graph. `parent_peer_id` on host_status
hints at one `hosts` edge per peer but isn't generalized.
Topology collector observes hosts/CTs/containers but doesn't
materialize edges.

**Missing.** `relationships` table in the config store;
`RelationshipKind` enum; auto-inference adapters per source
(topology collector, §1.14, §1.17, §1.8, §1.12b); manual-edge
verbs; bidirectional bounded-walk query; cycle-detection;
network-map UI view; failure-domain preflight integration with
every reconciler `apply` path.

**Exit criteria.** The two walk examples from the canonical
memory both return correct typed-edge chains by query:

- `opnsense → mikrotik → delta → echo → syncthing`
- `golf → opnsense → mikrotik → 1gb switch → access point → hotel`

Cycle case (golf ↔ opnsense) is queryable in either direction
without infinite recursion. Network-map UI renders all of the
above with cycles visible. Failure-domain preflight catches
"removing golf would also drop opnsense + everything opnsense
routes."

**Blocks on.** §1.6 (nicknames are the node identity), §1.10
(graph persists in config repo), §1.11 (apply flow for manual
edge edits).

**Unblocks.** §1.9 network-map UI; failure-domain preflight
across every reconciler in Phase 1; Phase 2 service-catalog
(uses `depends_on` for ordering).

**Detail.** `feedback_resource_relationships_and_graph` memory
(canonical).

---

### 1.22 Service-type inference + plugin auto-binding

**Scope.** When orca observes a new LXC / VM / Docker
container, it **infers what service is running** (sonarr, plex,
home-assistant, opnsense, etc.) and **auto-binds the matching
plugin** without operator wiring. Multi-signal detection with
confidence levels; high confidence auto-binds, medium emits a
§1.20 ack-required prompt, low marks as `unknown`. Full data
model + signal ordering in
`feedback_service_type_inference_and_autobinding`.

This is what makes the plugin surface useful at scale: once
bound, a resource automatically gets action verbs
(`orca <service> <verb>`), telemetry, dashboards, §1.21
`depends_on` edges, §1.5 service-aware health, and §1.8 native
backup — all for free, no manual wiring.

**Signals (combined for confidence):** image / LXC template
name; container labels (`orca.service` override + standard
OCI labels); nickname; exposed ports; HTTP API fingerprint
(`/api/v3/system/status` etc.); in-guest process listing as
last resort; mDNS / DNS-SD where applicable. Plugins declare
their own fingerprints — adding a plugin extends detection
automatically.

**Confidence behavior** (per user-triggered-only rule):

- **High** (≥2 strong independent signals): auto-bind +
  audit-log the signals.
- **Medium**: §1.20 notification with `apply <change_id>` link
  ("I think this is sonarr v4 — bind?"). Operator confirms.
- **Low**: mark `unknown` with detected hints; operator
  binds via `config.update <resource>.service <type>`.

Override is always available; manually-bound resources have
provenance `manual` and detection never overwrites them.
Re-evaluation runs on image/template change.

**Shipped.** Topology collector observes
hosts/CTs/VMs/containers. Per-service openapi plugins exist
(arr stack, plex, jellyfin, etc.). No inference layer, no
auto-binding.

**Missing.** Detection engine in the topology collector; plugin
fingerprint declarations in `orca-plugin.toml` + Tier 1
registration; confidence scoring; auto-bind audit; medium-
confidence prompt wiring through §1.20; multi-plugin binding;
explainable `config.list <resource>` showing the signals that
drove the binding; manual override + provenance.

**Exit criteria.** A fresh meerkat install with every Tier 1
plugin enabled auto-detects + auto-binds every existing arr /
plex / jellyfin / homeassistant / opnsense / dockge / etc.
instance with no operator wiring. `config.list --unbound`
shows the long tail of un-inferred resources for manual
review. Every bound resource exposes its plugin's full action
+ data + dashboard surface immediately.

**Blocks on.** §1.6 (nicknames as resource identity),
§1.10 (config-as-code stores bindings), §1.11 (apply flow for
medium-confidence prompts), §1.20 (notification surface).
Soft-blocks on per-plugin fingerprint declarations (every
plugin landing gets a fingerprint block).

**Unblocks.** Phase 2 service-surface parity — once auto-bind
works, the per-service action / data / dashboard work is just
plugin development.

**Detail.** `feedback_service_type_inference_and_autobinding`
memory (canonical).

---

### 1.20 Notifications — unified dispatcher + escalation

**Scope** — `projects/notify/` crate exposing one generic `Event`
shape and a single `Backend` trait. Multi-backend dispatch (ntfy,
email, Slack, Discord, SMS, generic webhook) all rendering the
same event. **Escalation chains** for events that require ack:
Discord first, email if no ack, SMS / Pagerduty after that.
**Ack ≠ approve** — acking silences escalation; approving
triggers `orca apply`. Both surfaces can coexist on one event.

Drives the user-facing edge of every Phase 1 capability: drift
(§1.4), rotation (§1.11), lifecycle events (§1.9), host updates
(§1.2), restore outcomes (§1.8), reconciler apply prompts
(§§1.1/1.7/1.14), inner-service health (§1.5).

**Shipped** — `projects/plugins/ntfy/` thin library — one
backend, no abstraction, no escalation. Heartbeat + send only.

**Missing** —

- `projects/notify/` crate scaffold + `Event` + `Backend` trait
  + dispatcher.
- Rendering matrix per backend (Slack Block Kit, Discord embeds,
  email html, ntfy headers, SMS truncation).
- Routing engine (class / severity / host → backends; TOML
  declarative).
- Escalation engine: `Event` carries `requires_ack` / `retrigger`
  / `escalation`; each `EscalationStep` has its own `retrigger`
  (re-fire cadence at current step) + `advance_after` (time before
  moving to next step); `max_total` ceiling on the chain;
  persistent `(event_id, step_index, last_fired_at, step_entered_at)`
  in SQLite (survives daemon restart); ack-stops-chain across all
  surfaces; parallel-step ack coordination; 5-minute de-dupe
  window on `(class, host, source, correlation)`. Events with
  `requires_ack` but no `escalation` re-fire on `Event.retrigger`
  forever (bounded by 24h default ceiling).
- Backends: ntfy (port existing code), email (SMTP), Slack
  (webhook + Events API), Discord (webhook + Interactions), SMS
  (Twilio first), generic webhook.
- **Native push notifications** (deferred until Phase 3 mobile +
  PWA apps exist; sender lives in §1.21 orca-cloud): Web Push
  (VAPID) for PWA, APNs for iOS, FCM for Android — same `Event`
  shape, same `Backend` trait, action buttons map to the
  platform's interactive-notification surface. Per-operator
  device-token registration via `orca user device link <platform>
  <token>`. Tracked in §1.20's backend list so the abstraction
  is push-aware from day one even though the adapters land
  alongside the apps.
- Authenticated apply-links for non-interactive surfaces (email,
  SMS, mobile push) — short-TTL signed URLs into the same `orca
  apply` path as CLI. **Single signer shared with §1.21
  orca-cloud** (push backends mint links via the same primitive;
  no second link-signer).
- `orca ack <event_id>` CLI verb + UI dismiss + interaction
  endpoints for chat platforms.
- `orca user link <backend> <id>` for mapping chat identities to
  orca operators (interactive backends need this).

**Exit criteria** —

- Every Phase 1 emitter (drift, rotation, lifecycle, reconciler)
  calls `notify::emit(Event)` — no caller knows or names a
  specific backend.
- Critical events with `requires_ack = true` escalate through
  the configured policy and **always** reach the operator
  within `max_total`, or terminate gracefully with a "gave up"
  message.
- Acking on any surface (button, link, SMS reply, CLI, UI) stops
  the chain across all surfaces; other surfaces get an
  "Acked via X by @user" update.
- `projects/plugins/ntfy/` is retired; ntfy is `projects/notify/backends/ntfy.rs`.
  Phase 0 shipped-table row for ntfy is updated to point at
  `projects/notify/` as part of this item's close-out.
- No special-case rendering for email — it follows the same
  `Event` shape, with action buttons rendered as authenticated
  signed links.

**Blocks on** — None for the core dispatcher + non-push backends
(ntfy / email / Slack / Discord / SMS / webhook). The push
backends (Web Push / APNs / FCM) block on §1.21 (orca-cloud as
the publicly-reachable sender) and on Phase 3 (mobile + PWA apps
exist to receive). Can ship the dispatcher in parallel with
reconciler work since emitters land progressively as each Phase 1
item closes.

---

## Phase 2 — Service surface parity

Begins only after Phase 1 closes. Each meerkat script + plugin +
compose stack maps to its orca successor.

Headline items, in roughly the order they unblock fleet operation:

- **Caddy plugin** — routes from `compose/caddy/routes/*.toml`,
  mTLS to orca upstreams, fan-out across edge hosts.
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
  alpha→echo replication path.
- **OSS media plugins** — arr stack / qBittorrent / SABnzbd as
  first-party plugins under a separate identity per
  `feedback_oss_media_terminology.md`, `feedback_oss_media_separate_identity.md`.
- **Unified user identity** — SMB login == orca login (the
  *identity unification* layer, distinct from §1.11's secret
  *storage* layer); baseline password rotation policy across the
  fleet. Detail: `feedback_unified_credentials.md`.
- **Alternative deploy targets + orca-cloud offsite** —
  containerized orca (same binary in Docker) + VPS-hosted
  orca-cloud as backup destination, escrow holder, and
  push-notification broker. Deferred until Phase 1 core is
  solid; full scope captured in orca memory
  `project_deploy_targets_and_orca_cloud.md`.

---

## Phase 3 — Deferred until parity

Explicitly out of scope until Phase 1 is closed. Listed so we
can say "no" with a reason.

| Item | Reason deferred |
|---|---|
| Caddy plugin first-class implementation | Phase 2. Routes today are operator-managed; not on the parity critical path. |
| Example downstream plugin | `feedback_no_consumer_strings_in_orca.md` — a second consumer of the plugin contract; only useful once the contract is hard. |
| Namespace consolidation (`docker-runtime.*` → `docker.runtime.*`, etc) | Cosmetic. Touching every call site costs more than the readability win. |
| Frontend polish (Mantine strip, a11y audit) | `project_frontend_deferred_todos.md`. UI must reflect server state (`feedback_ui_must_reflect_server_state.md`) — that's the only frontend rule that matters during Phase 1. |
| Advanced PKI revocation, CRL, CT log | Mutual trust + cert rotation already shipped; revocation lift is post-parity. |
| iOS / Android standalone | `project_mobile_as_standalone_orca.md`. UniFFI plan locked; not blocking lifecycle. |
| `dev:<branch>` channel | `project_dev_channel_plan.md`. Locked 2026-05-12, deferred. |
| 100% test coverage ratchet | `docs/coverage-baseline.md`. Floor=51 is a hard CI gate (blocks PRs below it); ratchets up per the touched-files-reach-100% rule. |

---

## Cross-cutting standing rules

These apply at every phase. **Pointer list only** — the rules
themselves live in memory (canonical source). If a rule needs
to change, edit the memory; this list just names which rules
are load-bearing across phases.

- User-triggered changes only — `feedback_user_triggered_changes_only`
- Parity rule before retirement — meerkat `feedback_parity_rule`
- Personal 1Password only — meerkat `feedback_personal_1password_only`,
  orca `feedback_op_personal_only`
- Native backup APIs first — meerkat `feedback_native_backup_apis`
- Storage abstraction, no host names in targets — meerkat
  `feedback_storage_abstraction`
- In-repo migrations + schema-evolution discipline — orca
  `feedback_no_data_migrations_for_name_cleanups`, `project_db_squash`
- orca-vs-consumer identity / no consumer-specific strings (e.g.
  "meerkat") in orca core — orca `feedback_no_consumer_strings_in_orca`,
  `feedback_orca_vs_meerkat_identity`
- One tool per resource (sub-domains OK when they're real domains) —
  orca `feedback_one_tool_per_resource`
- Orca self-updates without sudo — orca `feedback_orca_self_updates_no_sudo`
- Never blind-trust caller identity — orca `feedback_zero_trust_no_blind_trust`

---

## Open decisions blocking the roadmap

1. **Release signing — cosign vs minisign.** H1 from
   `project_security_hardening_v1` deferred. Install hardening
   (§1.3) cannot close until the verifier is wired into install.sh.
2. ~~Bootstrap.toml repo discovery vs GitHub App.~~ **Resolved
   2026-06-01:** no `bootstrap.toml`. First-run flow is install
   + (optional) `orca <role> restore <snapshot>` from §1.8
   backup. GitHub App stays in `orca-v1-scope.md` §3.6 as a
   deferred Phase 2/3 enhancement, not a Phase 1 blocker.
3. **Drift policy granularity per LXC.** `preserve-runtime-additions`
   on `mp*` is the media-a-style default — is there a CT where we
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
5. **§1.10 — config-as-code git sync** (bidirectional;
   outbound commits + inbound diff-on-push; provider-agnostic).
   Lands right after §1.11 so applies start producing commits
   from the very first reconciler.
6. **§1.6 — resource nicknames + grouping** (uniqueness +
   default assignment + cascade-on-rename + group membership;
   FQDN exposure is optional). Lands before §1.1 so every new
   resource gets a nickname from day one.
7. **§1.21 — resource relationships + topology graph**
   (typed-edge directed graph, cycle-tolerant, bidirectional
   walks, network-map UI). Lands here so every reconciler that
   follows contributes its own edge kind from day one.
8. **§1.22 — service-type inference + plugin auto-binding**
   (multi-signal detection with confidence; auto-bind on high,
   ack-prompt on medium, manual on low). Lands here so the
   LXC reconciler's first new CT gets auto-bound to its plugin.
9. **§1.1 — LXC + VM reconciler** (media-a-driven; first concrete
   consumer of the env+secret layer + the user-triggered apply
   pattern; uses §1.6 nicknames + §1.21 `hosts` edges + §1.22
   plugin binding).
10. **§1.4 — drift detection** (per-noun checkers, event schema,
    aggregate view). Becomes meaningful once §1.1 emits events.
    Also covers github-push drift via §1.10.
11. **§1.5 — inner-service health probes** (post-lifecycle gate
    for §1.1 + §1.2; service-aware via §1.22 plugin binding).
12. **§1.2 — host update lifecycle** (per-distro package drivers,
    GPU/accelerator drivers + DKMS, reboot hook chain, rolling
    selector — all under one `system.update` surface).
13. **§1.7 — storage server-side reconciler + runtime health**
    (nas-a exports + smb.conf + Avahi + wsdd from
    `config/nas-a/shares.toml`, plus nfs-monitor-equivalent failover).

Ships in parallel with the sequence above (no upstream blocker):

- **§1.20 — notifications dispatcher + escalation.** Lands the
  `projects/notify/` crate + `Event` + `Backend` trait early so
  every Phase 1 emitter (drift, rotation, lifecycle, reconciler
  apply prompts, restore outcomes, inner-service health) calls
  `notify::emit` from day one. Ntfy backend ports first; other
  backends land progressively. Retires `projects/plugins/ntfy/`.

Ships alongside §1.1 (preflight + symmetry):

- **§1.18 — `orca system doctor`.** Wanted as preflight for every
  reconciler `apply` from §1.1 onward.
- **§1.19 — `orca system uninstall`.** Small chore; ship whenever
  symmetric with §1.13 enrollment work.

Then in priority order without strict blocking:

- §1.8 backup plugin + native-API-first
- §1.12b pod rejoin (escrow recovery — needs §1.8 escrow infra)
- §1.13 host decommission
- §1.14 network reconciler (DNS / firewall / DHCP)
- §1.15 NTP / clock management surface
- §1.16 UPS-coordinated shutdown
- §1.17 storage replication policy
- §1.9 topology / observability minimum

Phase 2 (service surface) begins only after the top-10 sequence
above is closed.
