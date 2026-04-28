# REST API

The brain web server exposes a REST API on `:12000`. All endpoints are under `/api/`.

## Docs endpoints

These serve content from the brain vault (`~/brain`) and the embedded repo docs.

| Endpoint | Description |
|----------|-------------|
| `GET /api/tree` | Full doc tree for all roots (brain, rebuy, docs) |
| `GET /api/search?q=<query>&root=<root>` | Full-text search; root defaults to all |
| `GET /api/doc?root=<root>&path=<path>` | Read a doc file as plain text |

**Roots:**
- `brain` — `~/brain/` vault (excludes logs, memory, plugins from tree; memory is searchable)
- `rebuy` — `~/code/rebuy/` (excludes node_modules, dist, vendor, etc.)
- `docs` — this `docs/` directory, compiled into the binary (always available)

## Specs endpoints

External OpenAPI specs registered in `~/brain/config/brain.toml`.

| Endpoint | Description |
|----------|-------------|
| `GET /api/specs` | List all registered specs |
| `GET /api/specs/:repo` | Get the full spec JSON for a repo |
| `GET /api/specs/:repo/public` | Get the public-facing spec (scrubbed) |

## MCP proxy endpoints

Proxies calls to MCP servers configured in `~/brain/config/brain.toml`.

| Endpoint | Description |
|----------|-------------|
| `GET /api/mcp/tools` | All tools from all connected MCP servers |
| `POST /api/mcp/run` | Invoke a tool: `{ server, name, arguments }` |

## Docker endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/docker/services?path=<dir>` | List services from a compose file in `<dir>` |
| `POST /api/docker/action` | Start/stop/restart a service |
| `GET /api/logs/services` | All compose services across all rebuy projects |
| `GET /api/logs?project=<path>&service=<name>` | Fetch recent log lines |

## Schema endpoints

Reads MySQL schema via the `mysql` CLI. Connection details come from `brain.toml`.

| Endpoint | Description |
|----------|-------------|
| `GET /api/schema` | Tables and columns for configured database |
| `GET /api/schema/domains` | Group tables by domain prefix |

## OpenAPI spec

`GET /api/openapi.json` returns the full utoipa-generated spec for all endpoints. The spec is generated at runtime — handlers annotate themselves with `#[utoipa::path(...)]` and the spec is assembled when first requested.

## Why axum

axum was chosen for its close integration with tokio (the async runtime already required for LM Studio streaming) and for its type-safe extractors. The alternatives (actix-web, warp) would have required additional async bridge code.

## Why utoipa

utoipa generates OpenAPI specs from proc macros on the handler functions. This keeps the spec in sync with the implementation without a separate schema file to maintain. The spec is used by the brain site's API reference viewer and by external tooling.
