# Orca v1 — framework scope

Orca is a **generic** infrastructure tool framework. No homelab assumptions, no "meerkat" or "rebuy" strings in core. Consumers (meerkat, rebuy plugin, anyone else) point at it via a bootstrap file and get a uniform CLI/MCP/REST/WASM surface.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. What's already shipped

| Capability | Module | Status |
|---|---|---|
| Tool framework (one def → CLI + MCP + REST + WASM) | `utils/tool` | Prod |
| Proxmox API (VM/LXC, snapshot, `lxc_exec`) | `integrations/proxmox` | Prod |
| NFS / SMB mount + probe + lazy unmount | `integrations/{nfs,smb}` | Prod |
| ntfy push + heartbeat | `integrations/ntfy` | Prod |
| Docker Compose CLI wrapper | `integrations/docker/compose` | Prod |
| Dockge / Unraid GraphQL / Home Assistant REST | `integrations/{dockge,unraid,homeassistant}` | Prod |
| Secrets store (encrypted, SQLite) | `mcp/secrets_service` | Prod |
| Plugin host (mTLS + JSON-RPC) | `server/plugin_host` | Prod |
| Pod mesh (mTLS, mDNS discovery, cert rotation) | `server/pod` | Prod |
| Config loader (~/.orca, env) | `utils/config` | Prod |
| Git plugin (libgit2: clone/pull/commit/push) | reference plugin | Prod |

---

## 2. Canonical command surface

Flat. No `tools` umbrella. Every noun sits directly under `orca`.

```
orca <noun> <verb> [--host <name>] [...]

  <noun> is any registered tool — service, host integration (proxmox,
  nfs, pbs, docker, unraid), schedule, config, pod, secrets, plugin-
  contributed noun (e.g. rebuy), etc.

  Canonical verb set (each noun implements the subset that applies):
    list | status | health
    start | stop | restart    long-lived daemon lifecycle (only for nouns
                              that hold a persistent loop — services,
                              watchdogs, VMs/LXCs/containers)
    run                       one-shot execute. Same logic whether the
                              scheduler invokes it periodically or an
                              operator runs it manually.
    exec                      interactive shell as orca user in target context
    backup | restore | update | logs

  Domain-specific verbs are additive (`nfs mount`, `nfs failover`,
  `proxmox snapshot create`, `pbs hook`, etc.).

  Cross-host execution:
    --host <name>              dispatch this verb on a registered remote
                               host via pod mesh (§3.3). Defaults to local.
                               `<name>` is a `host` config-row name —
                               peers are registered, not free-form.
```

Examples:
```
orca service backup plex                 # local
orca service backup plex --host frigg    # routes to frigg via mesh
orca nfs run --host willow              # one watchdog iteration on willow
orca host exec                           # local shell
orca host exec --host baldur             # SSH-equivalent into baldur
orca github-app status                   # local (no --host = this host)
```

---

## 3. Features to build

### 3.1 Config store — **L**

DB is the source of truth. SQLite, alongside the existing secrets store.

**Tables**
- `config_row(id, host_owner, noun, name, json, updated_at, updated_by)` — typed by `noun` (matches CLI noun: `service`, `schedule`, `backup_job`, `nfs_watch`, `chown_sweep`, etc.). JSON validated against per-noun schema.
- `config_history(row_id, prior_json, changed_at, changed_by)` — every write keeps prior.

**Ownership** — each row has `host_owner`. Only the owning host writes. Other hosts read (replica) but cannot edit. CLI on a non-owner host operating on a non-local row routes the write to the owner via pod mesh.

**Schema registry** — each `noun` ships a JSON schema. `orca config set` validates input before write. Schemas marked `sensitive: true` on individual fields → field never serialized to git.

**CLI**
```
orca config list [--noun service] [--host thor]
orca config get service plex
orca config set service plex --runtime lxc:110 --backup-ref plex
orca config delete service plex
```

**TODO**
- [ ] SQLite schema + migrations
- [ ] Noun registry (each tool registers its schema)
- [ ] Owner-routing in CLI dispatch (mesh round-trip for non-local writes)
- [ ] History table + `orca config history <row>` viewer

---

### 3.2 Config sync — DB ↔ git ↔ DB — **L**

Bidirectional sync between the local config store and a git repo.

**Pull (every 10 min)**
- `host.config.pull.run` (scheduler invokes periodically) clones/fetches `config_repo` from `bootstrap.toml`.
- Diffs `config/<host_name>/` against local DB rows owned by this host.
- Applies repo → DB. Repo wins on conflict. Prior DB value lands in `config_history`.
- Re-materializes schedules + restarts affected watchers.

