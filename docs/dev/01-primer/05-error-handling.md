# Error Handling

Open `projects/server/src/serve/auth_routes.rs`. Look at `signup`.

```rust
// projects/server/src/serve/auth_routes.rs:196
pub async fn signup(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<SignupRequest>,
) -> Response {
    let username = req.username.trim();
    if username.is_empty() {
        return err(StatusCode::BAD_REQUEST, "username required");
    }
    if username.len() > 64 {
        return err(StatusCode::BAD_REQUEST, "username too long (max 64)");
    }

    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    // ...
}
```

The return type is `Response`, not `Result<Response>`. HTTP handlers in axum do not propagate errors up — they must produce a response for every outcome, including failures.

`match db::open_default()` branches on the result:

- `Ok(c) => c` — success. Bind `c` as the local variable for the rest of the function.
- `Err(e) => return err(...)` — failure. `return` exits the function immediately with an error response. The `err(...)` helper (defined in the same file) builds a JSON error body.

`&format!("db: {e}")` — `{e}` formats the error using its `Display` implementation. `anyhow::Error` (which orca uses throughout) chains all context messages, so the full chain appears here.

This is the explicit early-return pattern. It replaces exceptions. The failure path is visible in the source code at the exact line where it can occur.

---

## Error as data: `join_all`

Local model discovery probes every registered LLM endpoint concurrently:

```rust
// projects/model/src/local.rs:61
let probes: Vec<_> = enabled
    .iter()
    .map(|p| {
        let url = p.url.clone();
        let kind = p.kind.clone();
        async move {
            let ok = if kind == "ollama" {
                probe_ollama(&url).await
            } else {
                probe_lmstudio(&url).await
            };
            if ok { Some(/* LocalLlm */) } else { None }
        }
    })
    .collect();
let results = futures_util::future::join_all(probes).await;
if let Some(llm) = results.into_iter().flatten().next() {
    return Some(llm);
}
```

`join_all(probes).await` — runs all probe futures concurrently, waits for all of them, and returns a `Vec` of results in the original order.

Notice that an individual probe does *not* fail with an error — it always produces an `Option`. A failed probe is treated as data (`None`), not propagated. This is a deliberate choice: you want to check every endpoint, not abort on the first unreachable one, then decide at the top level (`flatten().next()` = "first one that worked").

---

## `?`: propagate errors up

Now open `projects/conversation/src/sessions/context.rs`.

```rust
// projects/conversation/src/sessions/context.rs:14-25
pub fn resolve(name: &str, config: &Config) -> Result<Self> {
    let memory_root = &config.memory_root;

    let exact = memory_root.join(name).join("MEMORY.md");
    if exact.exists() {
        let content = std::fs::read_to_string(&exact)?;
        return Ok(ProjectContext {
            project: Some(name.to_string()),
            memory_content: Some(content),
        });
    }
```

Line 20: `std::fs::read_to_string(&exact)?`

The `?` operator expands to this:

```rust
let content = match std::fs::read_to_string(&exact) {
    Ok(c)  => c,
    Err(e) => return Err(e.into()),
};
```

If reading succeeds, `content` is the file contents. If it fails (file not readable, permissions error, etc.), the function returns immediately with the error. The caller of `resolve` receives `Err(...)` and must handle it.

`?` chains naturally. A function with multiple `?` calls returns at the first failure:

```rust
let config = Config::load()?;      // returns early if config fails
let ctx = ProjectContext::resolve(&project, &config)?;  // returns early if resolve fails
let mut session = Session::new(config, ctx).await?;    // returns early if session fails
```

Each `?` is a potential exit point, but none of them require a `match` block. The function reads top-to-bottom as if errors don't exist, and they are handled at whatever level calls this function.

---

## `.context("msg")`: adding context to errors

An error from `std::fs::read_to_string` says something like `"No such file or directory (os error 2)"`. That tells you what happened but not where or why.

The `anyhow::Context` trait adds a message that wraps the original error:

```rust
use anyhow::Context;

let content = std::fs::read_to_string(&exact)
    .context("failed to read MEMORY.md")?;
```

If this fails, the error becomes: `"failed to read MEMORY.md: No such file or directory (os error 2)"`. The original error is preserved after the colon.

Multiple `.context()` calls stack. If `resolve` adds context and its caller adds more context, the final error message is a chain showing the full path of failure:

```
building system prompt: failed to read MEMORY.md: No such file or directory
```

Use `.context()` whenever you propagate an error across a function boundary and the caller cannot tell from the original error what operation was in progress.

---

## `if let Ok(...)`: silent failure

```rust
// projects/server/src/serve/mod.rs:38-48
if dev {
    if let Ok(Some(s)) = state::read() {
        let active_pid = std::env::var("ORCA_DEV_PARENT_PID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(std::process::id);
        let _ = state::write(&DaemonState {
            mode: DaemonMode::Dev,
            active_pid,
            ..s
        });
    }
}
```

`if let Ok(Some(s)) = state::read()` — reads the daemon state file. If reading succeeds *and* state exists (the nested `Option`), bind `s` and run the body. If reading fails or returns `None`, skip silently.

This is the appropriate pattern when a failure is genuinely ignorable: the server will start correctly even if it cannot read or update the state file. There is no user-visible error to report; the recovery path is just "continue normally."

Contrast this with the health handler's early return: that failure is worth reporting to the caller. Here the failure is worth ignoring.

---

## `let _ = expr`: explicitly discarded results

```rust
// projects/server/src/serve/mod.rs:74-82
let _ = state::write(&DaemonState {
    daemon_pid: std::process::id(),
    // ...
});
```

`let _ = state::write(...)` — calls `state::write`, which returns `Result<()>`, and discards the result. Rust will warn if you call a `Result`-returning function without handling its value: `warning: unused Result that must be used`. The `let _` assignment suppresses that warning and communicates intent: "I know this can fail; I am deliberately not handling it."

The underscore is a documented decision, not sloppiness. When you see `let _ = expr`, ask: is this a case where the author reasoned that failure is harmless? In this case — writing a state file for observability purposes — yes. The daemon continues whether or not the state file update succeeds.

Do not use `let _ = expr` to silence errors you should be handling. Use it only when you have actually thought through the failure case.

---

## How errors surface to the user

At the top of `main()`:

```rust
// projects/server/src/main.rs:189-190
#[tokio::main]
async fn main() -> Result<()> {
```

`main` returns `Result<()>`. If any `?` inside `main` propagates an error all the way up, Rust's runtime prints the error message and exits with code 1. The `anyhow::Error` display includes the full context chain.

That is the complete error handling model:

- Errors are values returned from functions.
- `?` propagates them upward.
- `.context("msg")` adds breadcrumbs as they travel up.
- The top-level handler (here `main`, or an HTTP handler, or an MCP dispatcher) decides: return a response, log and exit, or display to the user.

No invisible propagation. No surprises.
