# Orca-as-logic-layer — meerkat migration scope

Goal: move **every piece of behavior** out of meerkat into orca. After
this work, meerkat is purely declarative: docker-compose stacks, route
TOML, host/service definitions, backup snapshots, docs. No Go binaries,
no shell automation, no MCP server — orca runs everything.

**Orca is the single control and observation surface.** Every action
(provision, configure, deploy, backup, restore, restart, view status,
view logs) goes through `orca` — CLI, MCP, REST, or WASM, all the same
underlying tool definitions. No second pane of glass.

**Meerkat is one implementation of the pattern**, not a privileged one.
The orca/meerkat relationship is: orca daemon + config repo. Any git
repo that follows the schema works — rebuy is another consumer
(per `rebuy-plugin-scope.md`), a third party could write their own.

This is the umbrella doc. The Caddy plugin
([caddy-plugin-scope.md](caddy-plugin-scope.md)) is one slice of it.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. End state

```
┌────────────────────── meerkat repo ─────────────────────┐
│  compose/                 docker-compose stacks         │
│  compose/caddy/routes/    declarative routes            │
│  proxmox/{vms,lxcs}/      host inventory                │
│  backups/configs/         backup snapshots              │
│  config/                  orca bootstrap + service defs │
│  docs/                                                  │
└─────────────────────────────────────────────────────────┘
                            │ orca --config-repo /path/to/meerkat
                            ▼
┌────────────────────────── orca ─────────────────────────┐
│  framework + integrations + plugins + scheduler + mesh  │
│  reads meerkat as config-as-code, reconciles state      │
└─────────────────────────────────────────────────────────┘
```

Meerkat after the migration contains **no executable behavior**, only
data orca consumes. Every host runs an orca daemon; nothing else
homelab-specific lives on disk.

---

## 2. What "logic" means here

Logic = anything that *does* something at runtime.

