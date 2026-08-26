# Traits and Generics

> Open the linked source alongside this page; the code wins if a snippet drifts.

Traits define shared behavior. A trait says "any type implementing this trait can
do X." Generics let you write code that works over any type that satisfies
certain trait bounds. Together they are Rust's answer to interfaces, abstract
base classes, and generics in other languages — but with no runtime overhead for
the common case.

The central abstraction of orca's model layer is a trait. Understanding traits
explains the whole model-backend design.

---

## `ModelBackend`: the core trait

Open [`projects/model/src/backend/mod.rs`](../../../projects/model/src/backend/mod.rs)
and read the `ModelBackend` trait. It is declared `pub trait ModelBackend: Send + Sync`
and requires three methods: `chat(...)` (send messages, stream tokens to an
`OutputSink`, return a boxed future), `name()`, and `model_id()`.

```rust
// illustrative — the trait's essential shape
pub trait ModelBackend: Send + Sync {
    fn chat<'a>(&'a self, /* ... */) -> BoxFuture<'a, Result<BackendResponse>>;
    fn name(&self) -> &str;
    fn model_id(&self) -> &str;
}
```

This says: "any type that implements `ModelBackend` must provide `chat()`,
`name()`, and `model_id()`." Three concrete types implement it — `ClaudeBackend`
(Anthropic API), `LMStudioBackend` (local server), and `OllamaBackend` (local
server), each in its own file under
[`projects/model/src/backend/`](../../../projects/model/src/backend/). The rest
of orca only talks to `ModelBackend`; it never imports `ClaudeBackend` directly.

### `Send + Sync` bounds

The `: Send + Sync` after the trait name means any implementor must also be
`Send` (safe to move between threads) and `Sync` (safe to share references
between threads). This is required because async tasks run on a thread pool.

---

## Implementing a trait

[`projects/model/src/backend/claude.rs`](../../../projects/model/src/backend/claude.rs)
holds `impl ModelBackend for ClaudeBackend { ... }`. That block promises
"`ClaudeBackend` satisfies the `ModelBackend` contract," and the compiler checks
every method is implemented with the correct signature. `name()` returns a
literal, `model_id()` borrows a struct field, and `chat()` makes the HTTP request
to the Anthropic API. `LMStudioBackend` and `OllamaBackend` have their own impl
blocks — same trait, different HTTP calls, different URLs.

---

## Async trait methods without a macro

Trait methods cannot be written as bare `async fn` in a stable object-safe trait
(a limitation of how async compiles to state machines with lifetimes). `ModelBackend`
handles this by spelling out the `Pin<Box<dyn Future>>` boxing by hand, keeping
the future type explicit at the trait boundary and dropping the proc-macro
dependency (see [[no-async-trait-macro]]).

The convention is a type alias plus an explicit boxed future. `mod.rs` defines
`pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;`, the
trait method returns `BoxFuture<'a, Result<...>>`, and each impl writes the body
as `Box::pin(async move { ... })`. You get the same ergonomics as `async fn` at
the call site (`backend.chat(...).await`) with no proc-macro dependency and the
lifetimes made explicit.

---

## `Box<dyn Trait>`: trait objects

A *trait object* is a pointer to any type that implements a trait, where the
concrete type is not known at compile time: `Box<dyn ModelBackend>`,
`Arc<dyn ModelBackend>`, or `&dyn ModelBackend`.

The factory function `build_backend` in `mod.rs` returns one:

```rust
// illustrative — the factory's shape
pub fn build_backend(config: &Config, model: &Model) -> Result<Box<dyn ModelBackend>> {
    match model {
        Model::Claude(id)          => Ok(Box::new(ClaudeBackend::new(/* ... */))),
        Model::LMStudio { id, url } => Ok(Box::new(LMStudioBackend::new(/* ... */))),
        Model::Ollama { id, url }   => Ok(Box::new(OllamaBackend::new(/* ... */))),
    }
}
```

The return type is `Box<dyn ModelBackend>` — a heap-allocated backend whose
concrete type is chosen at runtime from `config`. The caller calls `.chat()` on
it without knowing or caring which backend it is.

**When to use trait objects:** when the concrete type is determined at runtime
(e.g., by config), when you need different types in one collection
(`Vec<Box<dyn ModelBackend>>`), or when you want to return different types from
one function.

**The tradeoff:** trait objects have a small runtime cost (dynamic dispatch —
calling through a vtable pointer). Generic functions avoid this cost but require
the type to be known at compile time.

---

## Generic functions and `impl Trait`

Instead of trait objects, generics let you write code that works for any type
satisfying a bound, resolved at compile time:

```rust
// illustrative
fn process<T: ModelBackend>(backend: &T) { println!("{}", backend.name()); }
// equivalent argument-position `impl Trait`
fn process(backend: &impl ModelBackend) { println!("{}", backend.name()); }
```

`impl Trait` in argument position means "some concrete type that implements
`ModelBackend`, determined at the call site." The compiler generates one copy of
the function per concrete type used — monomorphization.

