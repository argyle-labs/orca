# Modules and Crates

Rust code is organized into modules (within a file or across files) and crates (compilation units). A workspace is a collection of crates that share a lock file. Understanding this hierarchy explains why orca is split into 32 crates, what `pub use` does, and how `build.rs` generates code before compilation.

---

## The Workspace

The root `Cargo.toml` defines the workspace:

```toml
# Cargo.toml:132
[workspace]
members = [
    "projects/app-kit",
    "projects/db",
    "projects/server",
    "projects/contract",
    "projects/dispatch",
    "projects/derive",
    "projects/macro-runtime",
    "projects/plugin-abi",
    "projects/plugin-loader",
    "projects/auth",
    "projects/dev",
    "projects/inventory-tests",
    "projects/runtime",
    "projects/containers",
    "projects/conversation",
    "projects/files",
    "projects/model",
    "projects/namespace",
    "projects/notifications",
    "projects/storage",
    "projects/service",
    "projects/deploy-target",
    "projects/orca-inventory",
    "projects/spec",
    "projects/pod",
    "projects/system",
    "projects/utils",
    "projects/database",
    "projects/graphql",
    "projects/openapi",
    "projects/plugin-toolkit",
    "projects/plugin-toolkit-build",
]
resolver = "2"
```

Each entry is a path to a crate directory. Each crate has its own `Cargo.toml` with its own name and dependencies. There is one shared `Cargo.lock` at the workspace root — all crates agree on the same dependency versions.

Running `cargo build` from the workspace root builds all member crates. Running `cargo run` (or `cargo run -- serve`) from the workspace root runs the `orca` binary from `projects/server/`.

---

## `lib.rs` vs `main.rs`

A crate can be either a library (others can import it) or a binary (can be run). The difference:

- **Library crate:** has `src/lib.rs` as the root. Other crates can add it as a dependency.
- **Binary crate:** has `src/main.rs` as the root. Can be run but not imported.

Most crates in orca are libraries: `contract`, `utils`, `db`, `model`, `pod`, `system`, `conversation`, and the other domain crates. They all have `src/lib.rs`.

`projects/server/` has both: a library (`src/lib.rs`, lib name `orca`) holding the `mcp` and `serve` modules, and the binary entry point (`src/main.rs`) that produces the `orca` executable and imports all the domain crates.

---

## `mod`, `pub`, and `use`

### Declaring Modules

Inside a file, you declare a submodule with `mod`:

```rust
// projects/server/src/mcp/mod.rs
mod tools;
```

This tells Rust to look for `tools.rs` (or `tools/mod.rs`) in the same directory, compile it as the `tools` module, and make it available as `mcp::tools` from outside.

### Visibility

By default, everything in Rust is private — accessible only within the same module and its children. `pub` makes something public:

```rust
// projects/conversation/src/sessions/context.rs:6
pub struct ProjectContext {       // visible to all importers
    pub project: Option<String>,  // fields are also pub
    pub memory_content: Option<String>,
}
```

Without `pub`, `ProjectContext` would only be visible inside `context.rs`.

Visibility rules:
- `pub` — visible everywhere
- `pub(crate)` — visible within this crate only, not to other crates
- `pub(super)` — visible to the parent module
- *(nothing)* — private: visible only within this module and its children

### `use` for Imports

`use` brings names into scope:

```rust
// projects/server/src/main.rs:1
use ::model::{ClaudeBackend, Message, ModelBackend, stdout_sink};
use anyhow::{Context, Result};
use conversation::sessions::context::ProjectContext;
```

Without `use`, you would have to write the full path every time: `anyhow::Result`, `model::ModelBackend`.

---

## `pub use`: Re-exports

`pub use` re-exports an item, making it accessible at the current module's path:

```rust
// projects/model/src/lib.rs:26
pub use backend::{
    ClaudeBackend, LMStudioBackend, ModelBackend, OllamaBackend, OutputSink, buffer_sink,
    build_backend, sink_write, sink_writeln, stdout_sink,
};
pub use resolve::{estimate_context_window, resolve_model};
pub use types::{BackendResponse, Message, StopReason};
```

Without `pub use`, callers would have to write `model::backend::ClaudeBackend`. With it, they write `model::ClaudeBackend`. The internal module structure is hidden; the public API is clean. This is why `main.rs` can write `use ::model::{ClaudeBackend, Message, ModelBackend, stdout_sink};` — those names are all re-exported at the crate root.

---

## Module Hierarchy in the Server Crate

