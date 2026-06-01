# Operational Rules

Guidelines for running, deploying, and maintaining brain in practice.

## Dev Workflow

```sh
make dev        # starts cargo-watch (brain serve --dev) + Vite HMR on :12001
make build      # build site + release binary (runs spec sync + codegen first)
make install    # build + install to ~/.local/bin/brain
make spec       # sync all OpenAPI specs for supported rebuy repos
```

In dev mode:
- Rust server handles `/api/*` on `:12000`
- Vite HMR on `:12001` proxies `/api/*` to `:12000`
- Frontend changes are hot-reloaded; Rust changes restart the server
- Site is NOT embedded in the debug binary (only release builds embed it)
- After changing any API handler, run `cd projects/frontend && npm run gen` to regenerate hooks

## Deployment

brain deploys as a single self-contained binary. The React site, embedded docs, and all agents are compiled in.

```sh
make install    # build release binary + install to ~/.local/bin/brain
brain daemon start  # cooperative port handoff — parks the old process and starts the new one
```

The `brain daemon` command handles zero-downtime redeploy: sends `SIGUSR1` to the running process (parks it), starts the new binary, then sends `SIGUSR2` to terminate the old one.

## Configuration Locations

| What | Where | Managed by |
|------|-------|------------|
| LLM endpoints, API keys, shopify version | `~/brain/config/brain.toml` | Text editor |
| MCP server definitions | `~/.brain/brain.db` | `brain mcp add/remove` |
| Schema database connections | `~/.brain/brain.db` | `brain schema add/remove` |
| Docker runtime configs | `~/.brain/brain.db` | `brain docker add/remove` |
| OpenAPI specs (URL-fetched) | `~/.brain/brain.db` | `brain spec register/refresh` |
| OpenAPI specs (scanned) | `~/brain/rebuy/openapi/specs/` | `brain spec sync` |
| Agent memory | `~/.brain/memory/<project>/` | Auto-memory + `brain memory` |
| Session logs | `~/.brain/logs/sessions/` | Auto (pinky agent) |

Never edit brain.db directly. All CRUD goes through the CLI. The DB key lives at `~/.brain/.db_key`.

## MCP Server Registration

brain's own MCP server is registered with Claude Code once:
```sh
claude mcp add brain-local -- brain mcp-serve
```

To register additional external MCP servers for federation:
```sh
brain mcp add rebuy-cli --command node --args ~/code/rebuy/rebuy-cli/dist/mcp-server.js
```

Federated servers' tools appear in Claude Code as `mcp__brain-local__<tool>` after brain proxies them. brain skips `brain-local` and `context7` from federation to prevent loops.

## OpenAPI Spec Registry

Two mechanisms for spec ingestion:

1. **Scan** (local repos): `brain spec sync <repo>` or `brain spec sync --all`
   - Reads source code, generates OpenAPI JSON
   - Writes to `~/brain/rebuy/openapi/specs/<repo>.json`
   - Supported: `admin-api`, `apiv2`, `rebuyengine`, `admin-nextjs`, `rebuy-shopify-client`, `shopify-admin`

2. **Register** (URL): `brain spec register <name> --url <url>`
   - Fetches JSON from URL, stores in brain.db
   - Refresh: `brain spec refresh <name>` or `brain spec refresh --all`
   - These specs appear in `GET /api/specs` merged with disk specs

## Docker Runtime Detection

On first startup, brain auto-detects `~/.colima/default/docker.sock` and registers a "colima" runtime if the `docker_runtimes` table is empty. Additional runtimes (Docker Desktop, remote hosts, Dockge, Portainer) are registered manually:

```sh
brain docker add docker-desktop --socket ~/.docker/run/docker.sock
brain docker add staging --host tcp://staging:2376
brain docker add dockge --url https://dockge.internal
```

The first enabled socket/host runtime is injected as `DOCKER_HOST` for MCP subprocess calls.

## Hooks

Hooks in `hooks/` run as Claude Code event handlers:

| Hook | Trigger | Purpose |
|------|---------|---------|
| `bash-destructive-guard.sh` | Pre-Bash | Blocks destructive shell commands (rm -rf, git reset --hard, etc.) |
| `opnsense-guard.sh` | Pre-Bash | Blocks commands targeting OPNsense/pfSense network appliances |
| `pii-scanner.sh` | Pre-Write | Warns before writing files containing PII patterns |
| `pre-commit-secrets-scan.sh` | Pre-Bash (git commit) | Scans staged files for secrets/credentials |
| `glob-cache-check.py` | Pre-Glob | Validates glob patterns against cache |
| `glob-cache-write.py` | Post-Glob | Writes glob results to cache |
| `session-start.sh` | Session start | Logs session start event |
| `session-stop.sh` | Session stop | Logs session stop event |

## Health Checks

```sh
brain doctor    # validates agents, symlinks, vault structure, stale wolf.md refs
brain auth      # shows auth status for all services (GitHub, Atlassian, Keychain)
```

The web UI at `GET /api/health` runs rebuy service health checks (pings local docker services).

## Secrets Management

- **Anthropic API key** — stored in macOS Keychain via `brain auth`. Never in `.env` files.
- **brain.db encryption key** — stored at `~/.brain/.db_key`. Back this up. Without it, the DB cannot be opened.
- **GitHub OAuth tokens** — stored in Keychain via device flow (`brain login github`)
- **Atlassian tokens** — stored in Keychain via OAuth flow (`brain login atlassian`)

1Password CLI (`op`) is used in `make dev` to inject secrets at dev time only. Production runs don't use 1Password.

## Troubleshooting

**`brain mcp-serve` fails after a DNS operation crashes it:**
The MCP client pool caches dead connections. If a tool call fails with "MCP server closed", the dead client is evicted from the pool on the next call. The pool reconnects automatically.

**Frontend shows stale data:**
Run `brain gen` to regenerate `src/api/` from the current OpenAPI spec, then rebuild.

**Spec sync fails for a repo:**
Ensure `REBUY_ROOT` is set or the repo exists at `~/code/rebuy/<repo>`. Missing repos are skipped with a warning when `--all` is used.

**DB key missing:**
The `~/.brain/.db_key` file must exist. If lost, the DB must be recreated. All registries (MCP servers, schema DBs, Docker runtimes, specs) will need to be re-added.