Stays in meerkat (it's config / data):
- `compose/**` docker-compose stacks and their static assets
- `compose/caddy/routes/*.toml` (per the Caddy plugin doc)
- `proxmox/{configs,lxcs,vms}/` host inventory snapshots
- `backups/configs/` backup blobs
- `config/` orca bootstrap, service registry, secret references
- `docs/`, `README.md`, `agents/` claude prompts

Moves to orca (it's behavior):
- The `meerkat/` Go binary (MCP server + connectors + update + secrets)
- All `plugins/*` (these are already orca-shaped, but ownership shifts —
  see §4.2)
- All `scripts/*.sh` and `scripts/*.py` host automation
- All per-host `scripts/<hostname>/` shell wrappers
- `scripts/meerkat.d/` subcommand dispatcher

---

## 3. Inventory: meerkat logic today vs orca

### 3.1 Go binary (`meerkat/`)

| Meerkat module | LOC | Orca equivalent | Disposition |
|---|---|---|---|
| `cmd/meerkat/main.go` | ~150 | `orca` daemon | **Retire**. MCP server on `:12050` is replaced by orca's federated MCP on `:12002`. |
| `internal/mcp` (http.go, server.go) | | `server/plugin_host` + MCP federation | **Retire**. |
| `internal/connector` (interface, registry) | | `utils/tool` | **Retire**. Orca's tool framework is the same shape. |
| `internal/connectors/docker` | | `integrations/docker/compose` (Prod) | **Retire — duplicate**. |
| `internal/connectors/dockge` | | `integrations/dockge` (Prod) | **Retire — duplicate**. |
| `internal/connectors/graphql` | | (none yet — generic GraphQL client) | **Migrate** as orca integration. |
| `internal/connectors/homeassistant` | | `integrations/homeassistant` (Prod) | **Retire — duplicate**. |
| `internal/connectors/nfs` | | `integrations/nfs` (Prod) | **Retire — duplicate**. |
| `internal/connectors/proxmox` | | `integrations/proxmox` (Prod) | **Retire — duplicate**. |
| `internal/connectors/unraid` | | `integrations/unraid` (Prod) | **Retire — duplicate**. |
| `internal/connectors/plugin` | | `server/plugin_host` | **Retire**. Out-of-process plugin dispatch is orca's job. |
| `internal/secrets` | ~200 | `mcp/secrets_service` (Prod, SQLite encrypted) | **Retire — duplicate**. Migrate stored secrets in flight. |
| `internal/config` | | `utils/config` (Prod) | **Retire**. Meerkat's `meerkat.toml` becomes orca service-registry entries. |
| `internal/update` | ~200 | (orca update path) | **Retire**. Orca owns its own release channel. |
| `internal/httpclient` | | (orca uses `reqwest` directly) | **Retire**. Trivial. |
| Total | ~4600 LOC | | All retirable once consumers move. |

### 3.2 Plugins (`plugins/*`)

These are already orca plugin manifests + binaries — they speak the
orca plugin contract. The question is **ownership**, not architecture.

| Plugin | Language | Duplicates orca integration? | Disposition |
|---|---|---|---|
| `plugins/git` | Rust | Yes (`reference plugin Prod`) | **Retire** — use orca's. |
| `plugins/docker` | Go | Yes (`integrations/docker/compose`) | **Retire**. |
| `plugins/dockge` | Go | Yes | **Retire**. |
| `plugins/proxmox` | Go | Yes | **Retire**. |
| `plugins/unraid` | Go | Yes | **Retire**. |
| `plugins/homeassistant` | Go | Yes | **Retire**. |
| `plugins/nfs` | Go | Yes | **Retire**. |
| `plugins/ntfy` | Go | Yes (`integrations/ntfy` Prod) | **Retire**. |
| `plugins/graphql` | Go | Partial | **Promote** to orca built-in (generic GraphQL is broadly useful). |
| `plugins/rest` | Go | Partial | **Promote** to orca built-in. |

Net: most of `plugins/` is a transitional layer that disappears.
`graphql` and `rest` are the only ones with unique value — promote
them into orca and delete the plugin directories.

`legacy/` is already documented as deprecated — delete on the way past.

### 3.3 Top-level scripts (`scripts/*.sh|*.py`)

| Script | LOC | What it does | Disposition |
|---|---|---|---|
| `meerkat-agent.sh` | | Host-level sudo-whitelisted dispatcher (backup/restore/health/update/info) | **Retire**. Orca daemon replaces the sudo-allowlist surface; verbs become `orca host <verb>`. |
| `meerkat-sync.sh` | | Cron'd git pull of meerkat repo | **Retire**. Orca scheduler + git plugin. |
| `install-meerkat-sync.sh` | | Installs the cron job above | **Retire** with parent. |
| `meerkat.d/` | 8 files | Subcommand dispatcher (backup/restore/list/status/sync/update/registry/common) | **Retire**. Each becomes an orca verb. |
| `setup-host.sh` | 303 | New-host bootstrap | **Migrate** to `orca host bootstrap` (rust-native). |
| `backup-configs.sh` | | Backs up infra configs into `backups/configs/` | **Migrate** to `orca backup config` job. |
| `restore-config.sh` | 294 | Inverse | **Migrate**. |
| `nfs-monitor.sh` | 545 | NFS health watchdog | **Migrate** — already exists in orca as `integrations/nfs` health checks; verify parity, then retire. |
| `pbs-backup-hook.sh` | 133 | PBS pre/post-backup hook | **Migrate** to orca pbs integration hook surface. |
| `pbs-mount-watchdog.sh` | 50 | Re-mounts PBS NFS if stale | **Migrate** — folds into NFS health loop. |
| `sync-proxmox-configs.sh` | 55 | Pulls Proxmox node configs into repo | **Migrate** to scheduled `orca proxmox config snapshot`. |
| `fix-unbound-forwarding.py` | 108 | OPNsense config patch | **Migrate** to `orca opnsense` integration (or keep as a one-shot if it's truly one-shot). |
| `fix-wireguard-config.py` | 164 | WireGuard config repair | **Same as above** — decide one-shot vs ongoing. |
| `willow-nfs-release.sh` | 26 | Unraid NFS release on shutdown | **Migrate** into orca unraid integration. |
| `migrate.sh` | | Repo-internal migration (likely obsolete) | **Audit**, probably delete. |
| `autofs-switchback.sh` | | Reverts autofs change | **Migrate** or delete (one-shot). |

### 3.4 Per-host scripts (`scripts/<hostname>/`)

| Dir | Contents | Disposition |
|---|---|---|
| `scripts/freyr/` | freyr-mgmt.sh, backup/restore/update, crond.start, wait-nfs.start | **Migrate** wholesale into orca's host model. The freyr "noun" gains verbs. |
| `scripts/baldur/` | backup-appdata.sh, nfs-monitor.conf | **Migrate** — script becomes orca verb; `.conf` becomes config data (stays in meerkat). |
| `scripts/{maple,willow}/` | appdata backup + chown fix | **Migrate** — chown ops become `utils/fs/perms` calls (already on orca's roadmap per orca-v1-scope §3.x). |
| `scripts/{thor,frigg,pbs}/` | nfs-monitor.conf + maybe one script | **Split** — `.conf` stays as data; any `.sh` migrates. |
| `scripts/openwrt/` | PIA refresh, router aliases | **Migrate** to orca openwrt integration (new). |
| `scripts/opnsense/` | PIA refresh + watchdog, backup/restore, aliases | **Migrate** to orca opnsense integration (likely new). PIA-watchdog becomes a scheduled job. |

### 3.5 GitOps loop — orca polls the config repo

Pull-based reconciliation. When orca is installed on a host and
pointed at a config repo (meerkat or otherwise), it:

1. **Polls** the repo on a schedule (default: every N minutes) via
   the host's git-provider API, not by shelling out to `git`. See
   §3.5a for the provider abstraction.
2. **Diffs** the repo's desired state against the local config store
   and the live system (services running, mounts present, routes
   loaded, packages installed, users/keys/sudo policy in place).
3. **Plans** the set of actions to converge — same shape as
   `terraform plan`. Visible via `orca plan` before apply.
4. **Applies** in dependency order: bootstrap → packages → mounts →
   services → routes → schedules. Per-tool ACL gates which actions
   the GitOps loop is allowed to run unattended vs which require
   operator approval (e.g., destructive ops always need approval).
5. **Reports** outcome to ntfy + status surface; on failure leaves
   prior state intact and alerts.

Two write directions:

- **Pull-run** (default): repo is source of truth. Orca converges
  hosts toward it. Drift is reported and (optionally) corrected.
- **Push-run**: imperative mutations via `orca <verb>` or MCP land
  in the runtime overlay. If `persist = repo`, orca commits the
  change back through the same git-provider API (creates a commit
  via the contents/git-data endpoints, or opens a PR for changes
  that require review). Same loop then re-applies. This is how
  interactive operations graduate to source-of-truth without
  leaving git out of the picture.

Per-host scope:

- Each orca daemon polls **its own slice** of the repo (its host
  entry + services scheduled to it). Avoids every host re-applying
  every config.
- Cluster-wide things (route table, mesh ACLs, secrets layout) go
  through baldur (or whichever peer is elected leader for that
  resource — out of scope here, track separately).

This is the homelab equivalent of Flux / Argo CD; orca is the
agent, meerkat is the manifests. Standard GitOps tradeoffs apply:
the repo IS the change log; rollback is a `git revert`; manual
edits on hosts are drift and will be flagged.

Already on orca's roadmap as §3.2 (git sync — pull-run + push-run,
sensitive-field stripping + entropy scanner) per orca-v1-scope.
This doc pins meerkat as the first consumer of that capability.

### 3.5a Git-provider abstraction

A config repo is `git+<provider>://<owner>/<repo>[@ref]`. Orca
talks to the provider's HTTP API, not to a remote over SSH/HTTPS,
for everything except large-blob fetches. Rationale:

- API tokens (especially repo-scoped fine-grained ones) are easier
  to provision, rotate, and revoke than SSH deploy keys, and they
  carry per-operation permissions (read contents, write contents,
  create PR, etc.) which fit per-tool ACLs cleanly.
- API calls give us etags/conditional requests for cheap polling,
  webhook signatures for push-driven reconciliation, and structured
  errors instead of `git` exit codes.
- We get commits, branches, PRs, and reviews as first-class API
  objects — orca can author commits without ever needing a working
  tree on the orca-controller side.

Provider trait (sketch):

```rust
trait GitProvider {
    async fn head(&self, repo: &RepoRef) -> Result<Sha>;
    async fn tree(&self, repo: &RepoRef, sha: &Sha) -> Result<Tree>;
    async fn read_file(&self, repo: &RepoRef, sha: &Sha, path: &str) -> Result<Bytes>;
    async fn commit(&self, repo: &RepoRef, branch: &str, changes: &[FileChange], msg: &str) -> Result<Sha>;
    async fn open_pr(&self, repo: &RepoRef, head: &str, base: &str, title: &str, body: &str) -> Result<PrRef>;
    async fn webhook_verify(&self, headers: &Headers, body: &[u8]) -> Result<WebhookEvent>;
}
```

Implementations (priority order, all rust-native):

| Provider | Crate / approach | Notes |
|---|---|---|
| GitHub | `octocrab` | First impl. App auth + fine-grained PAT both supported. |
| Gitea / Forgejo | `gitea-sdk` or hand-rolled REST | Self-hosted option. API closely mirrors GitHub's contents/git-data endpoints. |
| GitLab | `gitlab` crate or hand-rolled | Adds when needed. |
| Generic git (fallback) | `gix` (gitoxide) | For raw `git+ssh://` or `git+https://` repos with no provider API. Read-only polling by fetching refs; writes via push. Last resort. |

Auth/credential storage: tokens live in orca's secrets store
(already Prod). One token per (provider, repo, scope) — never a
shared org-wide PAT.

Working tree on the host: orca **also** maintains a local checkout
under `${ORCA_DIR}/repos/<id>/` for the apply step — docker-compose,
caddy config, systemd units etc. need files on disk. That checkout
is materialized from the API responses (no `git pull`), so the local
copy is always a clean reflection of a known SHA. The fallback `gix`
provider does use a real working tree.

Detection: the repo URL's host determines the provider unless
overridden in bootstrap (`provider = "gitea"` for self-hosted on a
non-obvious domain).

Webhooks (optional, post-§3.6 github-app — and the equivalent for
Gitea/GitLab): each provider impl exposes `webhook_verify`. Orca
runs one HTTPS endpoint per provider type that accepts pushes,
verifies signature, and triggers an immediate poll on the affected
repo. Falls back to scheduled poll if webhooks aren't configured.

### 3.6 Provisioning / day-zero setup

Everything that today is a "first, run this script to set up the
host" step is also logic that belongs in orca. The scope of "logic"
includes **provisioning**, not just runtime behavior.

**Bootstrap is minimal.** When orca first lands on a host it does
only what it needs to be a functioning peer: install the binary,
create the service user, grant the minimum permissions to run, pair
into the mesh. Nothing else. See
[install-bootstrap.md](install-bootstrap.md) for the exact bootstrap
shape.

Everything below is **per-host config that the reconciler applies**
once the host is paired — declarative in the config repo, not flags
on the bootstrap command. The reconciler picks up the config on the
GitOps loop (§3.5) and acts on it. Bootstrap never invokes these
directly.

- **Users / SSH keys / sudo / doas**: `config/<host>/users.toml`
  lists users, their public keys, group membership, and sudo/doas
  policy. Orca reconciles: creates missing users, lays down
  `~/.ssh/authorized_keys`, writes `/etc/sudoers.d/<name>` (or
  `/etc/doas.d/<name>`), removes users that fall out of the file.
  Key rotation is a PR. Revoking access is a git commit.
- **Docker**: `config/<host>/docker.toml` declares the host wants
  docker. Reconciler installs docker engine + compose plugin via
  the host's package manager, enables the service, adds svc user to
  the `docker` group. Per-distro logic (apt/apk/pacman) lives in
  the rust implementation, not in shell. → `orca host docker reconcile`.
- **NFS client/server install + configure**: `config/<host>/nfs.toml`
  declares exports, mounts, and client/server role. Reconciler
  installs the right packages, writes `/etc/exports` or fstab
  entries, probes mounts. → `orca host nfs reconcile`. Not invoked
  by bootstrap.
- **Tailscale**: `config/<host>/tailscale.toml` declares the auth
  key reference and tailnet settings. Reconciler installs the
  package, runs `tailscale up` with the resolved auth key. →
  `orca host tailscale reconcile`.
- **PBS client**: declared per-host. → `orca host pbs reconcile`.
- **systemd unit / Alpine OpenRC / cron entry creation**:
  `config/<host>/services/*.toml` declares units. Reconciler writes
  them under `/etc/systemd/system/` (or equivalent) and enables
  them. Survives reboots and re-runs idempotently.
- **Proxmox node prep** (autofs, NFS mounts at boot, LXC unpriv UID
  mapping): declared in the host's config. → `orca proxmox host reconcile`.
- **OPNsense / OpenWrt setup**: same pattern — declared in per-host
  config; orca owns the *invocation* and the idempotency check
  (even when the underlying call is an SSH shell-out for now), not
  a hand-run script.

The throughline: **no greenfield host should ever need a shell login
to reach steady state.** A fresh VM runs the bootstrap command,
pairs into the mesh, the reconciler picks up its slice of the
config repo, and from that point everything — package install,
service config, secrets, schedules — converges on its own.

Implementation principle: per-distro install logic lives in rust
inside orca, not as shell snippets templated by orca. The shell
elimination is the point.

#### 3.6a User credential sync — trust-tier model

User accounts (UID/GID, shell, group membership, SSH authorized_keys,
salted+hashed password from `/etc/shadow`) sync **across hosts** via
orca's mesh, but only between hosts of equal-or-higher trust tier.

Tiers (declared per host in `config/<host>/host.toml`):

- `trust = "secure"` — full-disk encryption or equivalent at rest,
  orca daemon protected by mesh mTLS, no untrusted local processes.
  Examples: baldur, frigg, thor. Holds the canonical user table.
- `trust = "insecure"` — anything that doesn't meet the bar. Routers,
  IoT-adjacent boxes, edge devices, shared hardware, anything where
  the local disk could be read by an attacker. Examples: openwrt,
  opnsense if not full-disk-encrypted.

**Sync rules** (hard, enforced in the reconciler):

1. Hashed credentials (`/etc/shadow` entries) replicate **only**
   between `secure` hosts. Salted hashes are still material that
   shouldn't sit on disks an attacker can pull.
2. SSH public keys and user metadata may replicate to `insecure`
   hosts (public keys aren't secret), but...
3. ...an `insecure` host **must not perform local password
   authentication**. PAM/login config on insecure hosts is locked
   to either: (a) SSH key only, or (b) PAM module that delegates
   the auth request to a `secure` orca peer over mTLS, receives
   yes/no, and never sees the credential.
4. The delegating call uses the existing pod-mesh mTLS channel;
   the secure peer applies the salted-hash check locally and
   returns a signed result with a short TTL.
5. Insecure hosts cache nothing credential-derived. A peer
   unavailable means auth fails closed.

**Implementation hooks:**

- Trust tier is a property in the host record in orca's config
  store, set at bootstrap.
- The user-sync reconciler refuses to push shadow entries to peers
  with `trust != "secure"` and emits an audit event on attempt.
- A new PAM module (or `nsswitch` shim) `pam_orca_remote` ships
  with orca for use on insecure hosts. Configured via the host
  bootstrap step.
- An insecure host that gets *promoted* to secure (e.g., FDE added)
  picks up the full table on next reconcile; demotion wipes shadow
  state on the next pass.

**Open questions:**

- What's the exact transport for the auth-delegation call —
  reuse the MCP plugin-host channel, or a dedicated `auth_check`
  endpoint with stricter rate-limiting? Lean dedicated.
- Quorum: should a delegated auth check require N-of-M secure
  peers to agree, or first-response-wins? First-response is
  simpler and matches the SPOF reality of small homelabs.
- What's the answer for `root`? Recommend: no synced root
  password anywhere; root login disabled on insecure hosts;
  `sudo`/`doas` on secure hosts via user account.

### 3.7 Agents (`agents/`)

Claude agent definitions. After migration these still exist in meerkat
but their tool calls target orca's MCP, not meerkat's. Light edit pass
when meerkat's MCP server retires.

---

## 4. Strategy

### 4.1 Two principles

1. **Orca stays generic.** No "meerkat", "scottkey", or per-host
   strings leak into orca core. Per orca-v1-scope §3.8 ("de-meerkat-ifying")
   this is already a tracked workstream.
2. **No flag day.** Migrate one capability at a time. Meerkat's MCP
   server and orca's daemon coexist on every host through the
   transition; consumers (claude agents, scripts) get pointed at the
   new endpoint as features land.

### 4.2 Plugin retirement vs promotion

Rule of thumb: if the plugin duplicates an orca integration listed
"Prod" in orca-v1-scope §1, **delete the plugin** — don't try to
unify, the orca version wins.

Exceptions (`graphql`, `rest`): no orca built-in exists. Promote into
orca as `integrations/{graphql,rest}` and then delete the plugin.

This shrinks `plugins/` from 10 dirs to 0.

### 4.3 Per-host scripts → orca host model

Orca already has a host noun (per the canonical surface in
orca-v1-scope §2). The migration is:

1. For each per-host script, identify the verb (`backup`, `restore`,
   `update`, `nfs_release`, ...).
2. Express it as an orca tool that runs on that host's local daemon
   (no SSH, no central orchestrator — orca is already on every host).
3. Delete the shell script and its `meerkat.sh` sudo entry.

Net result: `/etc/sudoers.d/meerkat` goes away. Orca daemon runs as
the privileged process; per-tool ACLs gate who can invoke what.

### 4.4 Secrets

Meerkat's `internal/secrets` and orca's `mcp/secrets_service` are both
encrypted SQLite stores. Migration:

1. One-shot import tool: `orca secrets import --from meerkat <path>`.
2. Verify on one host (mint), then run everywhere.
3. Update connectors to call orca secrets resolver.
4. Delete meerkat secrets store + token (`MEERKAT_TOKEN`).

### 4.5 Self-update

Meerkat has its own GitHub-release update flow (`internal/update`).
Orca has its own. After meerkat binary retires this is automatic —
nothing to migrate, just delete.

---

## 5. Phasing

Ordered for low blast radius. Each phase leaves the system in a
working state.

### Phase 0 — prerequisites (do first)

- P0a Confirm orca daemon is healthy on all hosts (it is, per
  `peer_list`: mint, baldur, freyr, frigg, thor reachable; willow,
  maple, loki need attention separately).
- P0b Per-tool mesh ACL (also a prerequisite for Caddy plugin §11) —
  needed before opening orca verbs that today require sudo.

### Phase 1 — promote unique value

- P1a Promote `plugins/graphql` → `integrations/graphql` in orca.
- P1b Promote `plugins/rest` → `integrations/rest` in orca.
- Delete those two plugin dirs.

### Phase 2 — retire duplicate plugins

For each of `docker, dockge, proxmox, unraid, homeassistant, nfs,
ntfy, git`: confirm orca built-in covers the meerkat plugin's usage,
delete the plugin dir, remove from any plugin registration in
meerkat config.

### Phase 3 — retire meerkat Go binary

- P3a Migrate any code in `meerkat/internal/connectors/*` that
  *isn't* already covered by an orca integration (audit pass).
- P3b Import secrets store into orca.
- P3c Point claude agents (`agents/*.md`) at orca's MCP.
- P3d Delete `meerkat/` directory, `go.mod`, `Makefile`. ~4600 LOC gone.

### Phase 4 — migrate shell scripts

In dependency order:
- P4a `meerkat-sync.sh` + `install-meerkat-sync.sh` → orca scheduler
  job (uses git plugin already Prod in orca).
- P4b `meerkat.d/` subcommands → orca verbs (backup, restore, list,
  status, sync, update).
- P4c `meerkat-agent.sh` → orca host verbs. Remove sudoers entry.
- P4d Top-level utility scripts (`nfs-monitor`, `pbs-*`,
  `sync-proxmox-configs`, `setup-host`) → orca-native rust per the
  existing memory note ("scripts → orca (rust) migration").
- P4e Per-host script dirs (`scripts/<hostname>/`) → orca host-noun
  verbs. Keep `.conf` files in place as config.

### Phase 5 — fix-* one-shots

Decide for each (`fix-unbound-forwarding.py`, `fix-wireguard-config.py`,
`autofs-switchback.sh`, `migrate.sh`): keep as documented one-shot in
`docs/`, or migrate to a permanent orca verb. Default: delete unless
the failure mode they fix is recurring.

### Phase 6 — cleanup

- Delete `legacy/`.
- Delete `meerkat/` (already done in P3d) and any `.meerkat-*`
  artifacts on hosts.
- Update root `README.md` to say "meerkat = orca config repo for the
  scottkey homelab".
- Final sweep: zero "meerkat" strings in orca core (orca-v1-scope §3.8).

---

## 6. Open questions

- **Does meerkat the *name* survive?** The repo name is unique and
  cheap to keep; the binary name and Go module path are what go away.
  Recommend: keep the repo name as the homelab config repo identity.
- **Where do per-host `.conf` files (`nfs-monitor.conf`) live?**
  Options: (a) stay in `scripts/<host>/` as config data, (b) move to
  `config/<host>/`. (b) reads cleaner once `scripts/` is empty of
  shell — recommend (b) in P6.
- **Claude agents that shell out via Bash today** (`meerkat-deploy.md`,
  `meerkat-status.md`, `meerkat-backup-validate.md`): rewrite to call
  orca MCP tools? Or keep shell calls that go through orca CLI? Latter
  is simpler — orca CLI is the same surface as MCP per orca-v1-scope §2.
- **OPNsense / OpenWrt integrations** don't exist in orca yet. Are
  they worth building as first-class integrations, or do the PIA/DNS
  scripts stay as orca-scheduled shell-outs that just call `ssh router`?
  Recommend: shell-out for now, promote to integration only if a
  second use case emerges.
- **Backup format**: `backups/configs/` is currently produced by
  shell. The migrated orca backup verb must produce the same on-disk
  layout, or we lose continuity with existing snapshots. Pin the
  format before P4 lands.

---

## 7. Work breakdown

| # | Phase | Item | Size |
|---|---|---|---|
| 1 | P0 | Verify orca daemon health on willow/maple/loki | S |
| 2 | P0 | Per-tool mesh ACL (shared with Caddy plugin) | M |
| 3 | P1 | Promote `plugins/graphql` → `integrations/graphql` | M |
| 4 | P1 | Promote `plugins/rest` → `integrations/rest` | M |
| 5 | P2 | Retire 8 duplicate plugin dirs (audit + delete) | S |
| 6 | P3 | Audit `meerkat/internal/connectors/*` for unique behavior | S |
| 7 | P3 | Secrets import tool: meerkat → orca | S |
| 8 | P3 | Repoint claude agents at orca MCP | S |
| 9 | P3 | Delete `meerkat/` Go module | S |
| 10 | P4 | `meerkat-sync` → orca scheduled git job | S |
| 11 | P4 | `meerkat.d/` subcommands → orca verbs | M |
| 12 | P4 | `meerkat-agent.sh` → orca host verbs, drop sudoers entry | M |
| 13 | P4 | `nfs-monitor.sh` → orca nfs health (parity check + delete) | M |
| 14 | P4 | `pbs-*` scripts → orca pbs integration hooks | M |
| 15 | P4 | `sync-proxmox-configs.sh` → orca proxmox snapshot job | S |
| 16 | P4 | `setup-host.sh` → `orca host bootstrap` (rust-native, per-distro) | L |
| 16a | P4 | `orca host install docker` (apt/apk/pacman, group membership, enable) | M |
| 16b | P4 | `orca host install tailscale` (with auth-key secret resolution) | S |
| 16c | P4 | `orca host install pbs-client` | S |
| 16d | P4 | `orca host service install` (systemd / OpenRC / cron, idempotent) | M |
| 16e | P4 | `orca proxmox host configure` (autofs, NFS-at-boot, unpriv UID map) | M |
| 16f | P4 | `orca host users reconcile` (users, SSH keys, sudo, doas — declarative) | M |
| 16g | P4 | Trust-tier model: `trust = secure\|insecure` in host record + enforcement | S |
| 16h | P4 | Shadow-hash mesh sync (secure ↔ secure only, audit on attempts) | M |
| 16i | P4 | `pam_orca_remote` PAM module: delegate auth on insecure hosts | L |
| 16j | P4 | `auth_check` mTLS endpoint on secure peers (rate-limited, signed result) | M |
| G1  | P0 | Git-provider trait + GitHub impl via `octocrab` (poll + read) | M |
| G2  | P1 | Git-provider: commit + open-PR via API (write path) | M |
| G3  | P1 | Gitea/Forgejo provider impl | S |
| G4  | P2 | Generic `gix` fallback provider | M |
| G5  | P2 | Webhook receiver endpoint(s) per provider | S |
| G6  | P1 | Materialized working tree from API responses (no `git pull`) | M |
| 17 | P4 | Per-host script dirs → orca host-noun verbs (per host) | L |
| 18 | P5 | One-shot script triage (`fix-*`, `autofs-switchback`, `migrate`) | S |
| 19 | P6 | Delete `legacy/`, move `.conf` files to `config/<host>/` | S |
| 20 | P6 | README + agents/* rewrite for new mental model | S |
| 21 | P6 | Final "no meerkat strings in orca core" sweep | S |

P0 + P1 + P2 are independent and can land in any order. P3 unblocks
P4. P5/P6 are cleanup.

---

## 8. Relationship to other planned docs

- [orca-v1-scope.md](orca-v1-scope.md) — orca framework roadmap.
  §3.8 "de-meerkat-ifying" is the orca-side mirror of this doc.
- [caddy-plugin-scope.md](caddy-plugin-scope.md) — one slice of this
  migration (Caddy route management).
- [plugin-architecture.md](plugin-architecture.md) — the contract
  retained plugins must speak.
- [connector-roadmap.md](connector-roadmap.md) — likely overlaps
  §3.1 here; reconcile when this doc is approved.
