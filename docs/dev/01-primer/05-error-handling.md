# Error Handling

> The applied companion is [`docs/learn/rust-primer.md`](../../learn/rust-primer.md).
> Open the linked source alongside this page; the code wins if a snippet drifts.

Rust has no exceptions. Errors are returned as values using `Result<T, E>`. The
`?` operator propagates them up the call stack. `anyhow` makes this ergonomic for
application code; `thiserror` is for library error types. Orca uses `anyhow`
throughout, so its errors carry a chain of context messages.

---

## `?`: propagate errors up

Open [`projects/conversation/src/sessions/context.rs`](../../../projects/conversation/src/sessions/context.rs)
and read `ProjectContext::resolve`. It reads a file with
`std::fs::read_to_string(&exact)?`. The `?` is the whole error model in one
character:

```rust
// `x?` desugars to roughly:
let x = match fallible() {
    Ok(v)  => v,
    Err(e) => return Err(e.into()),
};
```

If the read succeeds, the value flows on. If it fails (missing file, permissions,
…), the function returns immediately with the error, and `resolve`'s caller
receives `Err(...)`. `?` chains naturally — a function with several `?` calls
returns at the first failure and otherwise reads top-to-bottom as if errors
didn't exist:

```rust
// illustrative
let config  = Config::load()?;                        // early return on failure
let ctx     = ProjectContext::resolve(&project, &config)?;
let session = Session::new(config, ctx).await?;
```

Each `?` is a potential exit point, but none of them require a `match` block.

---

## `.context("msg")`: adding breadcrumbs

A raw error from `std::fs::read_to_string` says `"No such file or directory (os
error 2)"` — what happened, but not where or why. The `anyhow::Context` trait
wraps the original error with a message:

```rust
use anyhow::Context;
let body = std::fs::read_to_string(&path).context("failed to read MEMORY.md")?;
```

On failure the message becomes `"failed to read MEMORY.md: No such file or
directory (os error 2)"` — the original preserved after the colon. Multiple
`.context()` calls stack into a chain showing the full path of failure. The
`ModelBackend` impls in
[`projects/model/src/backend/`](../../../projects/model/src/backend/) use this
heavily, e.g. `.context("failed to connect to Anthropic API")?` in
[`claude.rs`](../../../projects/model/src/backend/claude.rs). Use `.context()`
whenever you propagate an error across a boundary and the caller couldn't
otherwise tell what operation was in progress.

## `anyhow::bail!`: early-return an error

When you detect a failure yourself rather than propagating one, `bail!` builds an
`anyhow::Error` and returns it. The same backends use it for non-success HTTP
status: `bail!("Anthropic API error {status}: {text}")`. It is shorthand for
`return Err(anyhow!(...))`.

---

## Errors as data (don't always propagate)

Not every error should bubble up. Sometimes you want to *collect* outcomes and
decide at the top. The pattern is to turn each result into a value rather than a
short-circuit:

```rust
// illustrative — run N checks concurrently, keep every outcome
let results = futures_util::future::join_all(checks).await;
let report: Vec<Status> = results
    .into_iter()
    .map(|r| match r {
        Ok(v)  => Status::ok(v),
        Err(e) => Status::failed(format!("error: {e}")), // error becomes data
    })
    .collect();
```

`join_all(...).await` runs all the futures concurrently and returns their results
in order. Each entry is mapped to a value regardless of success — so a health-
style endpoint can report *all* check results instead of aborting on the first
failure. `result.is_ok()` (returns `bool`) tests the variant without consuming
it, no `match` needed.

---

## Handlers that must always produce a response

axum HTTP handlers do not propagate errors up — they must return a response for
every outcome, success or failure. See the handlers in
[`projects/server/src/serve/mod.rs`](../../../projects/server/src/serve/mod.rs)
(`ping_handler`, `proxy_http`, `static_handler`): they return `Response` (or a
concrete `Json<T>`), not `Result<Response>`. The failure path is handled inline
with an early `return` of an error response:

```rust
// illustrative — the explicit early-return pattern
let client = match pool.get_or_connect("service").await {
    Ok(c)  => c,
    Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, &format!("unavailable: {e}")),
};
```

The failure path stays visible at the exact line where it can occur — the error
travels as a return value, not through a hidden unwinding path.

---

## `if let Ok(...)`: silent, ignorable failure

Some failures are genuinely ignorable. `run` / `run_daemon` in `serve/mod.rs`
read an optional state file and simply skip when it's absent or unreadable:

```rust
// illustrative — matches the `utils::state::read()` pattern in serve/mod.rs
if let Ok(Some(s)) = utils::state::read() {
    // update state; if the read failed or returned None, do nothing
}
```

The server starts correctly whether or not it can read the state file, so there
is no user-visible error to report — the recovery path is just "continue
normally." Contrast with the handler's early return above: that failure is worth
reporting; this one is worth ignoring.

---

## `let _ = expr`: explicitly discarded results

`let _ = state::write(...)` calls a `Result`-returning function and discards the
result. Rust warns if you ignore a `#[must_use]` `Result`; `let _` suppresses the
warning *and* documents intent: "I know this can fail; I am deliberately not
handling it." The daemon writes its state file for observability and continues
whether or not the write succeeds.

Do not use `let _ = expr` to silence errors you should handle. Use it only when
you have actually reasoned that the failure is harmless.

---

## How errors surface to the user

`main` returns `Result<()>` (see
[`projects/server/src/main.rs`](../../../projects/server/src/main.rs), under
`#[tokio::main]`). If any `?` inside `main` propagates an error all the way up,
Rust's runtime prints the `anyhow::Error` — including its full context chain —
and exits with code 1.

That is the complete model:

- Errors are values returned from functions.
- `?` propagates them upward; `bail!` originates them.
- `.context("msg")` adds breadcrumbs as they travel up.
- The top-level handler (`main`, an HTTP handler, or an MCP dispatcher) decides:
  return a response, log and exit, or display to the user.

No invisible propagation. No surprises.
