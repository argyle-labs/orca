# Orca — orca project

Load the `FRONTEND` config doc for frontend conventions via the `config_detail` tool over the `orca` MCP (use `config_list` to find it).

Working directory: `~/code/argyle-labs/orca` — work here directly, one stream at a time. Do not create git worktrees.

MCP server: `orca` (HTTP, served by the daemon at `/api/mcp`; port in `~/.orca/http.port`).

## Rust style rules

- Never write nested `if` / `if let` when clippy's `collapsible_if` lint applies. Always collapse using `&&` let-chains: `if cond && let Some(x) = expr { ... }`.
