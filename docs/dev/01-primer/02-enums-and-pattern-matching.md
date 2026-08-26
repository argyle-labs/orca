# Enums and Pattern Matching

> Open the linked source alongside this page — when a snippet here and the file
> disagree, the file wins.

Open [`projects/server/src/main.rs`](../../../projects/server/src/main.rs). The
CLI is built entirely out of two enums and `match`.

## `Option<T>` in the CLI struct

The top-level `Cli` struct (derived from `clap::Parser`) has two optional
fields: `project: Option<String>` and `command: Option<Command>`.

```rust
// illustrative — the shape, not a copy
struct Cli {
    project: Option<String>,   // user may or may not pass a project
    command: Option<Command>,  // user may or may not pass a subcommand
}
```

`Option<String>` is exactly "maybe a string": `Some(String)` if the user passed
one, `None` if they didn't. Rust has no null; `Option` is the explicit
replacement. When `command` is `None`, orca starts an interactive session.

## The `Command` enum

The `Command` enum in `main.rs` has one variant per subcommand. Three shapes
appear:

- **Unit variants** carrying no data — e.g. `McpServe`.
- **Struct variants** carrying named fields — e.g. `Serve { dev: bool, port: Option<u16> }`.
- **Struct variants carrying another enum** — e.g. `Pod { action: PodAction }`.

```rust
// illustrative — the three variant shapes
enum Command {
    McpServe,                              // unit
    Serve { dev: bool, port: Option<u16> },// struct variant
    Pod { action: PodAction },             // carries a sub-enum
    // ...
}
```

In other languages you might model this as a base class with subclasses, or a
struct with many optional fields. The enum is more precise: each variant carries
exactly the data it needs, nothing more. Read the real variant list in the
source — it is the authoritative command surface, and it changes as commands are
added.

---

## `match` is exhaustive

`main()` dispatches `cli.command` with a `match`. Each arm handles one variant
and destructures its data in the same expression:

```rust
// illustrative — arm shapes mirror the real dispatch in main()
match cli.command {
    Some(Command::Run { agent, prompt }) => run_one_shot(&config, &agent, &prompt).await,
    Some(Command::McpServe)              => mcp::serve(&config).await,
    Some(Command::Serve { dev, port })   => serve::run(dev, port?, db).await,
    None => { /* no subcommand → interactive session */ }
    // ... every variant covered
}
```

A `match` must cover every possible variant. Add a new variant to `Command`
without adding a corresponding arm and the compiler refuses to build:

```
error[E0004]: non-exhaustive patterns: `Some(Command::NewThing)` not covered
```

This is not a warning. It is an error. The exhaustiveness guarantee is the
point: no new subcommand can be added to the CLI without wiring it into the
dispatch.

---

## Destructuring in match arms

When a variant carries data, you destructure it in the pattern. `{ dev, port }`
binds those two fields as local variables inside the arm body — no `.dev` /
`.port` field access needed. For `Command::Escalate { question, project }`, the
body borrows `&question` to pass to a function taking `&str`, and calls
`project.as_deref()` to turn `Option<String>` into `Option<&str>` — the
idiomatic way to pass an optional string by reference.

---

## Nested match: sub-enums

The `Pod` variant carries a `PodAction` enum (also derived from
`clap::Subcommand`). Dispatching it is a `match` inside a `match`:

```rust
// illustrative
Some(Command::Pod { action }) => match action {
    PodAction::Init         => cmd_pod_init().await,
    PodAction::Ping { host } => cmd_pod_ping(&host).await,
    // ...
},
```

First the outer `match` destructures `Command::Pod`, binding `action`. Then an
inner `match` on `action`. Each level is exhaustive. `clap`'s
`#[derive(Subcommand)]` parses the CLI into this nested structure automatically.

---

## `Option<T>` and `Result<T, E>` are enums

Both are ordinary enums from the standard library:

```rust
enum Option<T> { Some(T), None }
enum Result<T, E> { Ok(T), Err(E) }
```

You have already seen `cli.command: Option<Command>`. The `None` arm at the end
of the dispatch handles "no subcommand": it resolves a project context and
starts an interactive `Session` (`session.run_tui()`, or `session.run()` under
`--classic`). Simple extractions don't need a full `match` — `.as_deref()`,
`.unwrap_or("")`, and `.unwrap_or_default()` pull the inner value or a fallback
in one call.

Every fallible function in orca returns `Result`. The `?` operator is pattern
matching in disguise — it expands to a `match` that returns early on `Err` and
unwraps `Ok`. See the [error-handling primer](05-error-handling.md) for the full
treatment.

---

## `if let`: single-variant matching

When you care about only one variant, `if let` is shorthand for a two-arm
`match`. `ProjectContext::build_system_prompt` in
[`projects/conversation/src/sessions/context.rs`](../../../projects/conversation/src/sessions/context.rs)
uses it:

```rust
// illustrative
if let Some(memory) = &self.memory_content {
    // memory: &String — bound only in the Some case
} else {
    // None case
}
```

Use `if let` when you have one variant to act on and want to ignore the rest.
Use `match` when you need to handle multiple variants or need exhaustiveness.

---

## The catch-all arm: `_` and named bindings

When a variant is handled by another function rather than inline, bind it to a
name and pass it along — e.g. `other => cmd_daemon(other)`. That is equivalent
to `_ => cmd_daemon(action)` but names the value so it can be used. The `_`
wildcard discards the value entirely — use it when you do not need the value, a
name when you do:

```rust
_ => anyhow::bail!("unknown tool: {name}")
```
