# Modules and Crates

> The applied companion is [`docs/learn/rust-primer.md`](../../learn/rust-primer.md).
> Open the linked source alongside this page; the code wins if a snippet drifts.

Rust code is organized into modules (within a file or across files) and crates
(compilation units). A workspace is a collection of crates that share a lock
file. Understanding this hierarchy explains how orca is split into many small
domain crates, what `pub use` does, and how `build.rs` generates code before
compilation.

---

## The workspace

The root `Cargo.toml` defines a `[workspace]` whose `members` list is the
authoritative crate roster. Every member lives under `projects/` with a **flat
package name — no `orca-` prefix** (`server`, `model`, `conversation`, `db`,
`files`, `derive`, `dispatch`, `contract`, …). There are dozens of them, and the
list changes; **do not** try to memorize or duplicate it here. The single source
of truth for *what each crate owns* is
[`CRATE_RESPONSIBILITIES.md`](../../../CRATE_RESPONSIBILITIES.md); the
machine-authoritative member list is `Cargo.toml [workspace.members]`.

The shape that matters for this primer:

- Each entry is a path to a crate directory with its own `Cargo.toml`, name, and
  dependencies.
- There is one shared `Cargo.lock` at the workspace root — all crates agree on
  the same dependency versions.
- `cargo build` from the root builds every member. `cargo run` runs the `orca`
  binary from `projects/server/`.
- Crates are layered `surface → platform → core`; dependencies point *down*.
  Tools live in their owning **domain** crate, not in `server`. `server` is the
  top of the tree and the only crate with a `main.rs`.

---

## `lib.rs` vs `main.rs`

A crate is either a library (importable) or a binary (runnable):

- **Library crate:** root is `src/lib.rs`. Other crates depend on it.
- **Binary crate:** root is `src/main.rs`. Runnable, not importable.

Nearly every orca crate is a library (`model`, `conversation`, `files`, `db`,
`utils`, …), each with a `src/lib.rs`. `projects/server/` is the binary crate:
it has `src/main.rs` (and a `src/lib.rs` for the pieces its modules share) and
depends on the library crates.

---

## `mod`, `pub`, and `use`

### Declaring modules

Inside a file you declare submodules with `mod`. For example
[`projects/model/src/lib.rs`](../../../projects/model/src/lib.rs) opens with
`pub mod backend;`, `pub mod engine;`, `pub mod models;`, and so on — telling
Rust to compile `backend.rs`/`backend/mod.rs` as the `backend` module, reachable
as `model::backend` from outside.

### Visibility

By default everything is private — visible only within its own module and
children. `pub` opens it up:

```rust
// illustrative
pub struct Thing {          // visible to importers
    pub field: Option<String>,
}
```

- `pub` — visible everywhere
- `pub(crate)` — visible within this crate only
- `pub(super)` — visible to the parent module
- *(nothing)* — private to this module and its children

### `use` brings names into scope

`main.rs` imports across crates by their flat names:

```rust
// illustrative — mirrors the real imports in projects/server/src/main.rs
use anyhow::{Context, Result};
use conversation::sessions::session::Session;
use model::{ClaudeBackend, ModelBackend, stdout_sink};
```

Without `use` you would write the full path each time (`model::ModelBackend`).

---

## `pub use`: re-exports

`pub use` re-exports an item at the current module's path, hiding internal
structure behind a clean public API. `model/src/lib.rs` does exactly this —
`pub use backend::{ ... }` lifts backend types to the crate root, so callers
write `model::ModelBackend` instead of `model::backend::ModelBackend`:

```rust
// illustrative
pub use backend::{ModelBackend, OutputSink, build_backend};
```

The `self as cmd` idiom (`use conversation::{self as cmd, ...}`) imports a crate
under a short alias, so `cmd::something()` resolves to a re-export at that
crate's root.

---

## Module hierarchy in a crate

A crate's directory *is* its module tree. The `conversation` crate, for example:

