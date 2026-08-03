# Canonical Sources

Where to find the authoritative type, schema, and documentation sources in the
orca repo. Reference this file instead of repeating source locations in each
agent. Every path below is relative to the repo root (`~/code/argyle-labs/orca`)
and must be re-verified against the tree before you rely on it — code wins.

## Architecture & orientation

| What you need | Where it lives |
|---------------|----------------|
| Crate responsibilities (the crate map) | [`CRATE_RESPONSIBILITIES.md`](../../../CRATE_RESPONSIBILITIES.md) |
| System architecture overview | [`docs/architecture.md`](../../../docs/architecture.md) |
| Repo layout / where things live | [`docs/repo-structure.md`](../../../docs/repo-structure.md) |
| Guided codebase tour | [`docs/dev/00-tour.md`](../../../docs/dev/00-tour.md), [`docs/learn/codebase-tour.md`](../../../docs/learn/codebase-tour.md) |
| Contribution standards | [`CONTRIBUTING.md`](../../../CONTRIBUTING.md) |
| Documentation rules | [`docs/DOCUMENTATION-GUIDELINES.md`](../../../docs/DOCUMENTATION-GUIDELINES.md) |
| Out-of-process plugin model | [`docs/OUT-OF-PROCESS-PLUGINS.md`](../../../docs/OUT-OF-PROCESS-PLUGINS.md) |

## Types, contracts & tool surface

| What you need | Where it lives |
|---------------|----------------|
| Shared contract across every tool surface | `projects/contract/` (the `contract` crate) |
| Coding-agent config docs (this dir) | [`projects/contract/config-docs/`](.) |
| Tool dispatch macro (`#[orca_tool]` / `#[endpoint_tool]`) | `projects/derive/` (proc-macro) + `projects/dispatch/` (runtime) |
| Core shared types (`Message`, `ToolCall`, `ToolResult`) | `projects/utils/src/types.rs` |
| Agent domain (tools, prompt resolution; roster is external) | `projects/agents/src/` |
| MCP serving core | `projects/mcp/` |
| OpenAPI / GraphQL / spec integration | `projects/openapi/`, `projects/graphql/`, `projects/spec/` |

## Data & schema

| What you need | Where it lives |
|---------------|----------------|
| Encrypted SQLite runtime registry (pool/schema/migration primitives) | `projects/db/` |
| Domain tables | owned by each domain crate; `db` provides the pool/schema/migration/replication primitives |
| Backup / restore subsystem | `projects/system/src/backup/`, [`docs/BACKUP-SUBSYSTEM.md`](../../../docs/BACKUP-SUBSYSTEM.md) |

## Build, test & CI

| What you need | Where it lives |
|---------------|----------------|
| Build / format / lint / test targets | [`Makefile`](../../../Makefile) — `make format` (rustfmt + taplo), `make lint` (clippy), `make test` (nextest + doctests) |
| Coverage floor | [`.coverage-floor`](../../../.coverage-floor) |

## Hard rules

- **Verify before you cite.** Every path here is checked at write time; if a path
  moved, fix this file in the same pass rather than working around it.
- **Never guess at types.** Look them up in the crate above. If a type does not
  exist, that is the real finding — not a type cast.
- **Link, don't duplicate.** Point at the source file and describe what it does;
  never paste a `struct`/`enum`/route table that will drift.
