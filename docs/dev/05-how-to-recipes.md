# How-To Recipes

Step-by-step instructions for the five most common development tasks. Each recipe covers every file you need to touch, in the order you should touch them.

---

## Recipe 1: Add a New MCP Tool

MCP tools are what Claude Code calls. Adding one requires changes in three places: the tool definition (what Claude sees), the dispatch table (which function to call), and the handler (the actual logic).

### Step 1: Write the handler function

Add your function to `projects/server/src/mcp/handlers.rs`:

```rust
// In projects/server/src/mcp/handlers.rs

pub fn my_new_tool(args: &Value, config: &Config) -> Result<String> {
    // Extract required arguments:
    let target = args["target"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("target is required"))?;

    // Do the work:
    let result = do_something_with(target, config)?;

    // Return a string — this is what Claude sees as the tool result:
    Ok(format!("Result for {target}: {result}"))
}

// For async tools:
pub async fn my_async_tool(args: &Value, config: &Config) -> Result<String> {
    let url = args["url"].as_str().ok_or_else(|| anyhow::anyhow!("url required"))?;
    let response = reqwest::get(url).await.context("HTTP request failed")?;
    let text = response.text().await?;
    Ok(text)
}
```

Rules for handlers:
- Return `Result<String>` — the string is returned to Claude as-is
- Use `anyhow::anyhow!("msg")` for user-facing errors
- Prefer `&str` argument extraction over `.to_string()` clones where possible

### Step 2: Add the import in `mod.rs`

At the top of `projects/server/src/mcp/mod.rs`, add your handler to the `use` statement:

```rust
use handlers::{
    agents, get_agent, get_config, // ... existing imports ...
    my_new_tool,    // ← add this
    my_async_tool,  // ← and this
};
```

### Step 3: Add the dispatch arm

In the `dispatch` function in `projects/server/src/mcp/mod.rs`:

```rust
async fn dispatch(name: &str, args: &Value, config: &Config) -> Result<String> {
    match name {
        // ... existing arms ...
        "my_new_tool"   => my_new_tool(args, config),
        "my_async_tool" => my_async_tool(args, config).await,
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}
```

The string `"my_new_tool"` is the canonical tool name — this is exactly what Claude Code will use in its `tools/call` requests.

### Step 4: Add the tool definition

In `projects/server/src/mcp/tools.rs`, add your tool to the `json!([...])` array:

```rust
{
    "name": "my_new_tool",
    "description": "One sentence that tells the LLM when and why to use this tool. Be specific — the LLM uses this to decide whether to call it.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "target": {
                "type": "string",
                "description": "What to operate on"
            }
        },
        "required": ["target"]
    }
},
```

**Important:** The `name` here must exactly match the string in the `dispatch` match arm.

### Step 5: Verify

```bash
cargo check -p orca
```

Then test it by running `orca mcp-serve` and sending a `tools/list` request to see your tool appear.

---

## Recipe 2: Add a New HTTP API Endpoint

HTTP endpoints are how the web dashboard and external callers access orca's data. You need: a handler function, route registration, and (optionally) an OpenAPI annotation.

### Step 1: Pick or create the right handler file

Group endpoints by domain. Existing files:
- `serve/api/health.rs` — health checks
- `serve/api/mcp.rs` — MCP server registry
- `serve/api/docs.rs` — documentation
- `serve/api/logs.rs` — session logs

If your endpoint does not fit any existing file, create a new one: `serve/api/my_feature.rs`.

### Step 2: Write the handler

```rust
// In projects/server/src/serve/api/my_feature.rs

use axum::response::{IntoResponse, Json, Response};
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use super::prelude::*;  // brings in McpState, err(), db_json(), etc.

#[derive(Serialize, ToSchema)]
pub struct MyResponse {
    pub items: Vec<String>,
    pub count: usize,
}

// Endpoint with no arguments:
#[utoipa::path(
    get,
    path = "/api/my-feature",
    operation_id = "listMyFeature",
    responses(
        (status = 200, description = "List of items", body = MyResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "my-feature"
)]
pub async fn list_handler() -> Response {
    db_json(|| {
        let items = orca_utils::db::my_list()?;
        Ok(MyResponse { count: items.len(), items })
    })
}

// Endpoint that needs the MCP pool:
pub async fn status_handler(
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
) -> Response {
    let client = match pool.get_or_connect("some-server").await {
        Ok(c)  => c,
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
    };
    // ... use client ...
    Json(json!({ "ok": true })).into_response()
}
```

### Step 3: Declare the module

If you created a new file, add it to `projects/server/src/serve/api/mod.rs`:

```rust
// Near the top of serve/api/mod.rs:
pub mod my_feature;
```

And add a prelude import if your handler needs the shared helpers:

```rust
// In your handler file's mod.rs-compatible prelude, or inline:
use crate::serve::api::{err, db_json, McpState, ErrorResponse, OkResponse};
```

### Step 4: Register the route

In `projects/server/src/serve/mod.rs`, find the `build_router` function and add your route:

```rust
// In build_router():
.route("/api/my-feature",        get(api::my_feature::list_handler))
.route("/api/my-feature/status", get(api::my_feature::status_handler))
```

HTTP methods map to axum routing functions: `get(...)`, `post(...)`, `put(...)`, `delete(...)`, `patch(...)`.

### Step 5: Add to OpenAPI spec (optional but recommended)

In `projects/server/src/serve/openapi.rs` (or wherever the `orca_spec_json` function assembles the spec), add your handler to the `paths` list so it appears in the API reference.

### Step 6: Verify

```bash
cargo check -p orca
cargo run -- serve --dev
curl http://localhost:12000/api/my-feature
```

---

