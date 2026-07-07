# Rust Primer

This primer teaches the Rust concepts you'll encounter in this codebase. Every example is drawn from actual code in orca — no toy examples.

---

## Ownership: why the compiler argues with you

Rust has no garbage collector. Instead, every value has exactly one *owner*, and when the owner goes out of scope, the value is freed. This is enforced at compile time.

```rust
let s = String::from("hello");
let t = s;          // ownership moved to t
println!("{}", s);  // compile error: s was moved
```

The practical impact: you'll pass `&str` (a borrowed reference to a string) instead of `String` (an owned string) when a function only needs to read the value.

In orca this shows up everywhere:

```rust
// projects/files/src/embedded.rs — borrows the path, doesn't need to own it
pub fn read(path: &str) -> Option<String> {
    OrcaDocs::get(path).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}
```

`&str` is the read-only view. `String` is the owned, heap-allocated value. `into_owned()` converts the borrowed view into an owned `String` when you need to return it.

---

## Borrowing: & and &mut

A *borrow* lets you use a value without taking ownership. There are two kinds:

- `&T` — shared (read-only) borrow; many can exist simultaneously
- `&mut T` — exclusive (mutable) borrow; only one can exist at a time

```rust
fn print_length(s: &str) {     // borrows, doesn't own
    println!("{}", s.len());
}

fn append(s: &mut String) {    // mutable borrow
    s.push_str(" world");
}
```

The borrow checker enforces this: if you have a `&mut` borrow active, you can't also have any `&` borrows. This eliminates data races at compile time.

---

## Structs: grouping data

Structs are like classes without methods (methods are added via `impl`):

```rust
// From projects/utils/src/types.rs (simplified)
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Message { role: "user".into(), content: content.into() }
    }
}
```

`impl Into<String>` is a trait bound — it means "any type that can be converted into a String." This lets callers pass `&str` or `String` interchangeably.

`Self` inside an `impl` block means "the type this impl is for" — here, `Message`.

---

## Enums: sum types

Rust enums are far more powerful than in other languages. Each variant can hold different data:

```rust
// Simplified from the codebase
pub enum BackendResponse {
    Token(String),        // a streaming token arrived
    ToolCall(ToolCall),   // model wants to call a tool
    Done,                 // stream finished
}
```

You handle them with `match`:

```rust
match response {
    BackendResponse::Token(text) => print!("{}", text),
    BackendResponse::ToolCall(call) => execute_tool(call).await,
    BackendResponse::Done => break,
}
```

The compiler forces you to handle every variant. Forget one and the code won't compile.

---

## Option and Result: no null, no exceptions

Rust has no `null`. Instead:

- `Option<T>` — either `Some(value)` or `None`
- `Result<T, E>` — either `Ok(value)` or `Err(error)`

```rust
pub fn read(path: &str) -> Option<String> {
    OrcaDocs::get(path).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}
```

If the file isn't found, `OrcaDocs::get` returns `None`, and `map` propagates the `None` without panicking.

### The `?` operator

`?` is shorthand for "if this is an error, return the error from this function":

```rust
// Without ?
fn load() -> Result<Config, anyhow::Error> {
    let text = std::fs::read_to_string("orca.toml")?;  // returns Err if file missing
    let config: Config = toml::from_str(&text)?;         // returns Err if parse fails
    Ok(config)
}
```

`anyhow::Result<T>` is shorthand for `Result<T, anyhow::Error>` — anyhow is the error crate used throughout orca for ergonomic `?`-based propagation without defining custom error types everywhere.

---

## Traits: shared behavior

A trait is an interface — a set of methods a type must implement:

```rust
// From the model backend registry in projects/model (simplified)
pub trait ModelBackend: Send + Sync {
    async fn stream_response(
        &self,
        messages: &[Message],
    ) -> anyhow::Result<impl Stream<Item = BackendResponse>>;
}
```

Any struct that implements `ModelBackend` can be used as a backend. orca has `LmStudioBackend` and `ClaudeBackend`, both implementing this trait. Code that calls `backend.stream_response(...)` doesn't need to know which backend it's talking to.

`Send + Sync` are marker traits: `Send` means the type can be moved between threads; `Sync` means it can be shared across threads. Required because tokio runs on a multi-threaded executor.

---

## Async / await

Rust's async model is explicit: a function marked `async` returns a `Future`. The future does nothing until you `.await` it.

```rust
async fn fetch_doc(path: &str) -> anyhow::Result<String> {
    let response = reqwest::get(path).await?;   // suspend here until response arrives
    let text = response.text().await?;           // suspend here until body is read
    Ok(text)
}
```

`tokio` is the runtime that actually runs futures. orca starts it in `main.rs`:

```rust
#[tokio::main]
async fn main() {
    // everything in here can use .await
}
```

The `#[tokio::main]` attribute rewrites `main` to start the tokio executor. Nothing special — just macro sugar.

---

## Closures

Closures are anonymous functions that capture their environment:

```rust
let q = query.to_lowercase();

// This closure captures `q` by reference
let matches: Vec<String> = lines
    .filter(|(_, l)| l.to_lowercase().contains(&q))
    .map(|(i, l)| format!("L{}: {}", i + 1, l.trim()))
    .collect();
```

`filter` and `map` take closures. The `|args| body` syntax is the closure. `collect()` materializes the iterator into a `Vec`.

---

## The module system

Rust code is organized into modules. `pub` makes items visible outside the module.

```
projects/files/src/
  embedded.rs     pub fn list(), pub fn read(), pub fn tree()
```

The `files` crate is referenced in `projects/server/Cargo.toml` as:

```toml
files = { path = "../files" }
```

And used in server code as:

```rust
use files::embedded;

let content = embedded::read("architecture");
```

Crate names use hyphens in `Cargo.toml` but underscores in `use` statements. This is a quirk of the Rust toolchain.

---

## Derive macros

Many boilerplate implementations are generated automatically:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}
```

- `Debug` — enables `{:?}` formatting for logging
- `Clone` — enables `.clone()` to duplicate the value
- `Serialize` / `Deserialize` — enables JSON conversion via serde

---

## Where to go next

- [`codebase-tour`](learn/codebase-tour) — see these concepts in action across the full request lifecycle
- The [`stack`](stack) doc explains why each crate was chosen
- `projects/utils/src/types.rs` — the core shared types (`Message`, `ToolCall`, `ToolResult`)
- `projects/model` — the model backend registry (core) and its `ModelBackend` trait
