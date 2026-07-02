# How-To Recipes

Step-by-step instructions for the most common development tasks. Each recipe covers every file you need to touch, in the order you should touch them.

---

## Recipe 1: Add a New Tool

Tools are what Claude Code (MCP), the CLI, the HTTP API, and the OpenAPI spec all expose. One `#[orca_tool]` annotation covers all four surfaces — there is no dispatch table or tool-definition file to keep in sync.

### Step 1: Define typed args and output

In the owning domain crate (e.g. `projects/system`, `projects/pod` — wherever the capability belongs):

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct MyToolArgs {
    /// What to operate on (doc comment becomes the schema description)
    pub target: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MyToolOutput {
    pub result: String,
}
```

No opaque JSON — every payload is a typed struct or enum (hard rule).

### Step 2: Write the tool function

```rust
use derive::orca_tool;

/// One sentence that tells the LLM when and why to use this tool.
/// Be specific — the doc comment becomes the tool description.
#[orca_tool(domain = "mydomain", verb = "detail")]
async fn my_tool(args: MyToolArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<MyToolOutput> {
    let result = do_something_with(&args.target)?;
    Ok(MyToolOutput { result })
}
```

The macro emits an `OrcaTool` impl plus an `inventory::submit!` registration. At link time the tool lands in the registry that `projects/dispatch` walks — it appears as `mydomain.detail` on MCP (`tools/list`/`tools/call`), on the CLI, on HTTP under `/api/v1`, and in the OpenAPI spec.

Keep to the five-verb surface: `list`, `detail`, `create`, `update`, `delete`. Domain-specific semantics ride in an `action` field on the args, not in new verb names.

### Step 3: Persist through `db` (if needed)

If the tool reads or writes a table, the CRUD lives in `projects/db` — never inline SQL in a domain crate:

```rust
let rows = db::my_table::list()?;
```

### Step 4: Verify

```bash
cargo check -p <domain-crate>
cargo run -- mydomain detail --target foo      # CLI surface
# MCP surface: run `orca mcp-serve` and send tools/list — the tool appears
```

Regenerate the frontend client so the typed method appears on the SDK:

```bash
cd projects/frontend && npm run gen:client
```

---

## Recipe 2: Add a New HTTP API Endpoint

**Default answer: don't write an axum handler.** An `#[orca_tool]` function (Recipe 1) is automatically mounted under `/api/v1` by `dispatch::axum_router` and appears in the OpenAPI spec. Write a bespoke handler only for things that aren't tools — auth/session routes, static serving, streaming.

For those special cases:

### Step 1: Write the handler in `projects/server/src/serve/`

Auth routes live in `serve/auth_routes.rs`; follow that pattern with a `#[utoipa::path]` annotation so the endpoint appears in the spec.

### Step 2: Register it

Hand-written spec'd routes are added in `serve/openapi.rs`:

```rust
// projects/server/src/serve/openapi.rs:39
pub(super) fn openapi_router() -> OpenApiRouter {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(auth_routes::signin))
        // ← add yours here
}
```

Un-spec'd infrastructure routes (health ping, UI assets) are added directly in `build_router()` in `serve/mod.rs`.

### Step 3: Verify

```bash
cargo check -p server
cargo run -- serve --dev
curl http://localhost:12000/api/health
```

---

## Recipe 3: Add a New CLI Subcommand

Registry tools (Recipe 1) already get a CLI surface via `dispatch::cli` — `orca <domain> <verb> [args]`. Add a hand-written subcommand only for interactive/top-level verbs (like `escalate` or `daemon`).

### Step 1: Add the variant to `Command`

In `projects/server/src/main.rs`:

```rust
#[derive(Subcommand)]
enum Command {
    // ... existing variants ...

    /// Short description (shown in orca --help)
    MyCommand {
        /// Positional argument
        target: String,
        #[arg(long, default_value = "default")]
        mode: String,
    },
}
```

The doc comment (`///`) becomes the help text. `#[arg(...)]` attributes control how clap parses the argument.

### Step 2: Write the handler in the owning domain crate

Command logic lives with its domain, not in the server crate. For example, host lifecycle commands live in `projects/system/src/commands.rs`, log commands in `projects/conversation/src/log_cmd.rs`:

```rust
// projects/<domain>/src/commands.rs
pub async fn cmd_my_command(config: &Config, target: &str, mode: &str) -> anyhow::Result<()> {
    println!("Running my command on {target} with mode {mode}");
    Ok(())
}
```

### Step 3: Dispatch in `main.rs`

In the big `match cli.command` block:

```rust
Some(Command::MyCommand { target, mode }) => {
    mydomain::commands::cmd_my_command(&config, &target, &mode).await
}
```

### Step 4: Verify

```bash
cargo run -- my-command --help
cargo run -- my-command some-target
```

---

## Recipe 4: Add a New Agent

Agents are markdown prompts (YAML frontmatter + body) **contributed by plugins**, not files in this repo. The base roster (wolf, otter, …) lives in the external `argyle-labs/agents` plugin.

### Step 1: Add the agent to its plugin repo

In the plugin that should own the agent (usually `argyle-labs/agents`), add the markdown definition:

```markdown
---
name: myagent
description: One-line description used by agent.list / agent.get.
tools: Read, Glob, Grep, Bash
model: inherit
color: blue
---

You are MyAgent. [Your system prompt here.]
```

### Step 2: Register it with the plugin's `AgentProvider`

The plugin registers an `AgentProvider` (the core seam in `projects/contract/src/agents.rs`) whose `agents()` returns an `AgentDef` per agent — `name`, the full markdown `body`, and the provider `origin`. Add your new agent to that list.

### Step 3: Rebuild and reinstall the plugin, then materialize

```bash
# in the plugin repo:
cargo build --release
# then:
orca install    # re-materializes ~/.claude/agents/ from all registered providers
```

`orca install` writes every registered agent verbatim to `~/.claude/agents/<name>.md`. Later-registered providers win on name conflicts, so a per-profile plugin can override a base agent.

### Step 4: Test it

```bash
orca            # internal chat — the roster includes the new agent
# or via MCP: agent.get / agent.list
```

---

## Recipe 5: Add a New Doc Page

Documentation is embedded from `docs/` by the `files` crate (`rust-embed`). Any `.md` file you add there is automatically accessible via the files/docs tools, listed in the doc tree, and visible in the web dashboard.

### Step 1: Create the file

Place it under the appropriate directory:
- `docs/` — top-level pages
- `docs/dev/` — developer documentation (this directory)
- `docs/dev/01-primer/` — Rust primer

Name it with a number prefix to control sort order: `05-my-topic.md`.

### Step 2: Write the content

The first line must be `# Your Title` — the doc system extracts the title from the first `# ` line:

```rust
// projects/files/src/embedded.rs:56
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
cargo build -p files
```

`rust-embed` re-embeds all files on build. No other changes needed.

### Step 4: Verify

```bash
orca serve --dev
# Open the docs section in the web dashboard
```

---

## Common Pitfalls

**Untyped payloads:** `serde_json::Value` and opaque JSON are banned in tool args/outputs. Define a struct/enum with `Serialize`/`Deserialize`/`JsonSchema`.

**`#[async_trait]`:** the macro is banned. Hand-desugar async trait methods to `fn f<'a>(&'a self, …) -> BoxFuture<'a, T>` with `Box::pin(async move { … })` bodies.

**Inline SQL in a domain crate:** every persistent table's CRUD lives in `projects/db`. Domain crates call `db::<table>::*`, never open their own connection.

**Moving vs borrowing:** If you get a "value used after move" error, you probably need to clone a value instead of moving it. See [Ownership and Borrowing](01-primer/01-ownership-and-borrowing.md).

**Async in sync context:** If you call an async function without `.await`, Rust gives a "future is not used" warning. If you add `.await` in a non-async function, you get a compilation error.

**Route conflicts:** axum routes are matched in registration order for some patterns. If a new route never seems to match, check that a more general route is not catching it first.
