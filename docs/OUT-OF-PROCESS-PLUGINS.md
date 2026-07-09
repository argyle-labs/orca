# Out-of-process, capability-delegated plugins

Status: **design** (decided 2026-07-08). Supersedes the in-process `abi_stable`
cdylib model for plugins.

## Why

A plugin today is a cdylib `dlopen`'d into the daemon. That one fact creates
four problems, all rooted in *the plugin bundling and re-implementing what orca
already has*:

1. **Size.** The cdylib statically links the whole async/TLS/HTTP stack
   (`tokio`/`rustls`/`hyper`/`reqwest`) plus its generated client. proxmox =
   46 MB stripped, ~95 % bundled deps, re-resident per loaded plugin.
2. **ABI-version coupling.** `abi_stable` bakes the `plugin-abi` crate version
   into the layout/RootModule tag; every orca minor bump invalidated every
   plugin binary (patched interim by pinning `plugin-abi` to `0.1.0`).
3. **libc coupling.** A glibc `.so` can't load into a musl daemon; we shipped a
   gnu+musl build matrix and tried (and failed — static-pie) to make the musl
   daemon dynamic.
4. **No crash isolation (the decider).** A plugin fault SIGSEGVs the whole
   daemon. Observed 2026-07-08: proxmox loaded cleanly, then crash-looped a PVE
   daemon ~20 s in during an FFI call.

Plugins always run under orca. So a plugin should link **almost nothing** and
delegate every heavy capability back to the daemon. That collapses all four
problems at once.

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

Framing: `u32` little-endian length prefix + a JSON object. (MessagePack is a
drop-in later optimization; JSON first for debuggability — and the current FFI
already passes `ToolDef`/`BackendDef`/args/results as JSON strings, so the
payloads are unchanged.)

Every frame is one of:

```jsonc
// orca → plugin
{ "id": 42, "kind": "invoke", "tool": "proxmox.get_facts", "args": { ... } }
{ "id": 7,  "kind": "cap_result", "ok": true, "value": { ... } }   // reply to a plugin cap request

// plugin → orca
{ "id": 42, "kind": "result", "ok": true, "value": { ... } }        // reply to an invoke
{ "id": 99, "kind": "cap", "cap": "http.request", "args": { ... } } // capability call
{ "kind": "log", "level": "info", "msg": "...", "fields": { ... } } // fire-and-forget
```

`id` correlates request/response in each direction independently. `invoke` and
`cap` can be in flight concurrently (the daemon and plugin are both async);
ids are per-direction monotonic.

### Handshake (replaces the `abi_stable` gate)

On connect the plugin sends:

```jsonc
{ "kind": "hello",
  "protocol": "1.0",              // semver of THIS wire protocol
  "plugin": "proxmox", "version": "0.1.1-rc.3",
  "manifest": [ /* ToolDef[] — unchanged JSON shape */ ],
  "backends": [ /* BackendDef[] */ ],
  "schema":   { /* declared SQL, unchanged */ } }
```

orca replies `{ "kind": "welcome", "protocol": "1.0", "capabilities": [...] }`
or refuses on a **protocol** major-version mismatch. Compatibility is now a
*wire-protocol semver* negotiated at runtime — not a compiled layout tag. A
plugin built years ago still connects as long as the protocol major matches.
No `plugin-abi`, no per-libc gate.

## Host capability surface

The reverse-direction `cap` messages. Start with what plugins actually need
(and what the FFI already exposes for DB):

| cap | args → result | replaces |
|-----|---------------|----------|
| `http.request` | `{method,url,headers,body}` → `{status,headers,body}` | plugin's own reqwest/rustls |
| `db.op` | typed CRUD (the existing `DbOp`) | current `set_host`/`db_op` FFI |
| `secret.get` | `{name}` → value | current `set_secret_op`/`secret_op` |
| `transport.open`/`send`/`recv` | Socket.IO/WS client ops | [[orca-core-provides-transports-socketio]] |
| `log` | `{level,msg,fields}` | plugin `tracing` (never reached daemon.log anyway) |

This is the same seam already established for DB ([[plugin-db-through-core-design]])
— generalized to every heavy capability and moved onto the socket.

## Plugin runtime harness (`plugin-toolkit`)

Authoring stays declarative. `#[orca_tool]` and the backend declarations are
unchanged; what changes is the entrypoint. Instead of exporting a cdylib
`PluginMod`, the plugin compiles to a **small binary** whose `main()` runs a
toolkit-provided loop:

```rust
fn main() -> anyhow::Result<()> {
    plugin_toolkit::serve(orca_manifest!())   // connect UDS, handshake, dispatch
}
```

`serve()` owns: socket connect, handshake, decode `invoke` frames, call the
generated dispatch fn, encode `result`. Capability helpers (`toolkit::http`,
`toolkit::db`, `toolkit::secrets`) become thin shims that emit `cap` frames and
await `cap_result` — so the plugin links **none** of reqwest/rustls/hyper. The
plugin's own runtime is a minimal single-thread executor (or blocking loop);
tokio-full is gone.

