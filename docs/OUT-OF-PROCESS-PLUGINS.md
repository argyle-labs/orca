# Out-of-process, capability-delegated plugins

> Status: **Design record.** The rationale behind the subprocess plugin
> architecture. For the runtime mechanism (wire frames, handshake, loader
> lifecycle) see [`plugin-loading.md`](plugin-loading.md).

## Design goals

A plugin always runs under the orca daemon, so it links **almost nothing** and
delegates every heavy capability back to the daemon over a socket. Running each
plugin as its own subprocess buys four properties directly:

1. **Crash isolation.** A plugin fault takes down only that child process. The
   supervisor logs the fault and respawns; the daemon keeps running.
2. **libc independence.** A plugin process talks to the daemon over JSON on a
   socket, so a plugin binary loads next to any daemon regardless of glibc/musl
   — builds reduce to arch, or a single portable binary.
3. **Version stability.** Compatibility is a wire-protocol semver checked at the
   handshake, so a plugin binary keeps working across daemon upgrades as long as
   the protocol major matches.
4. **Small footprint.** The heavy async/TLS/HTTP/DB stack lives once in the
   daemon; a plugin that delegates its I/O carries only its own logic, generated
   types, and serde. (See *Thin by architecture*, below, for how far this
   reaches today.)

## Model

```
        ┌─────────────────────── orca daemon ───────────────────────┐
        │  plugin supervisor    capability host    tool registry     │
        └───────▲───────────────────▲──────────────────┬────────────┘
                │ spawn / health     │ cap requests     │ tool invoke
                │                     │ (plugin→orca)    │ (orca→plugin)
        ┌───────┴─────────────────────────────────────▼────────────┐
        │  plugin process (thin)   UDS  ⇄  length-prefixed JSON     │
        │  logic + generated types + serde. NO tokio/rustls/reqwest │
        └───────────────────────────────────────────────────────────┘
```

- orca **spawns** each installed plugin as a child process and connects a
  per-plugin **Unix domain socket** (abstract namespace on Linux, temp path on
  macOS). One socket, bidirectional, one plugin per process.
- The plugin performs **no direct I/O**. HTTP, TLS, DB, secrets, transport
  (Socket.IO/WS), and logging are **host capabilities**: the plugin sends a
  capability request over the socket, orca executes it with its single copy of
  the runtime, and returns the result.
- The daemon calls a plugin's **tools** over the same socket (orca→plugin);
  the plugin calls **capabilities** over it (plugin→orca). Both are just
  messages, multiplexed by direction + id.

## Wire protocol

Framing: `u32` little-endian length prefix + a JSON object. JSON keeps the
transport debuggable, and `ToolDef`/`BackendDef`/args/results all travel as JSON
strings. (MessagePack is a drop-in later optimization.)

Every frame is a variant of the `Frame` enum in
[`projects/plugin-proto/src/lib.rs`](../projects/plugin-proto/src/lib.rs)
(`#[serde(tag = "kind", rename_all = "snake_case")]`, so a Rust variant
`Invoke` is `"kind": "invoke"` on the wire). orca → plugin carries `Invoke`,
`CapResult`, `CapStreamChunk`, `CapStreamEnd`, `Welcome`, and `Shutdown`;
plugin → orca carries `Hello`, `Result`, `Cap`, and `Log`. `id` correlates
request/response in each direction independently, and `Invoke` and `Cap` can be
in flight concurrently (the daemon and plugin are both async); ids are
per-direction monotonic. The frame catalog and per-frame semantics are
tabulated in [`plugin-loading.md`](plugin-loading.md).

### Handshake

On connect the plugin sends a `Hello` (its protocol semver, plugin name and
version, tool `manifest`, `backends`, and declared SQL `schema`); orca replies
`Welcome` with the host capability list, or refuses on a **protocol**
major-version mismatch. Compatibility is a wire-protocol semver negotiated at
runtime: a plugin connects to any daemon whose protocol major matches its own,
independent of the daemon's build or libc.

## Host capability surface

The reverse-direction `cap` messages. The set the loader serves is the
`CAPABILITIES` const in
[`projects/plugin-loader/src/capability.rs`](../projects/plugin-loader/src/capability.rs)
(`db.op`, `secret.op`, `http.request`, `http.stream`, `agents.register`):

