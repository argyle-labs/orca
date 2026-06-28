# orca

Local-first AI agent orchestrator and homelab control plane. A single
self-contained Rust binary with an embedded web UI that runs on every host in a
pod and exposes one tool surface across CLI, REST, MCP, and a WASM browser
client. LM Studio (or any local model) runs everything by default — Claude is
escalation only.

## Installation

**From a GitHub release** (pre-built binary, auto-detects OS/arch, verifies sha256):

```sh
curl -fsSL https://github.com/argyle-labs/orca/releases/latest/download/install.sh | sh
```

Or fetch a binary directly:

```sh
# Apple Silicon
curl -Lo orca https://github.com/argyle-labs/orca/releases/latest/download/orca-aarch64-apple-darwin
chmod +x orca && mv orca ~/.local/bin/orca
```

macOS blocks unsigned binaries downloaded from the internet. Clear the quarantine flag before running:

```sh
xattr -d com.apple.quarantine ~/.local/bin/orca
```

**From source:**

```sh
make init      # verify/install build prerequisites (rust, node, etc.)
make install   # install git hooks + toolchain + cargo tooling (cargo-watch, cargo-audit, sccache)
make deploy    # build frontend + release binary, install to ~/.local/bin/orca, install the daemon
```

`make build` produces the binary without installing it; `make deploy` builds,
installs to `~/.local/bin/orca`, and registers the system daemon (launchd on
macOS, systemd on Linux).

## Setup

### Local dev secrets

`make dev` uses the 1Password CLI to inject secrets. It requires `OP_ACCOUNT`
set in your environment (configured in `dotfiles/.zshrc`, overridable via a
gitignored `.env.local`). On a new machine, ensure dotfiles are installed
before running `make dev`.

Find your account UUID: `op account list`

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

## Docs

- [Architecture](docs/architecture.md) — the four-surface model, ports, identity, state ownership
- [Repo structure](docs/repo-structure.md) — where everything lives and why
- [Crate responsibilities](CRATE_RESPONSIBILITIES.md) — what each workspace crate owns
- [Plugins](PLUGINS.md) — first-party plugins + how to author your own
- [Plugin authoring](docs/plugin-authoring.md) — the plugin contract and SDK
- [Developer docs](docs/dev/00-tour.md) — codebase tour, patterns, contributor workflow
- [Roadmap](docs/ROADMAP.md) — what's shipped vs. next

`docs/legacy/` is historical (the pre-`orca` "brain" design) and is not kept
current.

## Make targets

| Target | Description |
|--------|-------------|
| `make init` | Verify/install build prerequisites |
| `make install` | Install git hooks + toolchain + cargo tooling |
| `make dev` | Hot-reload dev mode (dev.sh: Rust API :12000 + Vite :12001) |
| `make build` | Build frontend + release binary (no install) |
| `make deploy` | Build, install to `~/.local/bin/orca`, install the daemon |
| `make run` | Run the installed binary with 1Password secrets |
| `make test` | vitest + cargo nextest + doctests |
| `make lint` | prettier + eslint + clippy (`-D warnings`) |
| `make format` | rustfmt + prettier (+ taplo for TOML) |
| `make check` | `cargo check --workspace` (no link) |
| `make audit` | npm audit + cargo audit |
| `make migration [up\|down\|status\|<slug>]` | Apply/scaffold DB migrations |
| `make clean` | Remove build artifacts |
| `make sync` | Refresh synced OpenAPI specs from upstream repos |

Releases are user-owned. Never run `make release`/`make deploy` or
`gh release create` from an agent, and never `git commit`.
