# Dynamic plugin loading — the subprocess model

orca is one binary, but its capabilities are not baked in. Every plugin is
an **independently built `argyle-labs` repo** that orca loads at runtime —
no recompile of orca to add, update, or remove one. This is the mechanism
behind the platform rule: core holds only abstractions and registries; every
concrete capability is an external plugin (`docs/CAPABILITY-REGISTRIES.md`).

**The model is subprocess-only.** A plugin is a child process of the orca
daemon, connected over a Unix-domain socket, speaking the `plugin-proto` wire
protocol. There is no in-process linking: plugins are crash-isolated,
libc/ABI-independent, and free to be written in any language that can speak
the protocol.

> The former in-process `cdylib` / `abi_stable` model is **removed**. The
> compiled layout/version gate is replaced by protocol-version negotiation
> (below). There is no `dlopen`, no `PluginMod`, no shared Rust type across
> the boundary. Do not build cdylib plugins.

---

## 1. The wire protocol (`plugin-proto`)

One Unix-domain socket carries both directions, length-prefixed JSON frames
(`projects/plugin-proto/src/lib.rs`). Tool args/results and capability
payloads travel as free-form JSON — the transport-dynamic boundary; per-tool
typing is validated *above* this layer against each tool's declared JSON
Schema.

| Frame | Direction | Meaning |
|---|---|---|
| `Hello` | plugin → orca | first frame: protocol version, name, plugin version, `manifest` (`ToolDef[]`), `backends`, optional SQL `schema` |
| `Welcome` | orca → plugin | accepts handshake; lists host `capabilities` |
| `Invoke` | orca → plugin | run tool `{id, tool, args}` |
| `Result` | plugin → orca | answer an `Invoke` `{id, ok, value?, error?}` |
| `Cap` | plugin → orca | call a host capability `{id, cap, args}` |
| `CapResult` | orca → plugin | answer a `Cap` `{id, ok, value?, error?}` |
| `Log` | plugin → orca | structured log line (fire-and-forget) |
| `Shutdown` | orca → plugin | begin graceful shutdown |

`id` correlates request↔response *within each direction* (monotonic per
direction), so a plugin can have in-flight `Cap` calls while servicing an
`Invoke`. Frames are capped at `MAX_FRAME_BYTES` (64 MiB) to guard against a
hostile length prefix.

---

## 2. Compatibility — protocol-major negotiation

The compiled `abi_stable` layout/version gate is gone. Compatibility is a
single runtime check at the handshake (`plugin-proto`):

```rust
pub const PROTOCOL_VERSION: &str = "1.0";
// a plugin and daemon interoperate iff their protocol MAJORs match
pub fn protocol_compatible(a: &str, b: &str) -> bool { /* major(a) == major(b) */ }
```

- A plugin built against protocol `1.x` connects to any daemon on `1.y`.
- Missing/malformed versions **fail closed** (treated as incompatible).
- The plugin also reports its own `version` (semver) and `plugin` name in
  `Hello` for the catalog/diagnostics; those are informational, not gates.

There is no layout hash and no shared Rust type across the boundary — the
whole point of dropping `abi_stable`.

---

## 3. Lifecycle — spawn, handshake, dispatch

Daemon side (`projects/plugin-loader/src/supervisor.rs`):

1. **Spawn** the plugin process and hand it a socket to connect back on.
2. **Handshake** — read `Hello`, check protocol major, reply `Welcome` with
   the host capability list. On mismatch, refuse cleanly and reap the child.
3. **Register** the plugin's `manifest` tools and `backends` into the loader's
   runtime registry; apply its declared SQL `schema` if present.
4. **Dispatch** — route matching tool calls as `Invoke` frames; stream
   `Cap` requests back to the capability handler; forward `Log` frames.
5. **Unload/crash** — reverse every registration so a dead plugin never
   leaves a stale backend or tool pointing at a closed socket. A subprocess
   crash takes down only that plugin.

Plugin side: `plugin-toolkit`'s `serve()` entrypoint
(`projects/plugin-proto/src/session.rs`) sends `Hello`, awaits and
major-checks `Welcome`, then runs the invoke/capability loop. A plugin author
implements tool handlers; the session loop handles framing and correlation.

### One registry, one namespace

orca's built-in tool registry is a frozen `OnceLock<ToolCache>` populated
once from `inventory::iter` at link time — no runtime insertion, by design.
Dynamically loaded plugins live in the loader's own `RwLock` registry, and
`plugin_loader::dispatch` fronts both (runtime first, static fallback) so
callers see one tool namespace regardless of where a tool comes from.

---

## 4. Capability delegation — plugins stay thin

A plugin links no HTTP client, no database, no secret store. It calls **back
into the daemon** via `Cap` frames. The daemon serves a fixed set
(`projects/plugin-loader/src/capability.rs`):

```rust
pub const CAPABILITIES: &[&str] = &["db.op", "secret.op", "http.request"];
```

- `db.op` → `db::plugin_tables::exec_db_op_pooled`
- `secret.op` → `db::secrets::exec_secret_op_pooled`
- `http.request` → pooled `utils::http::Client`

This is what makes plugins **thin by architecture** — no
axum/tower/reqwest/rusqlite in a plugin build, tokio in-process only
(thinness Phases A–C, PRs #29–#45; see `docs/OUT-OF-PROCESS-PLUGINS.md`).
Every heavy dependency lives in core and is reached over the socket, so a
plugin ships as a small binary that speaks JSON.

---

## 5. Authoring

This doc is the *mechanism*. To build a plugin — project layout, `serve()`,
declaring tools, and the `orca-plugin.toml` manifest for third-party MCP
servers — see `docs/plugin-authoring.md`.