| cap | args → result | serves |
|-----|---------------|--------|
| `http.request` | buffered `{method,url,headers,body}` → `{status,headers,body}` | HTTP+TLS from the daemon's single reqwest/rustls stack |
| `http.stream` | streaming response body, delivered as cap stream-frames (`ByteStream`/`EventStream`) | streamed bodies without buffering host-side |
| `db.op` | typed CRUD (the `DbOp` `List`/`Get`/`Upsert`/`Delete` surface; core tables via the empty-namespace convention) | the plugin's namespaced tables and the fixed core tables |
| `secret.op` | secret backend op | reads/writes against the secret backend |
| `agents.register` | contribute an `AgentRegistration` into the core agents domain | domain registration over the cap channel |

The plugin HTTP surface is `plugin_toolkit::client`, exposing the orca-owned
`Request`/`Response`/`Stream` types as the boundary (*re-export is not
abstraction* — a plugin never names reqwest or `futures_util`). Transport and
log caps are tracked as future additions.

`db.op` is the same seam established for DB ([[plugin-db-through-core-design]]),
generalized to every heavy capability and carried on the socket.

## Plugin runtime harness (`plugin-toolkit`)

Authoring is declarative. A plugin declares its tools with `#[orca_tool]` and
its backends with the backend declarations, and ships as an `rlib` + a `[[bin]]`
whose `fn main()` is emitted by a `serve_*_plugin!` macro
(`projects/plugin-toolkit/src/serve_macros.rs`):

```rust
// Emits a whole `fn main()` that connects `$ORCA_PLUGIN_SOCKET`, handshakes,
// and serves Invoke → dispatch → Result until Shutdown. `link:` names the
// plugin's own lib crate so its #[orca_tool] registrations aren't dead-stripped.
plugin_toolkit::serve_tool_plugin! { name: "jellyfin", target_compat: "", link: jellyfin }
// service/storage/backup backends use serve_service_plugin! / serve_storage_plugin! / etc.
```

The macro arms and the required `link:`/`backends:` fields are documented in
[`plugin-authoring/native-plugin.md`](plugin-authoring/native-plugin.md).

Under the hood the macro calls `plugin_toolkit::serve::serve(PluginSpec { .. })`,
which owns: socket connect, handshake, decode `Invoke` frames, call the generated
dispatch fn, encode `Result`. The HTTP client seam (`plugin_toolkit::client`) and
the DB/secret accessors emit `cap` frames and await the reply, so the plugin
links **none** of reqwest/rustls/hyper. The plugin drives its tool futures on the
shared orca-owned reactor (`plugin_toolkit::reactor`).

## Loader supervisor

`plugin-loader` runs a **supervisor** over each plugin process:

- `install`: catalog/`--name` fetch keyed by arch (a delegating plugin needs no
  libc split, and a single portable build may suffice), written to the install
  dir.
- `load`: spawn the process, connect the socket, complete the handshake,
  register the manifest's tools + backends into the live registry.
- `health`: missed heartbeats / socket close → restart with backoff. A crash is
  isolated: the daemon logs it and respawns; **orca stays up when a plugin dies.**
- `unload`: send `shutdown`, SIGTERM after a grace period.

## The web UI is a plugin too

