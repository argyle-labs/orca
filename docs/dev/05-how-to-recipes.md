# How-To Recipes

Step-by-step instructions for the five most common development tasks. Each recipe covers the files you touch, in order.

---

## Recipe 1: Add a New Tool

A tool is a single annotated function. The dispatch runtime projects every surface from the `#[orca_tool]` inventory (see [patterns §4](02-patterns.md#4-macro-driven-tool-dispatch-orca_tool)), so one function gives you the CLI verb, the `/api/v1` HTTP route, and the MCP tool all at once. The inventory carries the dispatch routing and the JSON schema, so the function and its args struct are the only things you write.

### Step 1: Write the function in the owning domain crate

Put the tool with the domain it belongs to (e.g. a files tool goes in [`projects/files/src/tools.rs`](../../projects/files/src/tools.rs); an agent tool in [`projects/conversation/src/run.rs`](../../projects/conversation/src/run.rs)). Annotate an `async fn` that takes a typed args struct and a `&ToolCtx` and returns a typed `Result`:

```rust
use derive::orca_tool;

#[orca_tool(domain = "example", verb = "greet", role = "read")]
async fn example_greet(args: GreetArgs, ctx: &contract::ToolCtx) -> anyhow::Result<GreetOutput> {
    // do the work; return the typed output
    Ok(GreetOutput { message: format!("hello, {}", args.target) })
}
```

- The tool name is `<domain>.<verb>` — here `example.greet`.
- The args and output structs derive `serde` (and the toolkit's schema derive); their fields become the tool's JSON schema and its typed HTTP/CLI arguments.
- Attributes carry metadata: `role = "read"` (auth role), `data_mutation = true` (marks a state-changing tool), etc. See the `#[orca_tool]` attribute parsing in [`projects/derive/src/lib.rs`](../../projects/derive/src/lib.rs).
- Use `anyhow` errors; the dispatch runtime turns your `Result` into the right per-surface response.

### Step 2: Make sure the crate is linked

The tool registers via `inventory` at link time. If the domain crate is already a dependency of the `server` binary (most are), nothing more is needed. A brand-new crate must be added to [`projects/server/Cargo.toml`](../../projects/server/Cargo.toml) and referenced so the linker keeps its inventory entries.

### Step 3: Verify

```bash
cargo check -p server
orca example greet --target world          # CLI surface
```

The same tool is now also reachable over MCP (`tools/call` with name `example.greet`) and HTTP (`POST /api/v1/example.greet`).

---

## Recipe 2: Add an HTTP Route

Most data operations belong in an `#[orca_tool]` (Recipe 1) — that automatically mounts a `/api/v1/<name>` route through `dispatch::axum_router`, which covers the common case.

Reach for a bespoke handler for endpoints that stand outside the tool surface — health, the OpenAPI document, auth bootstrap, asset/proxy routes. Those are registered in `build_router()` in [`projects/server/src/serve/mod.rs`](../../projects/server/src/serve/mod.rs).

### Step 1: Write the handler

Add an axum handler (an `async fn` returning something that implements `IntoResponse`). Extract shared state with `State<…>` and per-request values with `Extension<…>` (see [patterns §2](02-patterns.md#2-extension-injection-axum-extensiont--statet)).

### Step 2: Register the route

In `build_router()`, add the route next to the other fixed routes, e.g. `.route("/api/my-thing", get(my_handler))`. HTTP methods map to axum's `get(...)`, `post(...)`, `put(...)`, `delete(...)`, `patch(...)`.

### Step 3: Verify

```bash
cargo check -p server
cargo run -- serve --dev
curl http://localhost:12000/api/my-thing
```

---

## Recipe 3: Add a CLI Subcommand

Most CLI verbs come for free from `#[orca_tool]` — they surface as `orca <domain> <verb>` through the `dispatch::cli` inventory. Add a hand-written subcommand for a top-level verb that stands on its own outside the tool surface (like `escalate`, `run`, or `log`).

### Step 1: Add a variant to `Command`

In [`projects/server/src/main.rs`](../../projects/server/src/main.rs), add a variant to the `Command` enum (near the other subcommands). The `///` doc comment becomes the `--help` text; `#[arg(...)]` attributes control clap parsing.

### Step 2: Dispatch it

In the `match cli.command` block in `main.rs`, add an arm for your new variant that calls the handling code. Keep the handler logic in the appropriate domain crate and call into it from the arm.

### Step 3: Verify

```bash
cargo run -- my-command --help
cargo run -- my-command some-target
```

---

## Recipe 4: Add or Override an Agent

An agent is a Markdown file with YAML frontmatter — the persona and system prompt for one AI character. The canonical `wolf`/`otter`/… roster lives in the external `argyle-labs/agents` plugin, which contributes it over the `agents.register` capability; core resolves prompts against that roster and user overrides (see [domain concepts → Agents](04-domain-concepts.md#agents-named-system-prompts)). An agent can come from two places:

### Option A: Runtime override (no rebuild)

Drop a file at `~/.orca/agents/<name>.md`:

```markdown
---
name: myagent
description: One-line description shown by the agent listing tools.
tools: Read, Glob, Grep, Bash
model: inherit
color: blue
---

You are MyAgent. [system prompt here.]
```

`load_agent_prompt` (in [`projects/agents/src/resolve.rs`](../../projects/agents/src/resolve.rs)) checks override directories before the registered roster, so the file takes effect immediately. This is the fast iteration loop — edit and re-run, no build.

### Option B: Ship it in the agents plugin

To make an agent part of the standard roster, add its `.md` to the `argyle-labs/agents` plugin, which owns the roster and registers it into core over the `agents.register` capability.

### Verify

```bash
cargo run -- run -a myagent "Hello, what can you do?"
```

---

## Recipe 5: Add a Doc Page

Docs live under [`docs/`](../) and are embedded into the binary at compile time by `rust-embed` in [`projects/files/src/embedded.rs`](../../projects/files/src/embedded.rs) (`#[folder = "../../docs"]`). They are served through the `files.*` tools that back [`projects/files/src/lib.rs`](../../projects/files/src/lib.rs).

### Step 1: Create the file

Place it in the right directory — `docs/` (top level), `docs/dev/` (developer docs), `docs/dev/01-primer/` (Rust primer). Use a numeric prefix to control sort order (`06-my-topic.md`). Follow [`DOCUMENTATION-GUIDELINES.md`](../DOCUMENTATION-GUIDELINES.md): the first line must be `# Your Title` — the doc system extracts the title from the first `# ` line (see `embedded.rs`); otherwise the filename is used.

### Step 2: Index it

Link the new page from wherever its section is indexed so it's discoverable — an unlinked doc effectively doesn't exist. See the structure rules in [`DOCUMENTATION-GUIDELINES.md`](../DOCUMENTATION-GUIDELINES.md).

### Step 3: Rebuild and verify

`rust-embed` re-embeds the whole `docs/` tree on every build — no code change needed to register the file.

```bash
cargo build -p server
cargo run -- serve --dev      # then browse the docs surface
```

---

## Common Pitfalls

**A tool that never appears:** if a new `#[orca_tool]` doesn't show up on any surface, its crate's inventory entries were probably dropped by the linker — make sure the crate is a real dependency of the `server` binary and actually referenced.

**Moving vs borrowing:** a "value used after move" error usually means you need to clone instead of move. See [Ownership and Borrowing](01-primer/01-ownership-and-borrowing.md).

**Async in sync context:** calling an `async fn` without `.await` yields a "future is not used" warning; adding `.await` inside a non-async fn is a compile error. Tool functions are `async fn`.

**Missing `pub`:** functions imported from another module must be `pub`.

**Route conflicts:** axum matches some route patterns in registration order. If a new fixed route never matches, check that a more general route isn't catching it first.
