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
    ├── main.rs         ← one serve_*_plugin! macro (the whole fn main)
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

One `serve_*_plugin!` macro emits the whole `fn main()` — connect
`$ORCA_PLUGIN_SOCKET`, `Hello`/`Welcome` major-check, serve
`Invoke → dispatch → Result` until `Shutdown`. Don't hand-write it. Defs:
[`../../projects/plugin-toolkit/src/serve_macros.rs`](../../projects/plugin-toolkit/src/serve_macros.rs).

```rust
// Pure tool-surface plugin. `link:` names this plugin's OWN lib crate; the
// emitted `use <link> as _;` stops the linker dead-stripping the rlib (and with
// it every #[orca_tool] registration). Omitting it is a compile error.
plugin_toolkit::serve_tool_plugin! { name: "docker", target_compat: ">=20.10", link: docker }

// Hybrid tool + registered backend — `backends:` yields the backends JSON,
// `backend_dispatch:` handles the domain's `*.__backend.*` callbacks.
plugin_toolkit::serve_tool_plugin! {
    name: "ntfy", target_compat: "",
    backends: ntfy_backends_json(),
    backend_dispatch: ntfy_backend_dispatch,
}
```

Typed backends have dedicated macros — `serve_service_plugin!`,
`serve_storage_plugin!`, `serve_backup_kind_plugin!`,
`serve_backup_target_plugin!` (see [Backend plugins](backends.md)). Each macro
calls `plugin_toolkit::serve::serve(PluginSpec { .. })` with `version` from
`CARGO_PKG_VERSION`.

Worked examples: `argyle-labs/jellyfin` (pure tool, `link:`),
`argyle-labs/ntfy` (hybrid), `argyle-labs/docker` (unit provider).

## Zero-tool guard

`ORCA_PLUGIN_DUMP_MANIFEST=1 <binary>` prints the derived tool manifest as JSON
and exits. Release CI asserts it is **non-empty** — the guard against a linker
dead-strip shipping a plugin with zero tools. The pure `serve_tool_plugin!` arm
also requires `link:` at compile time.
