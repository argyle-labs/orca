# Repository Structure

Where everything lives and why. For sequencing see
[`ROADMAP.md`](ROADMAP.md); for the surface model see
[`architecture.md`](architecture.md).

## Root

```
orca/
  Cargo.toml             Workspace manifest — lists all member crates
  Cargo.lock             Pinned dependency versions
  rust-toolchain.toml    Pins the exact Rust toolchain for reproducible builds
  clippy.toml            Workspace clippy config (disallowed_types etc.)
  Makefile               Developer workflows (build, test, lint, format, release)
  CRATE_RESPONSIBILITIES.md  Boundary doc for the workspace crates
  PLUGINS.md             Plugin author quick-pointer
  CHANGELOG.md           Release notes
  README.md              Quick-start, install, dev commands
  CLAUDE.md              Project-specific rules
  docs/                  This directory (see below)
  hooks/                 Event hooks (safety guards, format/lint)
  scripts/               Build + install + release scripts
  tests/                 Workspace-level integration tests
  projects/              All member crates (see architecture.md for the list)
```

## Workspace crates (under `projects/`)

The authoritative list of crates and their responsibilities lives
in `CRATE_RESPONSIBILITIES.md` at the repo root.
[`architecture.md`](architecture.md) groups them by purpose. Quick
pointer:

- **Lifecycle core** — `system/` (install, update, scheduler,
  daemon, host status, topology).
- **Mesh + identity** — `pod/`, `auth/` (CA, mTLS, secrets store).
- **Macros + dispatch** — `derive/`, `dispatch/`, `contract/`.
- **Storage + sync** — `db/` (SQLite layer + migrations + sync
  primitive), `files/` (fs primitives).
- **Plugins** — there is no in-tree `projects/plugins/` directory;
  core ships no plugins. The plugin host `runtime/` (package
  `plugins`: registry + KV + manifest install) and the native-plugin
  SDK `plugin-proto/` + `plugin-loader/` + `plugin-toolkit/` +
  `plugin-toolkit-build/` live here. First-party plugins (docker,
  mcp, smb, jellyfin, plex, …) are standalone repos, run as
  subprocesses.
- **Domain** — `agents/` (core domain, embedded agent prompts +
  `agent.{list,get,run}`, exposed via the
  `plugin_toolkit::agents` registration seam), `containers/`,
  `storage/`, `database/`, `graphql/`, `openapi/`, `spec/`,
  `namespace/`, `conversation/`, `notifications/`,
  `orca-inventory/`.
- **Transport** — `server/` (thin HTTP+MCP, binary `orca`),
  `app-kit/` (UniFFI bindings). The SvelteKit web UI is **not** in
  this repo: it is the out-of-process `peacock` plugin
  ([argyle-labs/peacock](https://github.com/argyle-labs/peacock),
  SvelteKit project at `peacock/ui/`), which owns the root route
  `/`; orca proxies unmatched `/` requests to it.

### Naming rules

- No `orca-` prefix on workspace crates
  (`feedback_no_orca_prefix.md`). Flat names: `auth`, `pod`,
  `system`, `db`, etc.
- Stable contract types live in their own leaf crate so they cache
  independently of volatile runtime/dispatch
  (`feedback_crate_split_for_cache.md`).
- Every backend under `plugins/` is its own crate; no umbrella
  super-crate beyond a facade re-export
  (`feedback_integrations_one_crate_per_backend.md`).
- No consumer-specific strings (e.g. `meerkat` or any downstream
  plugin name) anywhere in orca core; those are separate downstream
  consumers (`feedback_no_consumer_strings_in_orca.md`).

## On-host layout

Once installed:

```
~/.orca/                       per-user state (when run as a normal user)
  orca.toml                    app config (ports, channels, plugin paths)
  channel                      stable | rc | dev
  orca.db                      encrypted SQLite (config rows, secrets, install state)
  .db_key                      DB encryption key (back this up)
  plugins/                     installed plugins
  machine_id                   peer identity anchor (or /etc/machine-id)

/var/lib/orca/                 service-user home (when installed via root flow)
  .ssh/authorized_keys         seeded from --admin-pubkey
  .local/bin/orca              installed binary
  .config/systemd/user/orca.service  user-systemd unit
```

The service user is created by `install.sh` when run as root and
never gets `sudo`. See [`install-runbook.md`](install-runbook.md).

## Scripts

`scripts/` (10 files total):

| Script | Purpose |
|---|---|
| `install.sh` | Pull install — host fetches binary from GitHub |
| `install-binary.sh` | Lower-level binary placement helper |
| `deploy-host.sh` | Push install — controller ships bytes over SSH |
| `build-host.sh` | Per-OS/arch release build |
| `release-lib.sh` | Shared release logic (single source of truth) |
| `release-local.sh` | Local release wrapper around release-lib |
| `dev.sh` | Dev-mode launcher |
| `setup.sh` | Repo bootstrap |
| `check-fast.sh` | Quick lint/format gate |

Release flow is user-owned: never run `make release` or
`gh release create` from an agent
(`feedback_releases_are_user_only.md`, `feedback_no_release_actions.md`).

## Contributor workflow

First-time setup:

```sh
make init       # verify/install build prerequisites (scripts/setup.sh)
make install    # git hooks + toolchain + cargo tooling (cargo-watch, cargo-audit, sccache)
```

The edit / build / run loop:

```sh
make dev        # hot-reload: Rust API :12000 + peacock Vite dev server :12001, secrets from 1Password
make build      # build the release binary (no install)
make deploy     # build, install to ~/.local/bin/orca, install the system daemon
make run        # run the installed binary with 1Password secrets

# Run the daemon directly while iterating:
make kill-dev                 # clear any running dev processes / stale daemon
cargo run -p server -- serve --dev   # backend only, no peacock web UI / HMR
cargo run -p server -- mcp-serve     # MCP stdio server (simulate Claude Code)
```

The daemon is installed as a launchd (macOS) / systemd (Linux) service via
`orca system install` (run by `make deploy`); `orca system delete` removes it.
On port handoff the dev process parks the running daemon (SIGUSR1) and reclaims
the port on exit — see `projects/system/src/daemon.rs`.

Quality gates (also run by the git hooks installed via `make install`):

```sh
make check      # cargo check --workspace (no link)
make lint       # prettier --check + eslint + clippy -D warnings
make format     # rustfmt + prettier (+ taplo for TOML)
make test       # vitest + cargo nextest + doctests
make coverage   # llvm-cov, enforces the workspace floor (mirrors CI + pre-push)
```

### Rust style rules

- **Collapse nested `if` / `if let`.** When clippy's `collapsible_if` applies,
  use `&&` let-chains: `if cond && let Some(x) = expr { ... }` — never a nested
  `if let` (project `CLAUDE.md`).
- **Imports at the top** of the file, never inline inside fns.
- **No `let _ = result_returning_call()`.** The workspace denies
  `clippy::let_underscore_must_use`; be explicit with `.ok()`, `.expect(...)`,
  or a typed match (`Cargo.toml [workspace.lints]`).
- **No opaque/untyped JSON types** in tool payloads — model them as typed
  structs deriving `serde` + `schemars`. Enforced by a pre-commit hook.
- **Flat crate names**, no `orca-` prefix; no consumer-specific strings
  (e.g. `meerkat`) in orca core.

### Norms

- **Never `git commit`, push, or stage** from an agent — the user owns commits
  and releases.
- A tool body doing real work inside `projects/server/` is misplaced; it
  belongs in the owning domain crate (see `CRATE_RESPONSIBILITIES.md`).
