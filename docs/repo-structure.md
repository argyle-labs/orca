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
- **Plugins** — `plugins/<id>/` per integration (proxmox, nfs,
  smb, docker, unraid, arr, etc.) + `plugins/runtime/` host +
  `sdk/` for multi-language authoring.
- **Transport** — `server/` (thin HTTP+MCP), `app-kit/`,
  `frontend/` (SvelteKit, embedded).

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
- No `meerkat` or `rebuy` strings anywhere in orca core; those are
  separate downstream consumers
  (`feedback_no_rebuy_or_meerkat_in_orca.md`).

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
