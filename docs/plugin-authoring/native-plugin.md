# Native plugin: anatomy & the serve loop

An `rlib` crate with a `[[bin]]` target. orca runs the binary as a persistent
child process; it connects back over a Unix socket, sends a `Hello` declaring its
tools, then serves invocations — delegating HTTP / DB / secret work to the daemon
as capabilities.

```
my-plugin/
├── Cargo.toml          ← rlib crate with a [[bin]] target
├── build.rs            ← (optional) codegen typed clients from OpenAPI/GraphQL
├── specs/              ← (optional) vendored spec files
└── src/
    ├── main.rs         ← the Plugin builder chain (the whole fn main)
    └── tools.rs        ← #[orca_tool] functions
```

## `Cargo.toml`

`plugin-toolkit` is the **only** orca dependency a plugin needs. It re-exports
the contract, dispatch, the wire protocol, and the runtime deps a plugin uses
(`serde`, `schemars`, `clap`, `inventory`, `anyhow`) so plugins never pin those
directly.

```toml
[dependencies]
plugin-toolkit = { git = "https://github.com/argyle-labs/orca", branch = "main" }

[build-dependencies] # only if you codegen typed clients in build.rs
plugin-toolkit-build = { git = "https://github.com/argyle-labs/orca", branch = "main" }
```

**Sole-consumer rule:** a plugin depends on its own domain/generated client +
`plugin-toolkit`, and nothing else. Anything a *second* plugin would also want
belongs in core, reached over a capability seam. A plugin never names `tokio`,
`reqwest`, `futures`, or `chrono` — see [Toolkit capabilities](toolkit-capabilities.md).

## The serve loop (`main.rs`)

`fn main()` is a short chain on the typed `Plugin` builder terminated by
`.serve()` — which connects `$ORCA_PLUGIN_SOCKET`, does the `Hello`/`Welcome`
major-check, and serves `Invoke → dispatch → Result` until `Shutdown`. Don't
hand-write the loop. Builder:
[`../../projects/plugin-toolkit/src/plugin.rs`](../../projects/plugin-toolkit/src/plugin.rs).

```rust
// Pure tool-surface plugin. `use docker as _;` force-links this plugin's OWN lib
// crate; without it the linker dead-strips the rlib (and with it every
// #[orca_tool] registration). The builder does NOT force-link for you, so this
// `use ... as _;` is required.
plugin_toolkit::instrument::bootstrap!();
use plugin_toolkit::plugin::Plugin;
use docker as _;

fn main() -> plugin_toolkit::anyhow::Result<()> {
    Plugin::named("docker")
        .version(env!("CARGO_PKG_VERSION"))
        .tools(["docker."])
        .serve()
}
```

A plugin that also registers a backend adds the typed facet alongside its tools —
e.g. `.service(..)`, `.secrets(..)`, `.unit(..)`, `.container_runtime(..)`:

```rust
Plugin::named("docker")
    .version(env!("CARGO_PKG_VERSION"))
    .tools(["docker."])
    .schema_json(docker::registration::schema_json())
    .container_runtime(docker::runtime_adapter::DockerAdapter::new())
    .unit(docker::registration::unit_provider())
    .serve()
```

Typed backends each have a facet method — `.service(..)`, `.storage(..)`,
`.replication(..)`, etc. (see [Backend plugins](backends.md)). Backup-kind /
backup-target backends have no facet yet and use the generic
`.backend(def, dispatcher)` escape hatch. `.serve()` calls
`plugin_toolkit::serve::serve(PluginSpec { .. })` with `version` from
`CARGO_PKG_VERSION`.

Worked examples: `argyle-labs/jellyfin` (pure tool, force-link),
`argyle-labs/nut` (backend facets), `argyle-labs/docker` (tools + unit provider).

## Zero-tool guard

`ORCA_PLUGIN_DUMP_MANIFEST=1 <binary>` prints the derived tool manifest as JSON
and exits. Release CI asserts it is **non-empty** — the guard against a linker
dead-strip shipping a plugin with zero tools. A pure tool plugin must keep its
force-link `use <crate> as _;` for the same reason.