## Loader changes

`plugin-loader` stops `dlopen`ing. It gains a **supervisor**:

- `install`: unchanged catalog/`--name` fetch (per triple → now just arch, no
  libc split needed since the plugin delegates I/O; a single portable build may
  even suffice), write to the install dir.
- `load`: spawn the process, connect the socket, complete the handshake,
  register the manifest's tools + backends into the live registry.
- `health`: missed heartbeats / socket close → restart with backoff. A crash is
  isolated: the daemon logs it and respawns; **orca never dies with the plugin.**
- `unload`: send `shutdown`, SIGTERM after a grace period.

## Migration

Both models coexist during the transition (loader detects cdylib vs executable
by file type):

1. Land the protocol crate + toolkit `serve()` + capability host in orca.
2. Port **proxmox** first (it's what crashed) as the proof: same tools, now a
   subprocess. Validate topology cluster-grouping end-to-end — the goal that's
   been blocked.
3. Port docker/dockge, then the rest.
4. Retire `plugin-abi`/`abi_stable`, the gnu/musl build matrix, and the
   musl-dynamic daemon hack — all obsolete once nothing is `dlopen`'d.

The web UI is itself an out-of-process plugin under this model: **peacock**
(repo [argyle-labs/peacock](https://github.com/argyle-labs/peacock)) registers
`contract::web`, owns the root route `/`, and renders via its `peacock.render`
tool (or a Vite `dev_upstream` in dev) — orca core proxies `/` to it rather than
embedding a SvelteKit build.

## Thinness is a requirement, not a nice-to-have

The whole point of delegating capabilities is that a plugin carries **only** its
own logic + generated types + serde. This is enforced as part of the process,
every slice — not left as a cleanup:

- **Delegate, never bundle.** HTTP/TLS, DB, secrets, transport, and logging are
  host capabilities. A plugin that only does DB/secret/logic links no
  `reqwest`/`rustls`/`tokio-net` at all.
- **Minimal features by default.** Plugins build the toolkit with
  `default-features = false` and opt into only what they use; a plugin never
  pulls the `full` profile for capabilities it delegates.
- **Measured + budgeted in CI.** The release workflow reports every artifact's
  size and warns over a size budget (`PLUGIN_SIZE_BUDGET_MIB`), so bloat is
  visible per-build. The budget ratchets down as plugins shed bundled deps.
- The `reqwest`-shedding effort (progenitor clients still link `reqwest`) is part
  of *reaching* thin — tracked and pursued, not parked.

## Thin by architecture: everything heavy lives in core

**The subprocess pivot alone does not shrink a plugin.** Measured, proxmox as a
subprocess bin is ~1.8 MiB *larger* than its cdylib (37.2 vs 35.4 MiB stripped,
darwin) — it still statically links the whole `reqwest`/`rustls`/`hyper`/`tokio`
stack *and* adds a serve loop. Crash isolation, libc independence, and the death
of ABI-version coupling are real wins; **size is not, yet.**

Size only falls when the heavy code **moves into core** and the plugin reaches
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
| `reqwest`/`rustls`/`hyper` (HTTP+TLS) — the bulk | `http.request` capability; `plugin_toolkit::http` becomes a cap-backed shim | **A** |
| progenitor client hardwired to `reqwest::Client` | `plugin_toolkit_build` retargets the generated client onto the cap-backed http client (or a reqwest-API-shaped shim) — typed clients keep working, link no reqwest | **B** (hard) |
| `tokio` (full) + serve's tokio runtime | micro-executor (`futures::executor::block_on`) — all I/O is synchronous cap round-trips, so no reactor is needed; `tokio` → in-process-only feature | **C** |
| `schemars` (tool/arg schemas) | bake manifest/backends/schema JSON as **build-time** string consts; `schemars` → build-dependency, not a runtime link | **D** |
| `dispatch` pulling `axum`/`reqwest`; `clap` arg parsing | split dispatch so plugins link only a registry+invoke core; `clap` → in-process-only | **E** |
| `rust_socketio`/`native-tls` (dockge) | `transport.open`/`send`/`recv` capability | **F** |

Phase A (HTTP capability) is the highest-leverage — it removes the largest
single chunk and unblocks measuring the rest. Phase B (progenitor) is the
hardest: the generated client's `reqwest` coupling is why `reqwest` can't simply
be dropped from a feature list. Everything after B is incremental subtraction.

## What this obsoletes

- `plugin-abi` version pinning ([[plugin-dylib-gotchas]]) — replaced by wire
  protocol semver.
- gnu/musl build matrix + `-crt-static` musl-daemon work — a delegating plugin
  is libc-independent; builds reduce to arch (or one portable binary).
- Per-plugin 40 MB runtime duplication — one runtime in the daemon.
- Daemon-fatal plugin crashes — isolated to the child process.
