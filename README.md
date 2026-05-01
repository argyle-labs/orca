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

`make dev` uses the 1Password CLI to inject secrets. It requires `OP_ACCOUNT` set in your environment — this is configured in `dotfiles/.zshrc`. On a new machine, ensure dotfiles are installed (`~/dotfiles/install.sh`) before running `make dev`.

Find your account UUID: `op account list`

## Usage

```sh
brain                          # interactive session (auto-detects project from cwd)
brain <project>                # session with project context loaded
brain run -a fox "why is this failing?"  # one-shot agent delegation
brain serve                    # start web UI on :12000
brain mcp-serve                # MCP stdio server (register with Claude Code)
```

Register as MCP server:
```sh
claude mcp add brain-local -- brain mcp-serve
```

## Docs

- [Architecture](projects/docs/src/architecture.md) — how the four roles (CLI, TUI, web server, MCP) fit together
- [Repo structure](projects/docs/src/repo-structure.md) — where everything lives and why
- [API](projects/docs/src/api.md) — HTTP endpoints
- [MCP server](projects/docs/src/mcp-server.md) — tools exposed to Claude Code
- [Frontend](projects/docs/src/frontend.md) — React site and code generation
- [Agent model](projects/docs/src/agent-model.md) — agent loading, delegation, and the vault
- [Local-first model policy](projects/docs/src/local-first.md) — LM Studio default, Claude escalation
- [Stack](projects/docs/src/stack.md) — language/framework choices and why
- [Testing](projects/docs/src/testing.md) — test suites and how to run them
- [Security](projects/docs/src/security.md) — auth, keychain, permission model

## Config

`~/brain/config/brain.toml` — MCP servers, schema databases. See `brain.toml.tpl` for all options.

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
