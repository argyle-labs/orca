# Rebuy plugin — scope

Refactor `rebuy-cli` into an orca plugin. Two goals:

1. Rebuy functionality is available under the canonical `orca …` surface like everything else.
2. The plugin is used to **validate orca's plugin contract** (§3.9 of [orca-v1-scope.md](./orca-v1-scope.md)) — anything rebuy needs that orca can't currently expose is a gap to close in orca core.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days.

---

## 1. Current rebuy-cli surface

Already largely MCP-shaped — visible as `mcp__rebuy-cli__*` tools in the current session. Domains:

| Domain | Verbs | Notes |
|---|---|---|
| `auth` | op_setup, op_reset, op_status, op_test, op_user, op_vaults, status | 1Password-backed |
| `certs` | download, install_ca, setup, status | |
| `claude` | install, status, uninstall | Claude Code integration |
| `config` | (root), setup | |
| `db` | current, down, download, health, install, list, logs, migrate, reset, status, switch, up | local DB lifecycle |
| `dns` | dev, prod, setup, status | |
| `engines` | list, start, status, stop, switch | |
| `env` | (root), current, dev, dns_*, generate, history, logs, restart, start, status, stop, switch, validate | engine env mgmt |
| `ephemeral` | approve_public, auth, build, build_clear, build_status, config*, create, create_from_pr, delete, deny_public, deploy, extend, health, lifecycle, list, list_approvals, rebuild, status, sync | preview envs |
| `graphql` | list, operation | |
| `network` | create, remove, status | |
| `pr` | context, create | |
| `project` | list, logs, restart, start, status, stop | |
| `release` | create, dry_run, list, run, status | |
| `repos` | clone, clone_missing, clone_team, list, presets, teams, update | |
| `spec` | endpoint, generate, list, schema, sync | OpenAPI |
| `tunnel` | extend, start, status, stop | Cloudflare tunnels |
| `shopify` | graphql_list, graphql_type | |
| (top-level) | doctor, init, feedback, mode_*, completion, pull, search, search_natural, setup, snapshot, sync_deps, status, update, version | |

---

## 2. Target shape under orca

Mapped to canonical `tools <noun> <verb>`. Verbs are renamed to match the canonical set (§2 of orca-v1-scope.md) where they overlap.

```
orca rebuy
  auth      status | setup | reset | test | list-vaults | whoami
  certs     status | list | install | download
  claude    status | install | uninstall
  db        list | status | health | start | stop | restart | logs
            up | down | migrate | reset | switch | download | install
  dns       status | switch     (dev | prod | setup as subverbs of switch?)
  engines   list | status | start | stop | switch
  env       list | status | start | stop | restart | logs
            switch | generate | validate | history
  ephemeral list | status | health | start | stop | logs
            create | delete | extend | rebuild | sync | deploy
            create-from-pr | approve-public | deny-public | list-approvals
            build | build-clear | build-status | lifecycle
  graphql   list | run                                 (run = old "operation")
  network   list | create | remove | status
  pr        create | context
  project   list | status | start | stop | restart | logs
  release   list | status | run | create | dry-run
  repos     list | clone | update
            clone-missing | clone-team | teams | presets
  spec      list | sync | generate | endpoint | schema
  tunnel    status | start | stop | extend
  shopify   graphql {list, type}

  doctor    run
  init      run
  feedback  run
  search    run | natural
  snapshot  run
  status    run
  update    run
  version   run
  mode      list | status | switch
```

Top-level rebuy verbs (`doctor`, `init`, etc.) become `orca rebuy <verb>` so they fit the noun/verb pattern.

---

## 3. Plugin manifest

`rebuy-plugin/orca-plugin.toml`:
```toml
name        = "rebuy"
version     = "1.0.0"
description = "Rebuy engineering platform integration"
entry       = "./bin/orca-rebuy-plugin"   # JSON-RPC server

[noun.rebuy]
description = "Rebuy platform tools"

[[tool]]
noun = "rebuy.db"
verb = "up"
schema = "./schemas/db_up.json"

# ... one [[tool]] per verb
```

---

## 4. Build phases

### 4.1 Inventory & gap report — **S**

- [ ] Read `rebuy-cli` source; enumerate every top-level command.
- [ ] For each command, note: required orca capability (HTTP client? cron? secrets? mesh?), inputs, outputs, side-effects, blocking vs background.
- [ ] Cross-reference against orca's existing capabilities + plugin API.
- [ ] Output: a gap list — what orca core needs to expose for the plugin to work.

### 4.2 Plugin scaffold — **M**

- [ ] Create `rebuy-plugin/` repo (or directory) with `orca-plugin.toml`, JSON-RPC server, Cargo workspace.
- [ ] Implement two trivial verbs end-to-end (`rebuy version run`, `rebuy auth status run`) to prove the contract.
- [ ] Verify CLI/MCP/REST surfaces all light up.

### 4.3 Migrate by domain — **L** per cluster

Order by independence + complexity. Each domain is its own PR.

1. **Simple wrappers first**: `version`, `feedback`, `completion`, `pr context`, `mode`, `status`, `update`.
2. **Auth / certs / claude / config / setup / init / doctor** — foundational, used by other domains.
3. **db / network / dns** — local lifecycle, no remote auth.
4. **repos / spec / graphql / shopify** — read-mostly Github/API plumbing.
5. **engines / env / project** — engine lifecycle.
6. **ephemeral** — biggest domain, depends on auth + build + deploy. Save for last.
7. **release / tunnel / search / snapshot / sync-deps** — operational verbs.

### 4.4 Close orca gaps — **M** (per gap)

For each gap found in §4.1, file an orca-core issue. Examples likely to surface:
- HTTP client trait (rebuy hits many internal HTTP services).
- Long-running background jobs (build, ephemeral create) — needs an orca-side job runner with progress.
- 1Password integration as a secrets backend (`secrets_backend = "1password"` in bootstrap).
- Cloudflare tunnel lifecycle (network plumbing).
- Interactive prompts during a tool run (auth flows).

### 4.5 Deprecate `rebuy-cli` binary — **S**

- [ ] Once parity is reached, mark the standalone binary deprecated.
- [ ] Wrapper script `rebuy` → `orca rebuy` for muscle-memory.
- [ ] Update docs.

---

## 5. Validation — proving the plugin contract

The plugin is a success when:

- [ ] All `mcp__rebuy-cli__*` tools have a `tools rebuy <noun> <verb>` equivalent.
- [ ] The CLI command shape is **identical** in argument structure between the old `rebuy <noun> <verb>` and `orca rebuy <noun> <verb>`.
- [ ] No code in orca core mentions "rebuy". Everything rebuy-specific lives in the plugin.
- [ ] Plugin can be uninstalled cleanly (remove from `bootstrap.toml`, restart orca, `tools rebuy …` disappears).
- [ ] Plugin works on a fresh orca install given only the manifest + binary.

---

## 6. Open questions

- [ ] Plugin language: keep rebuy-cli in its current lang (TS? Go?) and speak JSON-RPC, or rewrite in Rust for orca-binary cohesion?
- [ ] 1Password as the secrets backend — generic enough to belong in orca core, or stay rebuy-specific?
- [ ] Ephemeral env lifecycle is long-running (minutes). Does orca's tool framework support progress reporting today, or is that a gap?
- [ ] Single rebuy plugin, or split (e.g. `rebuy-core`, `rebuy-ephemeral`, `rebuy-spec`)?
