# Orca — orca project

Load `orca_get_config("FRONTEND")` for frontend conventions.

Working directory: `~/code/argyle-labs/orca`. MCP server: `orca-local`.

## Rust style rules

- Never write nested `if` / `if let` when clippy's `collapsible_if` lint applies. Always collapse using `&&` let-chains: `if cond && let Some(x) = expr { ... }`.
