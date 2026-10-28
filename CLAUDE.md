# Orca — orca project

Load the `FRONTEND` config doc for frontend conventions via the `config_detail` tool over the `orca` MCP (use `config_list` to find it).

Working directory: `~/code/argyle-labs/orca` — work here directly, one stream at a time. Do not create git worktrees.

MCP server: `orca` (HTTP, served by the daemon at `/api/mcp`; port in `~/.orca/http.port`).

## Rust style rules

- Never write nested `if` / `if let` when clippy's `collapsible_if` lint applies. Always collapse using `&&` let-chains: `if cond && let Some(x) = expr { ... }`.
- Comments: the code should be self-describing. Add a comment ONLY for what the code itself can't show — a non-obvious constraint, invariant, or reason. Default to one line; multi-line only when the non-obvious part genuinely needs it. Never restate what a well-named item already says.