**Push (nightly, configurable, default 03:00)**
- `host.config.push.run` serializes rows owned by this host into `config/<host_name>/<noun>.toml`.
- For each file changed, copies prior to `config/<host_name>/.history/<file>.<ts>`.
- Per-host commit (`config: <host> nightly sync`). Pushes if dirty.

**Secret safety** — three layers:
1. Secrets live in the **separate** secrets store, not `config_row`. Never serialized.
2. Schema-level `sensitive: true` fields are stripped at serialize time even from config rows.
3. Pre-commit scan: high-entropy strings, known token prefixes (`ghp_`, `sk-`, `AKIA*`), values matching anything in the local secrets store → abort push.

**Conflict policy** — repo wins. Documented loudly. The `.history/` files + DB `config_history` give recovery.

**TODO**
- [ ] TOML projection codec (DB row ↔ file)
- [ ] Pull-run tool + git plugin integration
- [ ] Push-run tool + history-file logic
- [ ] Sensitive-field stripping + entropy/prefix scanner
- [ ] Per-noun re-materialization hooks (e.g. schedule reload after `[[schedule]]` change)

---

### 3.3 Mesh-replicated config — **M**

Same config rows also replicate over pod mesh (so a non-owner host sees fresh state without waiting for the nightly git round-trip).

- Owner pushes row deltas to peers on write.
- Non-owners persist the replica locally with `is_replica = true`.
- Secrets do **not** replicate over the mesh.

**TODO**
- [ ] JSON-RPC method `config.replicate(rows[])` over existing mTLS channel
- [ ] Replica eviction when owner removes a row
- [ ] Peer-tool-invocation (`<peer>.<noun>.<verb>` routes to peer) — currently pending pairing work

---

### 3.4 In-process scheduler — **M**

Eliminates system cron. Single source of truth.

- Long-lived job. Loads scheduled tools from the config store.
- Each job is a canonical tool name + cron expression. Dispatches by calling tool registry (no shell-out).
- Per-job last-run / next-run / outcome persisted to SQLite.
- Crash recovery: missed-while-down jobs run on startup (configurable per job).
- Single-instance lock per job (in-process mutex by name).

**TODO**
- [ ] `tokio_cron_scheduler` (or `cron` crate + custom executor) wiring
- [ ] Schedule loader from config store (live-reload on `config` table change)
- [ ] `orca schedule {list,status,run}`
- [ ] Missed-job replay policy per job

---

### 3.5 Bootstrap contract — **M**

One file. System-wide. `/etc/orca/bootstrap.toml`.

```toml
host_name        = "thor"
config_repo      = "git@github.com:scottkey/meerkat.git"
config_branch    = "main"
config_path      = "config"
secrets_backend  = "local"                       # or "1password", "vault"
pod_join         = "orca://thor.lan:7878"        # optional
pull_interval    = "10m"
push_cron        = "0 3 * * *"

[github_app]
app_id           = 123456
installation_id  = 789012
private_key_path = "/etc/orca/github-app.pem"    # or secret:github_app/key
```

`orca` reads this on startup. Nothing meerkat-specific. Anyone points it at their own repo + GitHub App and gets the same UX.

**TODO**
- [ ] Bootstrap file loader + schema
- [ ] First-run flow: read bootstrap → clone repo → seed DB → join mesh
- [ ] `orca bootstrap doctor` — validates bootstrap.toml + connectivity

---

### 3.6 GitHub App — first-class orca subsystem — **L**

Orca **owns** GitHub App execution and secret material. Not a meerkat add-on; a core capability exposed as `orca github-app`. Any consumer (meerkat, rebuy plugin, anything else) requests GitHub operations through orca, which holds the private key, generates installation tokens, and brokers calls.

**Why orca-owned:**
- Private key is sensitive secret material → belongs in the encrypted secrets store, not a repo or bootstrap file path.
- Token minting (installation tokens, 1h TTL) needs a long-lived caching layer → already what orca is.
- Multiple consumers (meerkat config sync, rebuy plugin GitHub API calls, future plugins) all share one App and one rate-limit budget.

**CLI surface**
```
orca github-app
  list                      registered apps (multi-app support from day 1)
  status <name>             validity, installation count, rate-limit headroom
  add <name>                interactive: paste app_id + installation_id + private key (stored encrypted)
  rotate <name>             rotate private key; supports overlap window
  remove <name>
  token <name>              mint an installation token (operator escape hatch / debugging)
  exec <name> -- <git ...>  run a git command pre-authed against the App
```

