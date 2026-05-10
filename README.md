# brain

Local-first AI agent orchestrator. Single self-contained binary with an embedded web UI. LM Studio runs everything by default — Claude is escalation only.

## Installation

**From a GitHub release** (pre-built binary):

```sh
# Apple Silicon
curl -Lo brain https://github.com/scottdkey/brain/releases/latest/download/brain-aarch64-apple-darwin

# Intel Mac
curl -Lo brain https://github.com/scottdkey/brain/releases/latest/download/brain-x86_64-apple-darwin

chmod +x brain && mv brain ~/.local/bin/brain
```

macOS blocks unsigned binaries downloaded from the internet. Clear the quarantine flag before running:
```sh
xattr -d com.apple.quarantine ~/.local/bin/brain
```

**From source:**

```sh
make init     # install rust, node, cargo-watch, cargo-audit; npm install
make install  # build site + release binary, install to ~/.local/bin/brain
```

## Setup

### Local dev secrets

`make dev` uses the 1Password CLI to inject secrets. It requires `OP_ACCOUNT` set in your environment — configured in `dotfiles/.zshrc`. On a new machine, ensure dotfiles are installed (`~/dotfiles/install.sh`) before running `make dev`.

Find your account UUID: `op account list`

## Usage

```sh
brain                          # interactive session (auto-detects project from cwd)
brain <project>                # session with project context loaded
brain run -a fox "why is this failing?"  # one-shot agent delegation
brain serve                    # start web UI on :12000
brain mcp-serve                # MCP stdio server (register with Claude Code)
```

Register as MCP server with Claude Code:
```sh
claude mcp add brain-local -- brain mcp-serve
```

### Registry management

brain tracks MCP servers, schema databases, Docker runtimes, and OpenAPI specs in `~/.brain/brain.db` (encrypted SQLite/SQLCipher). Everything is managed through the CLI:

```sh
# MCP servers
brain mcp list
brain mcp add rebuy-cli --command node --args /path/to/server.js
brain mcp remove rebuy-cli

# Schema databases
brain schema list
brain schema add rebuy --database rebuy --user root --password secret --container rebuy-mysql
brain schema remove rebuy

# Docker runtimes
brain docker list
brain docker add colima --socket ~/.colima/default/docker.sock
brain docker add dockge --url https://dockge.internal
brain docker remove colima

# OpenAPI spec registry
brain spec list
brain spec register my-api --url https://api.example.com/openapi.json
brain spec refresh my-api          # re-fetch from stored URL
brain spec refresh --all           # refresh all URL-registered specs
brain spec unregister my-api
brain spec add admin-api           # register a local repo for scanning
brain spec sync admin-api          # scan the repo and generate a spec file
brain spec sync --all              # scan all supported repos
```

## Config

Runtime config is split across two locations:
- `~/brain/config/brain.toml` — app settings (LLM endpoints, API keys, shopify admin version, etc.)
- `~/.brain/brain.db` — registry data (MCP servers, schema DBs, Docker runtimes, OpenAPI specs)

See [`brain.toml.tpl`](projects/server/) for all toml options. Registries are managed via `brain mcp|schema|docker|spec` CLI commands — do not edit the DB directly.

## Docs

- [Architecture](docs/architecture.md) — how the four roles (CLI, TUI, web server, MCP) fit together
- [Repo structure](docs/repo-structure.md) — where everything lives and why
- [API](docs/api.md) — HTTP endpoints, registry CRUD, spec management
- [MCP server](docs/mcp-server.md) — tools exposed to Claude Code + federation model
- [Frontend](docs/frontend.md) — React site, code generation, patterns
- [Agent model](docs/agent-model.md) — agent loading, delegation, and the vault
- [Local-first model policy](docs/local-first.md) — LM Studio default, Claude escalation
- [Stack](docs/stack.md) — language/framework choices and why
- [Testing](docs/testing.md) — test suites and how to run them
- [Security](docs/security.md) — auth, keychain, permission model
- [Architecture deep-dive](docs/brain-architecture.md) — crate map, dependency flow, design decisions

## Make targets

| Target | Description |
|--------|-------------|
| `make dev` | Hot-reload dev mode (cargo-watch + Vite HMR) |
| `make build` | Build site + release binary |
| `make install` | Build and install to `~/.local/bin/brain` |
| `make test` | vitest + cargo test |
| `make lint` | eslint + cargo clippy |
| `make format` | prettier + cargo fmt |
| `make check` | Type-check only (no build) |
| `make audit` | npm audit + cargo audit |
| `make clean` | Remove build artifacts |
| `make spec` | Sync all OpenAPI specs for supported rebuy repos |