`projects/server/src/` has this structure:

```
main.rs         ← binary entry point (clap CLI + dispatch)
lib.rs          ← library root (lib name "orca"); declares pub mod mcp / serve / spec_detail
mcp/
  mod.rs        ← MCP stdio server (JSON-RPC protocol layer)
  tools.rs      ← federation tool defs
serve/
  mod.rs        ← axum HTTP/HTTPS server
  auth_routes.rs
  middleware.rs
  openapi.rs
  pdf_gen.rs
```

The server crate has both a `lib.rs` and a `main.rs`: the library (imported as `orca`) exposes `mcp` and `serve` to the binary and to integration tests, while `main.rs` is only the executable entry point. Actual tool logic lives in the domain crates (`pod`, `system`, `auth`, …) as `#[orca_tool]` functions — the server contains no business logic.

---

## How `build.rs` Generates Code

Cargo runs `build.rs` (if it exists) before compiling the crate. The build script can emit environment variables, generated source, or preconditions for the build.

The server crate uses one to bake the runtime version from git:

```rust
// projects/server/build.rs
fn main() {
    // on a clean tag → "0.0.3-rc.3"; N commits past → "…-dev+5.g66d2ea6"
    let version = resolve_version();
    println!("cargo:rustc-env=ORCA_VERSION={version}");
    // ...
}
```

The binary then reads it at compile time with `env!("ORCA_VERSION")`. Another example: `projects/plugin-toolkit-build` is a whole crate of build-script helpers that codegen typed OpenAPI/GraphQL clients for plugin repos.

(Historical note: agent `.md` prompts used to be embedded via a build.rs `include_str!` codegen. That's gone — agents are now contributed at runtime by plugins registering an `AgentProvider` in `contract::agents`; with no agents plugin loaded the roster is empty.)

---

## `rust-embed`: An Easier Way for Whole Directories

The `files` crate uses `rust-embed` to embed the repo's `docs/` tree:

```rust
// projects/files/src/embedded.rs:10
#[derive(rust_embed::RustEmbed)]
#[folder = "../../docs"]
struct OrcaDocs;
```

`#[derive(rust_embed::RustEmbed)]` with a `#[folder = …]` compiles every file in that directory into the binary. `OrcaDocs::get("path/to/file.md")` retrieves the bytes at runtime. `contract` does the same for `config-docs/`.

This is the pattern for embedding the frontend too:

```rust
// projects/server/src/serve/mod.rs:953
#[cfg(feature = "ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../frontend/dist/"]
struct Assets;
```

All files from the Vite build output are embedded in the server binary.

---

## Feature Flags

Orca does not currently use feature flags heavily, but they are worth knowing. In `Cargo.toml`:

```toml
[features]
default = ["full"]
full = ["dep:some-optional-crate"]
```

Feature flags let you conditionally compile parts of a crate. In code:

```rust
#[cfg(feature = "full")]
pub mod some_module;
```

You will not need to write feature flags for most work on orca, but you will encounter `#[cfg(...)]` for platform-specific code:

```rust
// Compile only on Unix systems
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
```

---

## Importing Between Workspace Crates

Each crate's `Cargo.toml` declares its dependencies. Workspace crates reference each other by path:

```toml
# projects/server/Cargo.toml (excerpt)
[dependencies]
utils    = { path = "../utils" }
dispatch = { path = "../dispatch" }
contract = { path = "../contract" }
db       = { path = "../db" }
auth     = { path = "../auth" }
pod      = { path = "../pod" }
system   = { path = "../system" }
```

Cargo resolves the dependency graph and compiles them in topological order. If you add a new library crate to the workspace, add it to the root `Cargo.toml` members list, and add a `path` dependency in any crate that needs it.

---

## Summary

| Concept | What it means |
|---|---|
| Workspace | Multiple crates, one lock file, shared build |
| `src/lib.rs` | Library crate root — importable by others |
| `src/main.rs` | Binary crate root — executable, not importable |
| `mod name;` | Declare a module; look for `name.rs` or `name/mod.rs` |
| `pub` | Make this item visible outside the module |
| `pub(crate)` | Visible within this crate only |
| `use path::Name;` | Bring a name into scope |
| `pub use path::Name;` | Re-export: expose it at the current module path |
| `build.rs` | Code run before compilation; can generate `.rs` files |
| `include!(...)` | Paste a generated file inline at compile time |
| `rust-embed` | Embed entire directories into the binary |
| `#[cfg(feature = "x")]` | Conditional compilation by feature flag |