A conversion trait in argument position is the most common form you'll write.
The `Message::user` constructor in
[`projects/model/src/types.rs`](../../../projects/model/src/types.rs) takes
`content: impl Into<String>` — "any type convertible into a `String`" — so
callers pass a `&str` literal or an owned `String` interchangeably and the
constructor does the `.into()` once, inside:

```rust
// illustrative — accept anything that converts to String
pub fn user(content: impl Into<String>) -> Self { /* content.into() */ }
```

`impl Trait` also appears in *return* position. In
[`projects/server/src/serve/mod.rs`](../../../projects/server/src/serve/mod.rs),
`mcp_catalog_handler()` returns `impl axum::response::IntoResponse` — "I return
some type that implements `IntoResponse`, but I'm not naming it." This lets axum
accept any response type without the function having to name it.

---

## Trait objects behind a type alias: `OutputSink`

The `OutputSink` alias (`Arc<Mutex<Box<dyn Write + Send>>>`) wraps a trait
object for anything implementing both `Write` and `Send`. `stdout` implements
`Write`; so does `Vec<u8>`; so does orca's own `BufferWriter` in the same file,
which stores bytes in a shared `Vec<u8>`:

```rust
// illustrative — implementing a std trait for a local type
impl Write for BufferWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> { /* push to buf */ Ok(data.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
```

Because `BufferWriter` implements `Write`, it can go anywhere an `OutputSink` is
expected — that is what `buffer_sink()` exploits to redirect what would be stdout
into an in-memory buffer. Background agent runs use exactly this: see
`buffer_sink` wired into a session in
[`projects/conversation/src/run.rs`](../../../projects/conversation/src/run.rs)
(`Session::new_with_output_and_model(..., sink, ...)`), after which the buffer is
read back as a `String`. The model backend never knew it was writing to a buffer
instead of stdout.

---

## Derive macros

The `#[derive(...)]` attribute auto-implements common traits. Orca's data types
carry them heavily — e.g. types in
[`projects/model/src/models.rs`](../../../projects/model/src/models.rs) derive
`#[derive(Serialize, Deserialize, JsonSchema, Clone)]`:

| Derive | Trait | What it gives you |
|---|---|---|
| `Debug` | `std::fmt::Debug` | `{:?}` formatting for debug output |
| `Clone` | `std::clone::Clone` | `.clone()` for a deep copy |
| `Default` | `std::default::Default` | `T::default()` constructor |
| `Serialize` | `serde::Serialize` | Convert to JSON/YAML via serde |
| `Deserialize` | `serde::Deserialize` | Parse from JSON/YAML via serde |
| `JsonSchema` | `schemars::JsonSchema` | Emit a JSON Schema for the type (tool I/O contracts) |
| `Parser` | `clap::Parser` | Parse CLI arguments |
| `Subcommand` | `clap::Subcommand` | Usable as a clap subcommand enum |

Derive macros are proc macros — they run at compile time, inspect the
struct/enum definition, and generate the implementation. You never write the
boilerplate; the macro does. (Orca's own `#[orca_tool]` attribute macro, in the
[`derive`](../../../projects/derive/src/lib.rs) crate, works the same way — it
reads a function and generates its CLI, REST, and MCP surfaces.)

---

## Trait bounds in practice

Generic functions constrain their type parameters with a `where` clause. A
helper that serializes any result to JSON reads like:

```rust
// illustrative — generic over the value type T and a closure F
fn to_json<T, F>(f: F) -> Response
where
    T: serde::Serialize,
    F: FnOnce() -> anyhow::Result<T>,
{
    match f() { Ok(v) => json_ok(v), Err(e) => json_err(e) }
}
```

The `where` clause says `T` must be `Serialize` (so it can become JSON) and `F`
must be a callable returning `Result<T>`. One implementation then handles any
serializable type; the compiler monomorphizes a separate copy per concrete `T`.

---

## Summary

| Concept | What it means | Where you see it |
|---|---|---|
| `trait Foo { fn bar(&self); }` | Defines required behavior | `ModelBackend`, `Write`, `Serialize` |
| `impl Foo for MyType { ... }` | Satisfies the contract | `impl ModelBackend for ClaudeBackend` |
| `Box<dyn Foo>` | Heap-allocated, runtime-dispatched trait object | `Box<dyn ModelBackend>` from `build_backend()` |
| `impl Foo` (argument) | Compile-time monomorphized generic | `fn process(b: &impl ModelBackend)` |
| `impl Foo` (return) | Caller need not name the concrete type | `-> impl IntoResponse` |
| `fn f<T: Foo>(x: T)` | Generic function, one copy per type | generic JSON helpers |
| `#[derive(...)]` | Auto-implement common traits | almost every struct and enum |
| `fn f<'a>(…) -> BoxFuture<'a, T>` + `Box::pin(async move …)` | Async trait method without `async_trait` | `ModelBackend` and its impls |
