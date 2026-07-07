# Traits and Generics

Traits define shared behavior. A trait says "any type implementing this trait can do X." Generics let you write code that works over any type that satisfies certain trait bounds. Together they are Rust's answer to interfaces, abstract base classes, and generics in other languages — but with no runtime overhead for the common case.

The central abstraction of orca's AI layer is a trait. Understanding traits explains the whole model backend design.

---

## `ModelBackend`: The Core Trait

Open `projects/model/src/backend/mod.rs`. The `ModelBackend` trait is defined here:

```rust
// projects/model/src/backend/mod.rs:84
pub trait ModelBackend: Send + Sync {
    /// Send messages to the model, streaming tokens to the provided output sink.
    /// Returns a boxed future — orca hand-desugars the async method rather than
    /// using the `#[async_trait]` macro (see the section below).
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        system: &'a str,
        cancel: CancellationToken,
        output: &'a OutputSink,
    ) -> BoxFuture<'a, Result<BackendResponse>>;

    /// Human-readable name for display.
    fn name(&self) -> &str;

    /// Model identifier for API calls.
    fn model_id(&self) -> &str;
}
```

This says: "any type that implements `ModelBackend` must provide `chat()`, `name()`, and `model_id()`." Three concrete types implement this trait: `ClaudeBackend` (Anthropic API), `LMStudioBackend` (local server), and `OllamaBackend` (local server). The rest of orca only talks to `ModelBackend` — it never imports `ClaudeBackend` directly.

### `Send + Sync` bounds

The `: Send + Sync` after the trait name means any type implementing `ModelBackend` must also implement `Send` (safe to move between threads) and `Sync` (safe to share references between threads). This is required because async tasks run on a thread pool.

---

## Implementing a Trait

Here is how `ClaudeBackend` implements `ModelBackend`:

```rust
// projects/model/src/backend/claude.rs:40
impl ModelBackend for ClaudeBackend {
    fn name(&self) -> &str {
        "claude"
    }

    fn model_id(&self) -> &str {
        &self.model  // borrows from the struct field
    }

    fn is_local(&self) -> bool {
        false
    }

    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        system: &'a str,
        cancel: CancellationToken,
        output: &'a OutputSink,
    ) -> BoxFuture<'a, Result<BackendResponse>> {
        Box::pin(async move {
            // ... makes HTTP request to Anthropic API ...
        })
    }
}
```

The `impl ModelBackend for ClaudeBackend` block says "I promise that `ClaudeBackend` satisfies the `ModelBackend` contract." The compiler checks that every method in the trait is implemented with the correct signature.

`LMStudioBackend` has its own `impl ModelBackend for LMStudioBackend` block in `lmstudio.rs`. Same trait, different HTTP calls, different URL.

---

## Async trait methods without a macro

Trait methods cannot be written as bare `async fn` in a stable object-safe trait (a limitation of how async compiles to state machines with lifetimes). A common workaround is the `async_trait` crate's proc macro, but orca **does not** use it on `ModelBackend` — the macro-hidden `Pin<Box<dyn Future>>` boxing is spelled out by hand instead (see [[no-async-trait-macro]]).

The convention is a type alias plus an explicit boxed future:

```rust
// projects/model/src/backend/mod.rs:23
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

- The trait method is a normal `fn` returning `BoxFuture<'a, Result<...>>`:
  `fn chat<'a>(&'a self, ...) -> BoxFuture<'a, Result<BackendResponse>>;`
- Each `impl` writes the body as `Box::pin(async move { ... })`.

You get the same ergonomics as `async fn` at the call site (`backend.chat(...).await`) with no proc-macro dependency and the lifetimes made explicit.

---

## `Box<dyn Trait>`: Trait Objects

A *trait object* is a pointer to any type that implements a trait, where the concrete type is not known at compile time. You write `Box<dyn ModelBackend>` (or `Arc<dyn ModelBackend>`, or `&dyn ModelBackend`).

The factory function uses this:

```rust
// projects/model/src/backend/mod.rs:118
pub fn build_backend(config: &Config, model: &Model) -> Result<Box<dyn ModelBackend>> {
    match model {
        Model::Claude(id) => {
            let key = config
                .anthropic_api_key
                .clone()
                .context("no API key — run `orca login`")?;
            Ok(Box::new(ClaudeBackend::new(key, id)))
        }
        Model::LMStudio { id, url } => Ok(Box::new(LMStudioBackend::new(url, id))),
        Model::Ollama { id, url } => Ok(Box::new(OllamaBackend::new(url, id))),
    }
}
```

The return type is `Box<dyn ModelBackend>` — a heap-allocated backend whose concrete type is determined at runtime based on `config`. The caller gets back a `Box<dyn ModelBackend>` and calls `.chat()` on it without knowing or caring which backend it is.

**When to use trait objects:** When the concrete type is determined at runtime (e.g., by config), when you need to store different types in a collection (`Vec<Box<dyn ModelBackend>>`), or when you want to return different types from one function.

**The tradeoff:** Trait objects have a small runtime cost (dynamic dispatch — calling through a vtable pointer). Generic functions avoid this cost but require the type to be known at compile time.

---

## Generic Functions and `impl Trait`

Instead of trait objects, you can use generics to write code that works for any type satisfying a trait bound, resolved at compile time:

```rust
// Monomorphized: one copy of the function per concrete type T
fn process<T: ModelBackend>(backend: &T) {
    println!("Using: {}", backend.name());
}

// Equivalent syntax using `impl Trait`
fn process(backend: &impl ModelBackend) {
    println!("Using: {}", backend.name());
}
```

