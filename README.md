# orca

[![CI](https://github.com/argyle-labs/orca/actions/workflows/ci.yml/badge.svg)](https://github.com/argyle-labs/orca/actions/workflows/ci.yml)
[![Stable](https://img.shields.io/github/v/release/argyle-labs/orca?sort=semver&display_name=tag&label=stable&color=blue)](https://github.com/argyle-labs/orca/releases/latest)
[![RC](https://img.shields.io/github/v/release/argyle-labs/orca?include_prereleases&sort=semver&display_name=tag&label=rc&color=orange)](https://github.com/argyle-labs/orca/releases)
[![Coverage](https://img.shields.io/badge/coverage-%E2%89%A565%25%20%E2%86%92%20100%25-blue)](docs/coverage-baseline.md)
![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)

Local-first AI agent orchestrator and homelab control plane. A single
self-contained Rust binary that runs on every host in a pod and exposes one tool
surface across CLI, REST, MCP, and a WASM browser client. The web UI is served
by the out-of-process `peacock` plugin
([argyle-labs/peacock](https://github.com/argyle-labs/peacock)), which owns the
root route `/`. LM Studio (or any local model) runs everything by default —
Claude is escalation only.

## Quick start

One script. It auto-detects your OS/arch, downloads the matching release
binary, verifies its sha256, and installs to `~/.local/bin`:

```sh
curl -fsSL https://github.com/argyle-labs/orca/releases/latest/download/install.sh | sh
```

Then run it:

```sh
orca            # interactive TUI chat session
orca serve      # web UI + REST + MCP-over-HTTP on :12000 / :12443
orca --help     # full, build-current command list
```

To build and run from source instead, see [Development](#development).

## Installation

The installer above (`install.sh`) is the supported install path for every
platform — there is no separate per-OS step. Useful flags (all also settable as
env vars):

```sh
# Pin a version / channel
sh install.sh --version v0.0.4-rc.1
sh install.sh --rc                      # newest pre-release (RC channel)

# Choose target / location
sh install.sh --target x86_64-unknown-linux-musl
sh install.sh --dir /usr/local/bin

# Bootstrap a dev box (also installs the Rust toolchain + cargo-watch)
sh install.sh --dev-setup
```

Run as `root` and it auto-provisions a least-privileged `orca` service user and
enables the user-systemd session (pass `--admin-pubkey "<ssh key>"`). The
channel marker (`stable` / `rc`) is written to `~/.orca/channel` and drives
`orca update`.

## Usage

The binary wears four hats from one build: CLI, TUI, web server, and MCP server.

```sh
orca                           # interactive TUI chat session
orca serve                     # start web UI + REST + MCP-over-HTTP on :12000 / :12443
orca mcp-serve                 # MCP stdio server (register with Claude Code)
orca run -a fox "why is this failing?"   # one-shot agent delegation
```

Register as an MCP server with Claude Code:

```sh
claude mcp add orca-local -- orca mcp-serve
```

### Tool surface

Every `#[orca_tool]` in a domain crate is emitted to all four surfaces. On the
CLI they appear as `orca <noun> <verb>`:

```sh
# MCP server federation
orca mcp list
orca mcp run <server> <tool> '{"arg":"value"}'

# Docker / compose
orca docker list
orca docker detail <id>

# LLM models
orca model list

# Agents
orca agent list

# Plugins
orca plugin add ~/code/my-plugin/orca-plugin.toml
orca plugin list
orca plugin data-set my-plugin my-key "value"

# Pod mesh
orca pod list
orca pod pair <addr>
```

Run `orca --help` for the full, build-current command list — it is generated
from the registered tools, not hand-maintained.

## Config

Runtime state lives under `~/.orca/`:

- `~/.orca/orca.toml` — app config (LLM endpoints, ports, channels, plugin paths)
- `~/.orca/orca.db` — encrypted SQLite/SQLCipher (config rows, secrets, registries, install state)
- `~/.orca/.db_key` — DB encryption key (back this up)

Ports are per-host configurable via `~/.orca/orca.toml [ports]` or env
(`ORCA_HTTP_PORT` / `ORCA_HTTPS_PORT` / `ORCA_MESH_PORT`). Registry data is
managed through the CLI/tool surface — do not edit the DB directly.

## Development

Build and run from a checkout:

```sh
git clone https://github.com/argyle-labs/orca && cd orca
make init      # verify/install build prerequisites (rust, node, etc.)
make install   # install git hooks + toolchain + cargo tooling (cargo-watch, cargo-audit, sccache)
make dev       # hot-reload dev mode: Rust API on :12000 + peacock Vite dev server on :12001
```

`make build` produces a release binary without installing it; `make deploy`
builds, installs to `~/.local/bin/orca`, and registers the system daemon
(launchd on macOS, systemd on Linux).

**Local dev secrets.** `make dev` / `make run` use the 1Password CLI to inject
secrets and require `OP_ACCOUNT` set in your environment (from `dotfiles/.zshrc`,
overridable via a gitignored `.env.local`). Find your account UUID with
`op account list`.

**Before you open a PR**, run the same gates CI enforces:

```sh
make lint        # rustfmt --check + clippy -D warnings + prettier/eslint
make test        # cargo nextest + doctests + vitest
make coverage    # llvm-cov workspace floor (ratcheting to 100% — see below)
```

Contribution workflow, PR acceptance criteria, and the coverage policy:

- [Contributing](CONTRIBUTING.md) — branch flow, PR checklist, acceptance criteria
- [Coverage baseline](docs/coverage-baseline.md) — the 100% coverage goal + ratchet rule

## Docs

- [Architecture](docs/architecture.md) — the four-surface model, ports, identity, state ownership
- [Repo structure](docs/repo-structure.md) — where everything lives and why
- [Crate responsibilities](CRATE_RESPONSIBILITIES.md) — what each workspace crate owns
- [Plugins](PLUGINS.md) — first-party plugins + how to author your own
- [Plugin authoring](docs/plugin-authoring.md) — the plugin contract and SDK
- [Developer docs](docs/dev/00-tour.md) — codebase tour, patterns, Rust primer
- [Contributing](CONTRIBUTING.md) — how to land a change (branch flow, PR criteria)
- [Roadmap](docs/ROADMAP.md) — what's shipped vs. next

`docs/legacy/` is historical (the pre-`orca` "brain" design) and is not kept
current.

## Make targets

| Target | Description |
|--------|-------------|
| `make init` | Verify/install build prerequisites |
| `make install` | Install git hooks + toolchain + cargo tooling |
| `make dev` | Hot-reload dev mode (dev.sh: Rust API :12000 + peacock Vite dev server :12001) |
| `make build` | Build the release binary (no install) |
| `make deploy` | Build, install to `~/.local/bin/orca`, install the daemon |
| `make run` | Run the installed binary with 1Password secrets |
| `make test` | vitest + cargo nextest + doctests |
| `make coverage` | llvm-cov workspace line-coverage gate (mirrors CI) |
| `make coverage-touched` | line coverage for the `.rs` files this branch touched (the 100% rule) |
| `make lint` | prettier + eslint + clippy (`-D warnings`) |
| `make format` | rustfmt + prettier (+ taplo for TOML) |
| `make check` | `cargo check --workspace` (no link) |
| `make audit` | npm audit + cargo audit |
| `make migration [up\|down\|status\|<slug>]` | Apply/scaffold DB migrations |
| `make clean` | Remove build artifacts |
| `make sync` | Refresh synced OpenAPI specs from upstream repos |

Releases are user-owned. Never run `make release`/`make deploy` or
`gh release create` from an agent, and never `git commit`.
