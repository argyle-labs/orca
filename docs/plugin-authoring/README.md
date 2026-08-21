# Plugin Authoring

How to write an orca plugin. Current state — how it works now. Each page covers
one concept; read the ones you need.

Orca has two plugin mechanisms:

| | **Native subprocess plugin** | **Manifest plugin (`orca-plugin.toml`)** |
|---|---|---|
| Language | Rust | any (MCP SDK) |
| Runs | own process, spoken to over a Unix socket | external process / HTTP endpoint |
| Tool model | `#[orca_tool]` + inventory, served by a `serve_*_plugin!` macro | MCP tools over stdio / HTTP-SSE |
| Author depends on | `plugin-toolkit` | the MCP SDK of your language |
| When | first-party integrations, typed orca contracts | non-Rust, third-party, experimental |

Both run **out-of-process**: each plugin is its own program orca talks to over a
socket. The mechanism (wire protocol, capability delegation, loader supervisor)
is [`../plugin-loading.md`](../plugin-loading.md); the design rationale is
[`../OUT-OF-PROCESS-PLUGINS.md`](../OUT-OF-PROCESS-PLUGINS.md). These pages are
how to *write* one.

## Native subprocess plugins (Rust)

1. [Anatomy & the serve loop](native-plugin.md) — crate shape, `Cargo.toml`, the
   `serve_*_plugin!` family, the zero-tool dead-strip guard.
2. [Registering tools](registering-tools.md) — `#[orca_tool]`, the real tool
   signature, and which of the three mechanisms to use.
3. [CRUD resources](endpoint-resource-crud.md) — `endpoint_resource!` for a
   stored `{list,detail,create,update,delete}` surface.
4. [Unit-shaped resources](unit-provider.md) — containers/VMs via
   `contract::unit::UnitProvider` (not `endpoint_resource!`).
5. [Backend plugins](backends.md) — secrets / service / storage / backup.
6. [Toolkit capabilities](toolkit-capabilities.md) — the capabilities orca
   exposes and the tools/macros the toolkit gives a plugin author.

## Manifest plugins & shared surfaces

7. [Manifest plugins](manifest-plugins.md) — `orca-plugin.toml`, MCP servers.
8. [Agents & plugin data](agents-and-data.md) — contributing agents and storing
   per-plugin data.

## Keeping this doc honest

The code snippets on these pages are pinned against real source so a renamed or
removed symbol **fails orca CI**:

- **Compiled anchor** — [`../../projects/plugin-toolkit/tests/doc_authoring_anchor.rs`](../../projects/plugin-toolkit/tests/doc_authoring_anchor.rs)
  applies `#[orca_tool]` and `#[endpoint_resource]` with the documented forms,
  imports the `serve_*_plugin!` family, and coerces every cited toolkit function
  to its documented type. `cargo nextest` compiles it in CI; a rename/removal
  breaks the build.
- **Path check** — the same file's `doc_paths_resolve` test asserts every
  `projects/…` path (and `:line`) these pages cite still exists.

Cross-repo links to sibling plugins (`argyle-labs/jellyfin`, `…/docker`,
`…/onepassword`, `…/ntfy`) are **illustrative** — orca CI cannot compile another
repo, so they are not path-checked. The in-repo authority files are.