The web UI is an out-of-process plugin under this model: **peacock**
(repo [argyle-labs/peacock](https://github.com/argyle-labs/peacock)) registers
`contract::web`, owns the root route `/`, and renders via its `peacock.render`
tool (or a Vite `dev_upstream` in dev); orca core proxies `/` to the peacock
process.

## Thinness is a requirement

Delegating capabilities keeps a plugin carrying **only** its own logic +
generated types + serde. Every slice enforces this as it lands:

- **Delegate, never bundle.** HTTP/TLS, DB, secrets, transport, and logging are
  host capabilities. A plugin that only does DB/secret/logic links no
  `reqwest`/`rustls`/`tokio-net` at all.
- **Minimal features by default.** Plugins build the toolkit with
  `default-features = false` and opt into only what they use; a plugin never
  pulls the `full` profile for capabilities it delegates.
- **Measured + budgeted in CI.** The release workflow reports every artifact's
  size and warns over a size budget (`PLUGIN_SIZE_BUDGET_MIB`), so bloat is
  visible per-build. The budget ratchets down as plugins shed bundled deps.
- **Tracked to completion.** The `reqwest`-shedding effort (progenitor clients
  still link `reqwest`) is an open, tracked step toward the thin profile.

## Thin by architecture: everything heavy lives in core

> Status: **Snapshot** — ongoing thinning work, phased and measured in CI.

Crash isolation, libc independence, and wire-protocol version stability hold the
moment a plugin becomes a subprocess. Size is a separate axis: a subprocess bin
that still statically links the whole `reqwest`/`rustls`/`hyper`/`tokio` stack is
as large as the code it links (proxmox measures ~37 MiB stripped on darwin).

Size falls when the heavy code **moves into core** and the plugin reaches
it through the orca runtime. The governing rule: a plugin links *almost nothing*
at runtime — everything expensive is a host capability or a build-time artifact.

**End-state plugin links ONLY:** `serde` + `serde_json`, `plugin-proto`
(serde-only), a thin `plugin-toolkit` serve harness + capability shims, its own
generated **types** (structs — not clients), and its logic. It does **not** link
`reqwest`/`rustls`/`hyper`, `tokio` (full), `schemars`/`clap`/`axum` at runtime,
progenitor's reqwest client, or `rusqlite`.

Today's bloat sources, and where each goes — phased, each step measured against
the CI size budget:

| Bloat in the plugin today | Moves to core as | Phase |
|---|---|---|
| `reqwest`/`rustls`/`hyper` (HTTP+TLS) — the bulk | `http.request` capability; `plugin_toolkit::http` becomes a cap-backed shim | **A** ✅ (#29) |
| progenitor client hardwired to `reqwest::Client` | `plugin_toolkit_build` retargets the generated client onto the cap-backed http client (or a reqwest-API-shaped shim) — typed clients keep working, link no reqwest | **B** (hard) ✅ (#30, #33) |
| `tokio` (full) + serve's tokio runtime | micro-executor (`futures::executor::block_on`) — all I/O is synchronous cap round-trips, so no reactor is needed; `tokio` → in-process-only feature | **C** ✅ (#45) |
| `schemars` (tool/arg schemas) | bake manifest/backends/schema JSON as **build-time** string consts; `schemars` → build-dependency, not a runtime link | **D** ⏸ (deferred) |
| `dispatch` pulling `axum`/`reqwest`; `clap` arg parsing | split dispatch so plugins link only a registry+invoke core; `clap` → in-process-only | **E** |
| `rust_socketio`/`native-tls` (dockge) | `transport.open`/`send`/`recv` capability | **F** |

Phase A (HTTP capability) is the highest-leverage — it removes the largest
single chunk and unblocks measuring the rest. Phase B (progenitor) is the
hardest: the generated client's `reqwest` coupling is why `reqwest` can't simply
be dropped from a feature list. Everything after B is incremental subtraction.

Phase D is **deferred**: the only runtime `schemars` entry on the thin path is
the tool-manifest `schema_for!` in `dispatch::erased`, and baking it at build
time is trivial for descriptor/codegen plugins (proxmox — schemas are already
spec-derived data in `plugin-toolkit-build`) but forks for hand-written
`#[orca_tool]` plugins, since a plugin's `build.rs` can't introspect its own
not-yet-compiled tool types. The two resolutions (build-side type replica vs.
committed manifest artifact + drift gate) should be chosen against a *real*
hand-written thin plugin's needs — and none exist in-tree yet (docker/dockge
are unported, peacock lives in its own repo, `agents` is in-process). Revisit D
when the first hand-written thin plugin is ported.

Phase C (#45) was wider than this row implies: `tokio` reached the thin profile
transitively through the domain crates' `dispatch_op` seams
(`spawn_blocking` / `tokio::process::Command`), not just `serve.rs`. Gating it
out spanned six crates — `plugin-toolkit` plus `contract`, `dispatch`,
`service`, `deploy-target`, `storage` — each `dispatch_op` now driving the
backend future on `futures::executor::block_on` on the thin profile.
