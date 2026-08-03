# Orca — orca project

Load the `FRONTEND` config doc for frontend conventions via the `config_detail` tool over the `orca-local` MCP (use `config_list` to find it).

Working directory: `~/code/argyle-labs/orca`. MCP server: `orca-local`.

## Rust style rules

- Never write nested `if` / `if let` when clippy's `collapsible_if` lint applies. Always collapse using `&&` let-chains: `if cond && let Some(x) = expr { ... }`.