**Programmatic API** (consumed by config-sync, plugins, future tools):
```rust
let app = orca::github_app::get("meerkat")?;
let token = app.installation_token().await?;       // cached, auto-refreshed
let repo = app.repo("scottkey/meerkat")?;
repo.clone_to(...).await?;
repo.commit_push(...).await?;
```

**Storage**
- Private key + app_id + installation_id → encrypted in the secrets store, namespace `github_app/<name>`.
- App metadata (name, default repo, scopes) → config_row with `host_owner = null` (shared across hosts) — or per-host if private keys differ per host. **Decision needed.**
- Token cache → in-memory only, never persisted.

**Bootstrap relationship**
`bootstrap.toml` references a *named* app, not the key material:
```toml
[config_sync]
github_app = "meerkat"      # name registered via `orca github-app add`
```
On fresh-host bootstrap, the operator runs `orca github-app add meerkat` once and pastes the key. The key never lives in a config file on disk.

**Pluggable auth trait** — `GitAuth` trait with `GitHubApp` as one impl. Cloud-broker impl (future) drops in without consumer changes.

**TODO**
- [ ] Create the App on github.com (out-of-band, one-time) — record `app_id`, generate private key, install to the meerkat repo. Capture `installation_id`. **Operator action, not code.**
- [ ] `integrations/github_app` module: `octocrab` + JWT signing, installation-token minting, caching with TTL
- [ ] Secrets-store namespace + add/remove/rotate flows
- [ ] `tools github-app` verbs (list/status/add/rotate/remove/token/exec)
- [ ] `GitAuth` trait + plumb through `integrations/git` clone/pull/commit/push
- [ ] Rate-limit awareness (single budget across consumers; surface in `status`)
- [ ] Rotation overlap window (old + new key both valid during rotation)

---

### 3.7 `exec` verb across nouns — **M**

Drops operator into an interactive shell as orca user in the target context.

| Noun | Implementation |
|---|---|
| `tools proxmox lxc exec <vmid>` | `pct enter <vmid>` |
| `tools proxmox vm exec <vmid>` | SSH or serial console |
| `tools docker exec <ctr>` | `docker exec -it <ctr> /bin/sh` |
| `tools unraid exec <host>` | SSH |
| `tools host exec` | local shell |
| `tools <service> exec` | resolves `runtime = "lxc:N"` / `"docker:X"` / `"host:Y"` in config store, delegates |

**TODO**
- [ ] `exec` tool trait (TTY passthrough — different from regular tool dispatch)
- [ ] Per-noun impls (proxmox, docker, unraid, host)
- [ ] Service-level delegation via runtime lookup

---

### 3.8 De-meerkat-ifying orca core — **M**

Currently in orca/orca-adjacent repos there are meerkat-named artifacts:
- `meerkat-agent.sh`
- `scripts/meerkat.d/`
- `meerkat/plugins/git/`
- `mcp__meerkat__*` tool prefixes (if any)
- Any hardcoded `meerkat` strings in core

**End state** — none of these live in `orca`. The git plugin moves to a stock orca integration. The agent is replaced by `orca host` verbs. Plugin discovery uses `bootstrap.toml` paths, not hardcoded directory names.

**TODO**
- [ ] Inventory all "meerkat" mentions in orca core (grep)
- [ ] Move git plugin → `integrations/git`
- [ ] Remove `meerkat/plugins/` dir, replace with `~/.orca/plugins/` and `bootstrap.toml` plugin_paths
- [ ] Delete `meerkat-agent.sh`; capabilities subsumed by `orca host`
- [ ] Rename any `mcp__meerkat__*` to a neutral prefix (or drop the namespace)

---

### 3.9 Plugin contract hardening — **M**

Validated by the rebuy plugin migration (separate doc). What plugins should be able to do:

- Register new nouns and verbs into the canonical tool tree.
- Ship their own JSON schemas for config rows.
- Subscribe to scheduler events.
- Use orca's secrets store via handle.
- Run cross-host via pod mesh.

**TODO**
- [ ] Document the plugin contract in code (`orca-plugin.toml` reference)
- [ ] Provide a `plugin-template` repo
- [ ] Use rebuy plugin to find gaps; close them

---

## 4. Cross-cutting additions

