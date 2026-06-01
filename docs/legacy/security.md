# Security Model

brain is a local developer tool. Its threat model is a trusted user on a trusted machine, not an internet-facing service.

## Intended deployment

- Runs on localhost
- Used by one person (the developer)
- Not exposed to the internet

In dev mode (`brain serve --dev`), the server binds `127.0.0.1` — only loopback connections are accepted.

## Known gaps (as of April 2026)

These are documented here because they are **known and accepted** for the current use case. They become relevant if the server is ever exposed beyond localhost.

### No authentication

There is no auth on any endpoint. Any process or host that can reach port 12000 can invoke docker actions, trigger MCP tool calls, and read vault content.

**Accepted because:** In dev mode, only loopback. In prod mode (`brain serve`), it binds `0.0.0.0` — this is intentional for LAN access from a browser on another device (e.g., phone checking homelab status).

**Mitigation if needed:** Add a shared-secret token in `brain.toml`, validated in an axum middleware layer.

### SQL injection via config interpolation

Database name from `brain.toml` is interpolated into SQL queries sent to MySQL via the CLI. If the config file is tampered with, arbitrary SQL can be injected.

**Accepted because:** The config file is controlled by the developer. If an attacker can write to `brain.toml`, the machine is already compromised.

**Mitigation if needed:** Validate `cfg.database` against `[a-zA-Z0-9_-]+` on load.

### Path traversal in /api/specs/:repo

The `:repo` path segment is not validated before being joined into a filesystem path. A crafted value could read arbitrary `.json` files outside the specs directory.

**Mitigation (easy, not yet done):** Validate `:repo` against `^[a-zA-Z0-9_-]+$`.

### CORS: allow_origin(Any)

All origins are permitted on all endpoints. This is fine with no auth — CORS only matters if auth is added later.

**Mitigation if auth is added:** Restrict to specific origins.

### Raw MySQL errors in HTTP responses

MySQL error output (including SQL text and server version) is returned in HTTP 500 responses.

**Mitigation:** Log internally; return a generic message to the caller.

## What IS secure

- API key stored in macOS Keychain, not in files or environment
- `doc_handler` checks `full.starts_with(root_dir)` before reading (path traversal blocked)
- Bash tool uses `Command::new("bash").arg("-c")` — not a shell-interpolated string from user input
- Docker tool uses `Command::new("docker").args(...)` — no shell injection possible (option injection is lower severity)
- Search queries are `regex::escape()`d before pattern matching
