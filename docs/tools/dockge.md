# `dockge.*` — Dockge stack-manager plugin

Manages registered Dockge endpoints and proxies stack lifecycle ops through them.

Every tool surfaces identically on three transports — **CLI**, **MCP**, and **REST** — generated from the same `#[orca_tool]` declaration.

## Endpoint registry vs stack ops

Dockge follows the REST verb convention ([[feedback-rest-verbs-for-tool-surfaces]]):

| Verb | Semantics | Errors if |
|---|---|---|
| `dockge.list` | GET collection (or stacks on one endpoint) | — |
| `dockge.detail` | GET logs for one stack | endpoint/stack not found |
| `dockge.create` | POST a new endpoint | name already exists |
| `dockge.update` | PATCH an endpoint, or run a stack action | endpoint not registered |
| `dockge.delete` | DELETE an endpoint | — (idempotent) |

Stack lifecycle actions (`start` / `stop` / `restart`) ride on `.update` via an `action` arg — stack is the resource, lifecycle is a state transition.

---

## `dockge.create` — register a new endpoint

### CLI

```sh
orca dockge create \
  --name dockge-a \
  --base-url http://127.0.0.1:5001 \
  --token <bearer-token>
```

### MCP

```json
{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
  "name": "dockge.create",
  "arguments": {
    "name": "dockge-a",
    "base_url": "http://127.0.0.1:5001",
    "token": "<bearer-token>"
  }
}}
```

### REST

```sh
curl -X POST http://localhost:7474/api/v1/dockge.create \
  -H "Content-Type: application/json" \
  -d '{
    "name": "dockge-a",
    "base_url": "http://127.0.0.1:5001",
    "token": "<bearer-token>"
  }'
```

### Output

```json
{
  "name": "dockge-a",
  "base_url": "http://127.0.0.1:5001",
  "enabled": true
}
```

### Errors

- `dockge endpoint 'dockge-a' already exists; use dockge.update` — use `dockge.update` to modify an existing row.

---

## `dockge.list` — endpoints, or stacks on one endpoint

### CLI

```sh
orca dockge list                       # → registered endpoints
orca dockge list --endpoint dockge-a   # → stacks on dockge-a
```

### MCP

```json
{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
  "name": "dockge.list",
  "arguments": {"endpoint": "dockge-a"}
}}
```

### REST

```sh
curl -X POST http://localhost:7474/api/v1/dockge.list \
  -H "Content-Type: application/json" \
  -d '{"endpoint": "dockge-a"}'
```

### Output (collection mode)

```json
{
  "endpoints": [
    {"name": "dockge-a", "baseUrl": "http://127.0.0.1:5001", "enabled": true}
  ]
}
```

### Output (drill-in mode)

```json
{ "stacks": { /* upstream Dockge payload */ } }
```

---

## `dockge.detail` — recent logs for one stack

### CLI

```sh
orca dockge detail dockge-a webapp
```

### MCP

```json
{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
  "name": "dockge.detail",
  "arguments": {"endpoint": "dockge-a", "stack": "webapp"}
}}
```

### REST

```sh
curl -X POST http://localhost:7474/api/v1/dockge.detail \
  -H "Content-Type: application/json" \
  -d '{"endpoint": "dockge-a", "stack": "webapp"}'
```

### Output

Opaque upstream Dockge logs payload.

---

## `dockge.update` — modify endpoint, or run a stack action

### CLI

Modify endpoint:

```sh
orca dockge update --name dockge-a --token <new-token>
orca dockge update --name dockge-a --enabled false
```

Stack action:

```sh
orca dockge update --endpoint dockge-a --stack webapp --action start
orca dockge update --endpoint dockge-a --stack webapp --action stop
orca dockge update --endpoint dockge-a --stack webapp --action restart
```

### MCP

```json
{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
  "name": "dockge.update",
  "arguments": {"endpoint": "dockge-a", "stack": "webapp", "action": "restart"}
}}
```

### REST

```sh
curl -X POST http://localhost:7474/api/v1/dockge.update \
  -H "Content-Type: application/json" \
  -d '{"endpoint": "dockge-a", "stack": "webapp", "action": "restart"}'
```

### Output

```json
{
  "applied": ["stack:dockge-a:webapp:restart"],
  "action_status": 200
}
```

### Errors

- `dockge endpoint 'X' not registered; use dockge.create` — endpoint must exist before `.update` can modify it.
- `stack action requires endpoint + stack + action together` — partial action args rejected.

---

## `dockge.delete` — remove a registered endpoint

### CLI

```sh
orca dockge delete dockge-a
```

### MCP

```json
{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
  "name": "dockge.delete",
  "arguments": {"name": "dockge-a"}
}}
```

### REST

```sh
curl -X POST http://localhost:7474/api/v1/dockge.delete \
  -H "Content-Type: application/json" \
  -d '{"name": "dockge-a"}'
```

### Output

```json
{ "name": "dockge-a", "changed": true }
```

`changed: false` means no row matched (idempotent — not an error).

---

## Cross-transport invariants

- **Same args, same output, same errors** across CLI, MCP, REST.
- **Same name** as the tool ID: `dockge.create` is the MCP tool name and the REST path tail. The CLI splits the dot: `orca dockge create`.
- **REST endpoint:** every tool is at `POST /api/v1/<tool-name>` with the args as the JSON body.
- **MCP:** every tool is callable via `tools/call` with `name` and `arguments`.
- **CLI:** every tool is `orca <domain> <verb> [args]`.

If parity breaks, that's a bug in the `#[orca_tool]` macro emission — not a per-plugin concern.