| Addition | Where | Size |
|---|---|---|
| Job runner primitive (validate → tmp → promote) | `utils/job` | M |
| Consumer-restart abstraction (docker/lxc/systemd) | extend integrations | M |
| File ownership ops (chown by uid predicate) | `utils/fs/perms` | S |
| fstab parser + lazy umount/mount helpers | `integrations/mount` | S |
| Uptime Kuma push client | `integrations/uptime_kuma` | S |
| TTY-aware exec dispatch | `utils/tool` extension | S |

---

## 5. Phased build map

Three phases. Each phase ends with a usable orca state.

### Dependency graph

```
                                  ┌──────────────────────────────┐
                                  │  Foundation (P0)             │
                                  │                              │
                                  │  §3.1 config store ──┐       │
                                  │                      ├─► §3.4 scheduler
                                  │  §3.8 de-meerkat ────┘       │
                                  └──────────────┬───────────────┘
                                                 │
                                  ┌──────────────▼───────────────┐
                                  │  IaC loop (P1)               │
                                  │                              │
                                  │  §3.6 github-app             │
                                  │       │                      │
                                  │       ▼                      │
                                  │  §3.5 bootstrap ──► §3.2 sync│
                                  └──────────────┬───────────────┘
                                                 │
                                  ┌──────────────▼───────────────┐
                                  │  Mesh & UX (P2)              │
                                  │                              │
                                  │  §3.3 mesh-replicate         │
                                  │     (needs pairing pending)  │
                                  │  §3.7 exec verb              │
                                  │  §3.9 plugin contract        │
                                  │     (validated via rebuy)    │
                                  └──────────────────────────────┘
```

### P0 — Foundation (~8 dev-days)

State at end: orca has a working config store and an in-process scheduler. Hosts can read/write rows locally; jobs run on cron expressions sourced from the store. Nothing leaves the host yet.

| # | Feature | Size | Notes |
|---|---|---|---|
| 1 | §3.1 config store | L | SQLite schema, noun registry, CRUD CLI. Owner-routing stub (rejects non-local writes until §3.3). |
| 2 | §3.4 scheduler | M | Loads from store, dispatches in-process. Replaces system cron entirely. |
| 3 | §3.8 de-meerkat-ifying (start) | M | Done in parallel — rename/move artifacts as features land in their new shape. |

**Gate to P1:** A host can `orca config set schedule …` and the job runs.

### P1 — IaC loop (~12 dev-days)

State at end: meerkat repo is the offsite IaC snapshot. Edits in either direction (DB or repo) converge within 10 min. Secrets never leak to git.

| # | Feature | Size | Notes |
|---|---|---|---|
| 4 | §3.6 github-app | L | First. Owns all subsequent git ops. |
| 5 | §3.5 bootstrap | M | References the App by name. `bootstrap doctor` validates connectivity. |
| 6 | §3.2 git sync | L | Pull-run + push-run. Sensitive-field stripping + entropy scanner. |

**Gate to P2:** Push to meerkat → effected host applies within 10 min. Nightly push from host appears in git with no secrets in diff.

### P2 — Mesh & UX (~10 dev-days)

State at end: cross-host operations work. Operator UX is polished. Plugin contract proven via the rebuy plugin.

| # | Feature | Size | Notes |
|---|---|---|---|
| 7 | §3.3 mesh-replicated config | M | Depends on pod peer-tool-invocation (pending pairing work). |
| 8 | §3.7 exec verb | M | Parallelizable — can start in P1 if bandwidth allows. |
| 9 | §3.9 plugin contract hardening | M | Validated against rebuy plugin migration; gaps close here. |
| 10 | §3.8 de-meerkat-ifying (finish) | (in M above) | Final sweep. Zero "meerkat" strings in orca core. |

**Total:** ~30 dev-days for orca v1.

---

## 6. Open questions

1. SQLite or sled for the config store? (Recommend SQLite — already used for secrets.)
2. TOML or JSON as the on-disk projection? (User said either; recommend TOML for human-edit ergonomics.)
3. Plugin discovery — explicit list in `bootstrap.toml`, scan a system dir, or both?
4. Cloud auth broker design — defer to v2, or sketch the interface now so v1 trait is right?

---

## 7. Definition of done

- `orca` binary contains zero "meerkat" or "rebuy" strings.
- A fresh host with only `bootstrap.toml` + GitHub App key can come up, clone, seed DB, join mesh, and start scheduled jobs.
- Editing a row via CLI on the owner host triggers (a) immediate mesh replication, (b) inclusion in the next nightly push.
- Editing a file in the repo + pushing triggers DB update on the owner host within 10 min (via the scheduled pull-run).
- Secrets never appear in the repo, ever, by any path. Verified by the pre-commit scanner test suite.
