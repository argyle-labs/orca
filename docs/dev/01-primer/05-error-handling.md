# Error Handling

Open `projects/server/src/serve/api/health.rs`. Look at `service_health_handler`.

```rust
// projects/server/src/serve/api/health.rs:39-60
pub async fn service_health_handler(
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
) -> Response {
    const CHECKS: &[(&str, &str)] = &[
        ("DB", "service_db_status"),
        ("Env", "service_env_status"),
        ("Engines", "service_engines_status"),
        ("Tunnel", "service_tunnel_status"),
        ("Network", "service_network_status"),
        ("Mode", "service_mode_current"),
    ];

    let client = match pool.get_or_connect("service").await {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("service MCP unavailable: {e}"),
            );
        }
    };
```

The return type is `Response`, not `Result<Response>`. HTTP handlers in axum do not propagate errors up — they must produce a response for every outcome, including failures.

Lines 52–59: `match pool.get_or_connect("service").await`. This awaits an async call and branches on the result.

- `Ok(c) => c` — success. Bind `c` as the local variable for the rest of the function.
- `Err(e) => { return err(...) }` — failure. `return` exits the function immediately with a 503 response. The `err(...)` helper builds a JSON error body.

`&format!("service MCP unavailable: {e}")` — `{e}` formats the error using its `Display` implementation. `anyhow::Error` (which orca uses throughout) chains all context messages. If `get_or_connect` failed with context, the full chain appears here.

This is the explicit early-return pattern. It replaces exceptions. The failure path is visible in the source code at the exact line where it can occur.

---

## Error as data: `join_all`

```rust
// projects/server/src/serve/api/health.rs:62-91
let futures: Vec<_> = CHECKS
    .iter()
    .map(|(label, tool)| {
        let client = client.clone();
        let label = label.to_string();
        let tool = tool.to_string();
        let cid = cid.clone();
        async move {
            let result = client.call_tool(&tool, json!({}), &cid).await;
            let output = match &result {
                Ok(v) => v["content"][0]["text"].as_str().unwrap_or("").to_string(),
                Err(e) => format!("error: {e}"),
            };
            let ok = result.is_ok() && !output.to_lowercase().contains("error");
            HealthCheck {
                label,
                tool,
                output,
                ok,
            }
        }
    })
    .collect();

let checks = futures_util::future::join_all(futures).await;
Json(HealthResponse {
    timestamp: chrono::Utc::now().to_rfc3339(),
    checks,
})
.into_response()
```

`join_all(futures).await` — runs all six health check futures concurrently, waits for all of them, and returns a `Vec` of results in the original order.

Notice that each individual check does *not* fail with an error — it always produces a `HealthCheck` value. The error is treated as data: if `call_tool` returns `Err(e)`, the output is `"error: {e}"` and `ok` is `false`. The health response always returns HTTP 200 with a JSON body describing what succeeded and what did not.

This is a deliberate design choice for health endpoints: you want to show all check results, not abort on the first failure. Treating errors as data (rather than propagating them) lets you collect everything and decide at the top level.

`result.is_ok()` — checks which variant of `Result` it is without consuming the value. `.is_ok()` returns `bool`. No `match` needed for a simple boolean test.

---

## `?`: propagate errors up

Now open `projects/server/src/context.rs`.

```rust
// projects/server/src/context.rs:14-25
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