```
projects/conversation/src/
  lib.rs                 ← crate root
  run.rs                 ← background one-shot runs
  sessions/
    mod.rs               ← sessions module root
    context.rs           ← ProjectContext
    session/
      mod.rs             ← Session struct + new/run/run_tui/one_shot
      chat.rs, ui.rs, …
```

A binary crate's `main.rs` is a crate root that external crates cannot import
from directly; that is why `server` also carries a `lib.rs` for anything its own
modules (and tests) need to share.

---

## How `build.rs` generates code

Cargo runs a crate's `build.rs` (if present) before compiling it. The script can
generate Rust source that is then `include!`d into the crate.
[`projects/agents/build.rs`](../../../projects/agents/build.rs) does this: it
writes lookup-table functions to `OUT_DIR`, and
[`projects/agents/src/embedded.rs`](../../../projects/agents/src/embedded.rs)
pulls them in with

```rust
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));
```

`include!` pastes the generated file inline; `env!("OUT_DIR")` expands to the
build directory at compile time. The generated tables are empty by design: the
roster (wolf/otter/… `.md`) lives in the external
[`argyle-labs/agents`](https://github.com/argyle-labs) plugin and registers at
runtime through the plugin seam. The build script keeps the `include!` targets
and machinery types compiling while the roster arrives over that seam.

---

## `rust-embed`: embedding whole directories

For embedding an entire directory rather than generating a match arm per file,
`rust-embed` is simpler. The `files` crate bakes the docs tree into the binary
this way, in
[`projects/files/src/embedded.rs`](../../../projects/files/src/embedded.rs):

```rust
#[derive(rust_embed::RustEmbed)]
#[folder = "../../docs"]
struct OrcaDocs;
```

`OrcaDocs::get("dev/00-tour.md")` retrieves the bytes at runtime, so
`orca mcp-serve` serves docs without touching the filesystem. (The web UI takes
the opposite route: the external `peacock` plugin serves it out-of-process as a
separate plugin process.)

---

## Feature flags and `cfg`

Feature flags conditionally compile parts of a crate. In `Cargo.toml`:

```toml
[features]
default = ["full"]
full = ["dep:some-optional-crate"]
```

and in code `#[cfg(feature = "full")] pub mod some_module;`. You will more often
meet `#[cfg(...)]` for platform-specific code:

```rust
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
```

Some orca crates *do* use features to gate a surface (e.g. `dispatch` has a
`server` feature). Check the crate's `Cargo.toml` when a symbol only appears
under certain builds.

---

## Importing between workspace crates

Each crate declares its dependencies in its own `Cargo.toml`; workspace crates
reference each other by **path**:

```toml
# illustrative — from projects/server/Cargo.toml
[dependencies]
utils        = { path = "../utils" }
model        = { path = "../model" }
conversation = { path = "../conversation" }
derive       = { path = "../derive" }
```

Cargo resolves the graph and compiles in topological order. To add a new library
crate: create it under `projects/`, add it to the root `Cargo.toml` `members`
list, give it an entry in `CRATE_RESPONSIBILITIES.md`, and add a `path`
dependency in any crate that needs it.

---

## Summary

| Concept | What it means |
|---|---|
| Workspace | Many crates, one lock file, shared build |
| `src/lib.rs` | Library crate root — importable by others |
| `src/main.rs` | Binary crate root — executable, not importable |
| `mod name;` | Declare a module; look for `name.rs` or `name/mod.rs` |
| `pub` / `pub(crate)` / `pub(super)` | Visibility levels |
| `use path::Name;` | Bring a name into scope |
| `pub use path::Name;` | Re-export at the current module path |
| `build.rs` | Code run before compilation; can generate `.rs` files |
| `include!(...)` | Paste a generated file inline at compile time |
| `rust-embed` | Embed entire directories into the binary (`OrcaDocs`) |
| `#[cfg(feature = "x")]` / `#[cfg(unix)]` | Conditional compilation |
