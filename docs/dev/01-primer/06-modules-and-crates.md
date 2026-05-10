# Modules and Crates

Rust code is organized into modules (within a file or across files) and crates (compilation units). A workspace is a collection of crates that share a lock file. Understanding this hierarchy explains why orca is split into eight crates, what `pub use` does, and how `build.rs` generates code before compilation.

---

## The Workspace

The root `Cargo.toml` defines the workspace:

```toml
# Cargo.toml:1
[workspace]
members = [
    "projects/agents",
    "projects/commands",
    "projects/core",
    "projects/docs",
    "projects/jobs",
    "projects/scanner",
    "projects/server",
    "projects/utils",
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

Most crates in orca are libraries: `orca_agents`, `orca_commands`, `orca_core`, `orca_docs`, `orca_utils`. They all have `src/lib.rs`.

`projects/server/` is a binary crate with `src/main.rs`. It imports all the library crates.

A crate can have both (a library and multiple binaries), but orca keeps it simple: one binary (`orca`) and several libraries it imports.

---

## `mod`, `pub`, and `use`

### Declaring Modules

Inside a file, you declare a submodule with `mod`:

```rust
// projects/server/src/mcp/mod.rs:5
mod context7;
mod docs;
mod handlers;
mod specs;
mod tools;
```

This tells Rust to look for `context7.rs` (or `context7/mod.rs`) in the same directory, compile it as the `context7` module, and make it available as `mcp::context7` from outside.

### Visibility

By default, everything in Rust is private — accessible only within the same module and its children. `pub` makes something public:

```rust
// projects/server/src/context.rs:6
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
use anyhow::Result;
use orca::context::ProjectContext;
use orca_core::backend::{ClaudeBackend, ModelBackend, stdout_sink};
```

Without `use`, you would have to write the full path every time: `anyhow::Result`, `orca_core::backend::ModelBackend`.

---

## `pub use`: Re-exports

`pub use` re-exports an item, making it accessible at the current module's path:

```rust
// projects/commands/src/lib.rs:51
pub use spec::{SpecAction, cmd_spec};
pub use log_cmd::{LogAction, cmd_log};
pub use auth::{cmd_login, cmd_logout, cmd_auth};
pub use agents::cmd_agents;
```

Without `pub use`, callers would have to write `orca_commands::auth::cmd_login`. With it, they write `orca_commands::cmd_login`. The internal module structure is hidden; the public API is clean.

In `main.rs`:

```rust
// projects/server/src/main.rs:7
use orca_commands::{self as cmd, CredsAction, DaemonAction, DbAction, ...};
```

`self as cmd` imports the crate itself as `cmd` — so `cmd::cmd_agents()` calls `orca_commands::cmd_agents()`. This works because `lib.rs` re-exports `cmd_agents` at the crate root.

---

## Module Hierarchy in the Server Crate

`projects/server/src/` has this structure:

```
main.rs         ← crate root (binary entry point)
context.rs      ← mod context, declared in main.rs as: use orca::context::ProjectContext
session.rs
mcp/
  mod.rs        ← mcp module root
  handlers.rs
  docs.rs
  specs.rs
serve/
  mod.rs        ← serve module root
  api/
    mod.rs      ← serve::api module root
    health.rs
    mcp.rs
    ...
```

`main.rs` is the crate root but since this is in a binary crate (`main.rs` not `lib.rs`), external crates cannot import from it directly. For the server crate to expose things to tests or integration code, it would need a `lib.rs` too — but orca's server crate is binary-only.

---

## How `build.rs` Generates Code

Cargo runs `build.rs` (if it exists) before compiling the crate. The build script can generate Rust source files that are then `include!`d into the crate.

In `orca_agents`:

```rust
// projects/agents/build.rs
fn main() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = Path::new(&out_dir).join("embedded_agents.rs");

    let mut code = String::from("pub fn embedded_agent(name: &str) -> Option<&'static str> {\n");
    code.push_str("    match name {\n");

    // For each .md file in src/agents/:
    for entry in entries {
        let name = ...;
        let abs = path.canonicalize()?;
        code.push_str(&format!(
            "        \"{name}\" => Some(include_str!(\"{}\")),\n",
            abs.display()
        ));
    }
    code.push_str("        _ => None,\n");
    code.push_str("    }\n}\n");

    fs::write(&dest, code)?;
}
```

This generates a file like:
```rust
pub fn embedded_agent(name: &str) -> Option<&'static str> {
    match name {
        "wolf"  => Some(include_str!("/path/to/wolf.md")),
        "bear"  => Some(include_str!("/path/to/bear.md")),
        // ...
        _ => None,
    }
}
```

Then in `lib.rs`:

```rust
// projects/agents/src/lib.rs:9
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));
```

`include!` pastes the generated file's contents inline. `env!("OUT_DIR")` expands to the build directory at compile time. The result: agent `.md` files are compiled into the binary as static strings. No filesystem access needed at runtime.

`orca_commands` uses the same pattern for slash command prompts:

```rust
// projects/commands/src/lib.rs:22
include!(concat!(env!("OUT_DIR"), "/embedded_commands.rs"));
```

---

## `rust-embed`: An Easier Way for Whole Directories

The `orca_docs` crate uses `rust-embed` instead of a custom build script:

```rust
// docs/lib.rs:6
#[derive(rust_embed::RustEmbed)]
#[folder = "src"]
struct OrcaDocs;
```

`#[derive(rust_embed::RustEmbed)]` with `#[folder = "src"]` compiles every file in the `src/` directory into the binary. `OrcaDocs::get("path/to/file.md")` retrieves the bytes at runtime.

This is the pattern for embedding the frontend too:

```rust
// projects/server/src/serve/mod.rs:255
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
# projects/server/Cargo.toml (approximately)
[dependencies]
orca_core     = { path = "../core" }
orca_agents   = { path = "../agents" }
orca_commands = { path = "../commands" }
orca_docs     = { path = "../docs" }
orca_utils    = { path = "../utils" }
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
