# REST API

The brain web server exposes a REST API on `:12000`. All endpoints are under `/api/`. The full utoipa-generated OpenAPI spec is at `GET /api/openapi.json`.

## Docs endpoints

Serve content from the brain vault and embedded repo docs.

| Endpoint | Description |
|----------|-------------|
| `GET /api/tree?root=<root>&path=<subpath>` | Doc tree for a root, optionally scoped to a subpath |
| `GET /api/search?q=<query>&root=<root>` | Full-text search; root defaults to all |
| `GET /api/doc?root=<root>&path=<path>` | Read a doc file as text |

**Roots:** `brain` (vault), `rebuy` (`~/code/rebuy/`), `docs` (embedded binary docs)

## OpenAPI spec endpoints

External OpenAPI specs. Disk-scanned specs and URL-registered specs are merged in list responses.

| Endpoint | Description |
|----------|-------------|
| `GET /api/specs` | All registered specs (disk + URL-registered from brain.db) |
| `GET /api/specs/db` | URL-registered specs only (from brain.db, with metadata) |
| `GET /api/specs/:repo` | Full OpenAPI JSON for a repo (disk first, DB fallback) |
| `GET /api/specs/:repo/public` | Public-facing spec only |
| `GET /api/specs/:repo/graphql` | GraphQL SDL as plain text |
| `GET /api/specs/:repo/graphql/info` | Parsed GraphQL schema (queries, mutations, types, enums) |
| `POST /api/specs/:repo/graphql/proxy` | Proxy a GraphQL query to a live shop |
| `POST /api/specs/register` | Fetch OpenAPI JSON from a URL and store in brain.db |
| `POST /api/specs/:name/refresh` | Re-fetch a URL-registered spec from its stored URL |
| `DELETE /api/specs/:name/unregister` | Remove a URL-registered spec from brain.db |

**Register request body:** `{ "name": "my-api", "url": "https://api.example.com/openapi.json" }`

**Spec download:** append `?download=true` to any spec endpoint for a `Content-Disposition: attachment` response. Append `?format=yaml` for YAML instead of JSON.

## MCP proxy endpoints

Proxy calls to MCP servers. Includes both brain-owned tools and federated tools from registered servers.

| Endpoint | Description |
|----------|-------------|
| `GET /api/mcp/tools` | All tools from all connected MCP servers (brain + federated) |
| `POST /api/mcp/run` | Invoke a tool: `{ "server": "...", "name": "...", "arguments": {} }` |
| `GET /api/mcp/servers` | List MCP servers registered in brain.db |
| `POST /api/mcp/servers` | Add/update an MCP server: `{ "name": "...", "command": "...", "args": [], "env": {} }` |
| `DELETE /api/mcp/servers/:name` | Remove an MCP server from brain.db |

## Schema database endpoints

MySQL/MariaDB schema visualizer. Connection details come from brain.db.

| Endpoint | Description |
|----------|-------------|
| `GET /api/schema` | Tables and columns for all configured schema databases |
| `GET /api/schema/domains` | Group tables by domain prefix |
| `GET /api/schema/databases` | List schema databases registered in brain.db |
| `POST /api/schema/databases` | Add/update a schema database |
| `DELETE /api/schema/databases/:name` | Remove a schema database |

**Add request body:**
```json
{
  "name": "rebuy",
  "database": "rebuy",
  "user": "root",
  "password": "secret",
  "container": "rebuy-mysql",
  "host": null,
  "port": null,
  "domainsFile": null
}
```
Use `container` OR `host`/`port`, not both. `container` uses `docker exec` to connect.

## Docker endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/docker/runtimes` | List Docker runtimes registered in brain.db |
| `POST /api/docker/runtimes` | Add/update a Docker runtime |
| `DELETE /api/docker/runtimes/:name` | Remove a Docker runtime |
| `GET /api/docker/engine` | List Docker Compose services for a project path |
| `POST /api/docker/engine/start` | Start Docker engine (Colima) |
| `GET /api/docker/services` | All services across all rebuy projects |
| `POST /api/docker/action` | Start/stop/restart/logs for a service |

**Add runtime request body:**
```json
{
  "name": "colima",
  "socketPath": "~/.colima/default/docker.sock",
  "host": null,
  "url": null
}
```
Use `socketPath` for local Unix socket (Colima, Docker Desktop), `host` for remote TCP (`tcp://host:2376`), or `url` for web-based orchestrators (Dockge, Portainer).

## Log endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/logs/services` | All Compose services across all rebuy projects (for log service list) |
| `GET /api/logs?project=<path>&service=<name>&tail=<n>` | Recent log lines for a service |

## Health endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/health` | Ping all configured rebuy local services, return status |

## System endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/system/status` | Brain installation status (binary, MCP registration, symlinks) |
| `POST /api/system/action` | Install or uninstall brain: `{ "action": "install" \| "uninstall" }` |

## Test runner endpoint

| Endpoint | Description |
|----------|-------------|
| `POST /api/tests/run?suite=<suite>` | Run a test suite: `rust \| frontend \| e2e \| all` |

## Atlassian endpoints (Jira + Confluence)

| Endpoint | Description |
|----------|-------------|
| `GET /api/jira/issues?project=<key>&...` | List Jira issues for a project |
| `GET /api/jira/issues/:key/transitions` | Get available status transitions |
| `POST /api/jira/issues/:key/transition` | Apply a status transition |
| `GET /api/confluence/search?q=<query>` | Search Confluence |

## Bitbucket endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/repos` | List repos |
| `GET /api/prs?repo=<slug>` | List open PRs for a repo |

## Context7 proxy endpoint

| Endpoint | Description |
|----------|-------------|
| `POST /api/ctx7` | Proxy a context7 library docs request |

## Learning progress endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/progress` | Get learning progress for all repos |
| `POST /api/progress` | Save a progress update |

## OpenAPI spec

`GET /api/openapi.json` — full utoipa-generated spec for all endpoints.
`GET /api/openapi/public.json` — public endpoints only (tagged `[public]`).

The spec is assembled at server startup by `utoipa-axum` — each handler annotates itself with `#[utoipa::path(...)]` and the spec is built from those annotations. Used by the brain site's API reference viewer and by `brain gen` to generate the TypeScript client.

## Why axum

axum is chosen for its close integration with tokio (already required for LM Studio streaming) and for its type-safe extractors. The `utoipa-axum` crate wires handler annotations directly into the router, eliminating a separate OpenAPI schema file.

## Adding a new endpoint

1. Create a handler in `projects/server/src/serve/api/<domain>.rs` with `#[utoipa::path(...)]`
2. Export the handler from `api/mod.rs` via `pub use <domain>::*;`
3. Register in `projects/server/src/serve/openapi.rs` → `openapi_router()` with `.routes(routes!(handler))`
4. Add any new request/response types to `api/mod.rs` and to `#[openapi(components(schemas(...)))]` in `openapi.rs`
5. Run `brain gen` to regenerate the TypeScript client