The `impl Trait` syntax in argument position means "some concrete type that implements `ModelBackend`, determined at the call site." The compiler generates one copy of the function per concrete type used — this is called monomorphization.

In orca, `impl Trait` appears in return position too:

```rust
// projects/server/src/serve/api/health.rs:23
pub async fn ping_handler() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}
```

`impl IntoResponse` means "I return some type that implements `IntoResponse`, but I'm not naming it." This lets axum accept any response type without the function having to name it explicitly.

---

## Generic Structs

The `OutputSink` type uses generics via `Box<dyn Write>`:

```rust
// projects/model/src/backend/mod.rs:27
pub type OutputSink = Arc<Mutex<Box<dyn Write + Send>>>;
```

`Write` is a standard library trait. `Box<dyn Write + Send>` is a trait object for anything that implements both `Write` (has `.write()`, `.flush()`) and `Send` (can be moved across threads). `stdout` implements `Write`. So does `Vec<u8>`. So does orca's custom `BufferWriter`:

```rust
// projects/model/src/backend/mod.rs:43
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufferWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut buf) = self.0.lock() {
            buf.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
```

`BufferWriter` stores bytes in a shared `Vec<u8>`. Because it implements `Write`, it can be used anywhere an `OutputSink` is expected — you can redirect what would otherwise go to stdout into an in-memory buffer.

This is the `buffer_sink()` function's purpose:

```rust
// projects/model/src/backend/mod.rs:36
pub fn buffer_sink() -> (OutputSink, Arc<Mutex<Vec<u8>>>) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = BufferWriter(buf.clone());
    (Arc::new(Mutex::new(Box::new(writer))), buf)
}
```

Used in the MCP `run` handler:

```rust
// projects/server/src/mcp/handlers.rs:40
let (sink, buf) = buffer_sink();
let ctx = ProjectContext::default();
let mut session = Session::new_with_output(config.clone(), ctx, sink).await?;
session.one_shot(full_prompt).await?;

let bytes = buf.lock().unwrap();
Ok(String::from_utf8_lossy(&bytes).into_owned())
```

The session runs with the buffer sink; after it finishes, the buffer is read back as a `String`. The model backend never knew it was writing to a buffer instead of stdout.

---

## Derive Macros

The `#[derive(...)]` attribute auto-implements common traits. You will see these throughout orca:

```rust
// projects/server/src/serve/api/mod.rs:53
#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}
```

What each derive does:

| Derive | Trait | What it gives you |
|---|---|---|
| `Debug` | `std::fmt::Debug` | `{:?}` formatting for printing in debug output |
| `Clone` | `std::clone::Clone` | `.clone()` for deep copy |
| `Default` | `std::default::Default` | `T::default()` constructor (all fields zeroed/empty) |
| `Serialize` | `serde::Serialize` | Convert to JSON/YAML/etc. via serde |
| `Deserialize` | `serde::Deserialize` | Parse from JSON/YAML/etc. via serde |
| `ToSchema` | `utoipa::ToSchema` | Appear in the OpenAPI spec generated by utoipa |
| `Parser` | `clap::Parser` | Parse CLI arguments from `std::env::args()` |
| `Subcommand` | `clap::Subcommand` | Usable as a clap subcommand enum |

Derive macros are proc macros — they run at compile time, inspect the struct/enum definition, and generate the implementation code. You never write the boilerplate; the macro does it.

---

## Trait Bounds in Practice

The `db_json` helper in `serve/api/mod.rs` uses a generic bound:

```rust
// projects/server/src/serve/api/mod.rs:17
pub fn db_json<T, F>(f: F) -> Response
where
    T: serde::Serialize,
    F: FnOnce() -> anyhow::Result<T>,
{
    match f() {
        Ok(val) => Json(val).into_response(),
        Err(e)  => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
```

This function is generic over `T` (the value type) and `F` (a closure type). The `where` clause says:
- `T` must implement `serde::Serialize` (so we can convert it to JSON)
- `F` must implement `FnOnce() -> anyhow::Result<T>` (it is a callable that returns a Result)

This function can now be called with any serializable type:

```rust
db_json(|| orca_utils::db::list_mcp_servers())
db_json(|| orca_utils::db::list_schemas())
```

One implementation handles both. The compiler monomorphizes separate copies for each concrete `T`.

---

## Summary

| Concept | What it means | When you see it |
|---|---|---|
| `trait Foo { fn bar(&self); }` | Defines required behavior | `ModelBackend`, `Write`, `Serialize` |
| `impl Foo for MyType { ... }` | Satisfies the contract | `impl ModelBackend for ClaudeBackend` |
| `Box<dyn Foo>` | Heap-allocated, runtime-dispatched trait object | `Box<dyn ModelBackend>` from `build_backend()` |
| `impl Foo` in argument position | Compile-time monomorphized generic | `fn process(b: &impl ModelBackend)` |
| `impl Foo` in return position | Caller doesn't need to know the concrete type | `-> impl IntoResponse` |
| `fn f<T: Foo>(x: T)` | Generic function, one copy per type | `db_json<T, F>` |
| `#[derive(Debug, Clone, ...)]` | Auto-implement common traits | Almost every struct and enum |
| `fn f<'a>(…) -> BoxFuture<'a, T>` + `Box::pin(async move …)` | Async trait method without the `async_trait` macro | `ModelBackend` and its impls |
