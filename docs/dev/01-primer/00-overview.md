# Rust Primer: Overview

This primer teaches Rust using orca's own source code as the example base. Rather than abstract toy examples, every concept is demonstrated with real production code you can find in this repository. Reading the codebase will be confusing until you understand these concepts; reading these docs without the code will be abstract. Do both together.

---

## How to Use This Primer

The primer is organized so each document builds on the previous one. You do not need to read them in strict sequence — if you understand ownership but not async, skip ahead — but the order is intentional.

---

## The Six Topics

### [01 — Ownership and Borrowing](01-ownership-and-borrowing.md)

The single concept that makes Rust different from every other language. Rust has no garbage collector and no manual `free()`. Instead, the compiler tracks ownership of every value and inserts frees automatically based on static analysis.

**Why read this first:** Every other topic assumes you understand why `String` vs `&str` matters, why you sometimes get "value moved" errors, and what `.clone()` actually does. Without this foundation, the rest of the code looks arbitrary.

**Key orca examples:** `OutputSink`, `ProjectContext`, `Config` passing patterns, the `to_string()` calls throughout `context.rs`.

---

### [02 — Enums and Pattern Matching](02-enums-and-pattern-matching.md)

Rust enums are sum types: a value of type `Command` is exactly one variant, and each variant can carry different data. This is profoundly different from C/Java enums. Combined with `match`, it gives the compiler the ability to enforce that you handle every possible case.

**Why read this second:** The entire CLI is built on `Command` enum + `match`. You cannot read `main.rs` without understanding this.

**Key orca examples:** The `Command` enum in `main.rs`, `Option<T>`, `Result<T, E>`, the `LoginService` sub-enum.

---

### [03 — Traits and Generics](03-traits-and-generics.md)

Traits are Rust's answer to interfaces and type classes. A trait defines behavior; a type implements it. Generics let you write code that works over any type satisfying certain trait bounds. Trait objects (`Box<dyn Trait>`) give you runtime polymorphism.

**Why read this third:** The core abstraction of orca — the model backend — is a trait. Understanding traits explains why `ClaudeBackend` and `LMStudioBackend` can be used interchangeably, why `derive` macros work, and how `serde` serialization is plugged in.

**Key orca examples:** `ModelBackend` trait in `projects/model/src/backend/mod.rs`, `#[derive(Debug, Clone, Serialize, Deserialize)]` throughout, `OutputSink` as `Box<dyn Write + Send>`.

---

### [04 — Async/Await and Tokio](04-async-await-tokio.md)

Orca is an async program: it serves HTTP requests, reads stdin for MCP, spawns background tasks, and handles OS signals — all concurrently, on a thread pool managed by Tokio. Understanding the async model explains why functions are marked `async`, why you need `.await`, and how `tokio::select!` multiplexes multiple futures.

**Why read this fourth:** The daemon loop and MCP server are async. You cannot modify either without understanding `tokio::select!`, signal handling, and `Arc<Mutex<T>>` for shared state.

**Key orca examples:** `run_daemon()` in `serve/mod.rs`, the `while let` read loop in `mcp/mod.rs`, `tokio::spawn` for update checks.

---

### [05 — Error Handling](05-error-handling.md)

Rust has no exceptions. Errors are returned as values using `Result<T, E>`. The `?` operator propagates errors up the call stack. `anyhow` makes this ergonomic for application code; `thiserror` is for library error types.

**Why read this fifth:** Every async function in orca returns `Result<()>`. Understanding `?`, `.context()`, and `anyhow::bail!` makes the error handling readable rather than noise.

**Key orca examples:** Handler functions in `mcp/handlers.rs`, the `?` chains in `context.rs`, `anyhow::bail!` in `build_backend()`.

---

### [06 — Modules and Crates](06-modules-and-crates.md)

Rust code is organized into modules and crates. A workspace is a collection of crates that share a lock file and can depend on each other. Modules control visibility. `pub use` re-exports items so callers don't need to know the internal structure.

**Why read this last:** Once you understand the language, this explains the organizational decisions: why `orca_commands` is separate from `orca`, why `pub use` appears in every `lib.rs`, and how `build.rs` generates code at compile time.

**Key orca examples:** The workspace `Cargo.toml`, `projects/commands/src/lib.rs` re-exports, `projects/agents/src/build.rs` code generation.

---

## What This Primer Does Not Cover

These topics appear in the codebase but are not covered in depth here, because they build on the six above and are well-documented elsewhere:

- **Closures and iterators** — used heavily in `projects/docs/src/lib.rs` (`filter`, `map`, `collect`); read the Rust Book chapter on iterators once you're comfortable with ownership.
- **Lifetimes in full** — the primer gives you enough to read the code; advanced lifetime annotations rarely appear in orca.
- **Macros** (`macro_rules!`, proc macros) — `serde` and `clap` use proc macros internally; you use them via `#[derive(...)]` without needing to write them.
- **Unsafe code** — orca has essentially none.
- **Testing** (`#[cfg(test)]`) — standard Rust, well documented; not covered here.