## Recipe 3: Add a New CLI Subcommand

CLI subcommands are the verbs of the `orca` command. Adding one requires a variant in the `Command` enum, a handler in `orca_commands`, and a dispatch arm in `main.rs`.

### Step 1: Add the variant to `Command`

In `projects/server/src/main.rs`, add a variant to the `Command` enum:

```rust
#[derive(Subcommand)]
enum Command {
    // ... existing variants ...

    /// Short description (shown in orca --help)
    MyCommand {
        /// Positional argument
        target: String,
        /// Optional flag
        #[arg(long, default_value = "default")]
        mode: String,
    },
}
```

The doc comment (`///`) becomes the help text. `#[arg(...)]` attributes control how clap parses the argument.

### Step 2: Write the handler in `orca_commands`

Add a new file `projects/commands/src/my_cmd.rs`:

```rust
use anyhow::Result;
use orca_utils::config::Config;

pub fn cmd_my_command(config: &Config, target: &str, mode: &str) -> Result<()> {
    println!("Running my command on {target} with mode {mode}");
    // ... actual logic ...
    Ok(())
}
```

If the command needs `async`, return `Result<()>` and make the function `async`:

```rust
pub async fn cmd_my_command_async(config: &Config, target: &str) -> Result<()> {
    let result = some_async_operation(target).await?;
    println!("{result}");
    Ok(())
}
```

### Step 3: Export from `orca_commands/src/lib.rs`

```rust
// In projects/commands/src/lib.rs:
pub mod my_cmd;
pub use my_cmd::cmd_my_command;
```

### Step 4: Dispatch in `main.rs`

In the big `match cli.command` block in `main.rs`:

```rust
Some(Command::MyCommand { target, mode }) => {
    cmd::cmd_my_command(&config, &target, &mode)
}
```

For async:

```rust
Some(Command::MyCommand { target }) => {
    cmd::cmd_my_command_async(&config, &target).await
}
```

### Step 5: Verify

```bash
cargo run -- my-command --help
cargo run -- my-command some-target
```

---

## Recipe 4: Add a New Agent

Agents are named Markdown files. Adding one is mostly non-Rust work, but it is worth understanding the full process.

### Step 1: Create the agent file

Create `projects/agents/src/agents/myagent.md`:

```markdown
---
name: myagent
description: One-line description used by list_agents and get_agent.
tools: Read, Glob, Grep, Bash
model: inherit
color: blue
---

You are MyAgent. [Your system prompt here.]

You have these capabilities:
- ...

When given a task, you:
1. ...
```

The YAML frontmatter between `---` lines is parsed by `frontmatter_field_from_str`. The body after the second `---` is the actual system prompt.

### Step 2: Rebuild

```bash
cargo build -p orca_agents
```

The `build.rs` script scans `src/agents/` and generates the match arm:

```rust
"myagent" => Some(include_str!("/path/to/myagent.md")),
```

The agent is now embedded in the binary.

### Step 3: Override at runtime (optional)

For iterative development, drop the file at `~/.orca/agents/myagent.md` — the filesystem lookup runs before the embedded lookup. You can edit the file and test without rebuilding.

### Step 4: Test it

```bash
cargo run -- run -a myagent "Hello, what can you do?"
```

Or via MCP:
```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_agent","arguments":{"name":"myagent"}}}' | cargo run -- mcp-serve
```

---

## Recipe 5: Add a New Doc Page

Documentation is embedded from `docs/`. Any `.md` file you add there is automatically:
- Accessible via `read_doc` MCP tool
- Searchable via `search_docs` MCP tool
- Listed in the doc tree
- Visible in the web dashboard's docs section

### Step 1: Create the file

Place it under the appropriate directory:
- `docs/` — top-level pages
- `docs/dev/` — developer documentation (this directory)
- `docs/dev/01-primer/` — Rust primer

Name it with a number prefix to control sort order: `05-my-topic.md`.

### Step 2: Write the content

The first line must be `# Your Title` — the doc system extracts the title from the first `# ` line:

```rust
// docs/lib.rs:53
fn doc_title(path: &str) -> String {
    OrcaDocs::get(path)
        .and_then(|f| {
            let content = String::from_utf8_lossy(&f.data);
            content
                .lines()
                .find(|l| l.starts_with("# "))
                .map(|l| l[2..].trim().to_string())
        })
        // ...
}
```

If no `# ` line is found, the filename is used as the title.

### Step 3: Rebuild

```bash
cargo build -p orca_docs
```

`rust-embed` re-embeds all files on build. No other changes needed.

### Step 4: Verify

```bash
# Via CLI:
orca mcp-serve
# In another terminal:
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_docs","arguments":{}}}' | orca mcp-serve

# Or via web dashboard:
orca serve --dev
# Open http://localhost:12000/docs
```

---

## Common Pitfalls

**Dispatch/definition mismatch:** If you add a match arm in `dispatch()` but forget the tool definition in `tools.rs` (or vice versa), the tool will either be callable but invisible, or visible but uncallable. Always update both.

**Moving vs borrowing:** If you get a "value used after move" error when adding a new handler, you probably need to clone a value instead of moving it. See [Ownership and Borrowing](01-primer/01-ownership-and-borrowing.md).

**Async in sync context:** If you call an async function without `.await`, Rust gives a "future is not used" warning. If you add `.await` to a non-async function, you get a compilation error. Make sure your handler function is `async fn` before using `.await`.

**Missing `pub`:** If your new function is not exported from its module, `mod.rs` imports will fail. All functions you import elsewhere need `pub`.

**Route conflicts:** axum routes are matched in registration order for some patterns. If a new route never seems to match, check that a more general route is not catching it first.
