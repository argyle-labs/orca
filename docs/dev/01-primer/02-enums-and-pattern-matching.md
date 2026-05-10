# Enums and Pattern Matching

Open `projects/server/src/main.rs`.

The first struct you see is the CLI definition:

```rust
// projects/server/src/main.rs:13-26
#[derive(Parser)]
#[command(name = "orca", about = "Context-first AI agent orchestrator", version)]
struct Cli {
    /// Project context to load (e.g. "meerkat"). Omit for general session.
    #[arg(value_name = "PROJECT")]
    project: Option<String>,

    /// Use classic readline mode instead of the split-pane TUI.
    #[arg(long)]
    classic: bool,

    #[command(subcommand)]
    command: Option<Command>,
}
```

`project: Option<String>` — the user may or may not pass a project name. `Option<String>` is exactly that: `Some(String)` if they did, `None` if they didn't. Rust has no null; `Option` is the explicit replacement.

`command: Option<Command>` — the user may or may not pass a subcommand. When there's no subcommand, `command` is `None` and orca starts an interactive session.

Now look at `Command`:

```rust
// projects/server/src/main.rs:28-177
#[derive(Subcommand)]
enum Command {
    Login {
        #[command(subcommand)]
        service: LoginService,
    },
    Auth,
    Logout {
        #[command(subcommand)]
        service: LoginService,
    },
    Projects,
    Agents,
    Escalate {
        question: String,
        #[arg(long)]
        project: Option<String>,
    },
    Run {
        #[arg(short = 'a', long, default_value = "wolf")]
        agent: String,
        prompt: String,
    },
    McpServe,
    Serve {
        #[arg(long)]
        dev: bool,
        #[arg(short, long, default_value = "12000")]
        port: u16,
    },
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    // ...and more
}
```

This is an enum. Each variant is a different subcommand. Three shapes appear here:

- `Auth`, `McpServe`, `Projects` — unit variants carrying no data.
- `Serve { dev: bool, port: u16 }` — struct variants carrying named fields.
- `Login { service: LoginService }` — struct variant carrying another enum.

In other languages you might model this as a base class with subclasses, or a struct with many optional fields. The enum is more precise: each variant carries exactly the data it needs, nothing more.

---

## `match` is exhaustive

Now look at how `Command` is dispatched. This is in `main()`, starting at line 208:

```rust
// projects/server/src/main.rs:208-288
match cli.command {
    Some(Command::Login { service }) => match service {
        LoginService::Anthropic => cmd::cmd_login(&config),
        LoginService::Github    => cmd_oauth_github().await,
        LoginService::Atlassian => cmd_oauth_atlassian().await,
    },
    Some(Command::Logout { service }) => match service {
        LoginService::Anthropic => { let _ = cmd::cmd_logout(); Ok(()) },
        LoginService::Github    => cmd_logout_github(),
        LoginService::Atlassian => cmd_logout_atlassian(),
    },
    Some(Command::Auth)     => cmd::cmd_auth(&config),
    Some(Command::Projects) => cmd::cmd_projects(&config),
    Some(Command::Agents)   => cmd::cmd_agents(&config),
    Some(Command::Escalate { question, project }) => {
        escalate(&config, &question, project.as_deref()).await
    }
    Some(Command::Doctor) => cmd::cmd_doctor(&config),
    Some(Command::Log { action }) => cmd::cmd_log(&config, action),
    Some(Command::Run { agent, prompt }) => run_one_shot(&config, &agent, &prompt).await,
    Some(Command::McpServe) => mcp::serve(&config).await,
    Some(Command::Serve { dev, port }) => serve::run(dev, port, config.db_path.clone()).await,
    Some(Command::Daemon { action }) => match action {
        DaemonAction::Start { port } => serve::run_daemon(port, config.db_path.clone()).await,
        other => cmd::cmd_daemon(other),
    },
    // ... every variant covered
    None => {
        // no subcommand: start interactive session
        let mut session = Session::new(config, ctx).await?;
        session.run_tui().await
    }
}
```

The `match` expression requires you to cover every possible variant of every type. If you add a new variant to `Command` and do not add a corresponding arm here, the compiler refuses to build:

```
error[E0004]: non-exhaustive patterns: `Some(Command::NewThing)` not covered
```

This is not a warning. It is an error. The exhaustiveness guarantee is the point: no new feature can be added to the CLI without wiring it into `main`.

---

## Destructuring in match arms

When you match a variant that carries data, you destructure it in the same expression:

```rust
// projects/server/src/main.rs:241
Some(Command::Serve { dev, port }) => serve::run(dev, port, config.db_path.clone()).await,
```

`{ dev, port }` in the pattern binds those two fields as local variables inside the arm body. `dev` is `bool`, `port` is `u16`. No field access with `.dev` or `.port` needed — the destructuring does it.

Compare:

```rust
Some(Command::Escalate { question, project }) => {
    escalate(&config, &question, project.as_deref()).await
}
```

`question` is a `String`. `&question` borrows it for passing to `escalate` (which takes `&str`). `project` is `Option<String>`. `.as_deref()` converts it to `Option<&str>` — the idiomatic way to pass an optional string by reference.

---

## Nested match: sub-enums

The `Login` variant contains another enum:

```rust
// projects/server/src/main.rs:179-187
#[derive(Subcommand)]
enum LoginService {
    Anthropic,
    Github,
    Atlassian,
}
```

Dispatching it:

```rust
// projects/server/src/main.rs:209-213
Some(Command::Login { service }) => match service {
    LoginService::Anthropic => cmd::cmd_login(&config),
    LoginService::Github    => cmd_oauth_github().await,
    LoginService::Atlassian => cmd_oauth_atlassian().await,
},
```

First the outer `match` destructures `Command::Login`, binding `service`. Then an inner `match` on `service`. Each level is exhaustive. `clap`'s `#[derive(Subcommand)]` parses the CLI into this nested structure automatically.

---

## `Option<T>` is an enum

`Option` is defined in the standard library as:

```rust
enum Option<T> {
    Some(T),
    None,
}
```

You have already seen `cli.command: Option<Command>`. The `None` arm at the end of the dispatch is when no subcommand was given:

```rust
// projects/server/src/main.rs:269-287
None => {
    let explicit = cli.project.as_deref().unwrap_or("");
    let project = if explicit.is_empty() {
        detect_project_from_cwd(&config).unwrap_or_default()
    } else {
        explicit.to_string()
    };
    let ctx = if project.is_empty() {
        ProjectContext::default()
    } else {
        ProjectContext::resolve(&project, &config)?
    };
    let mut session = Session::new(config, ctx).await?;
    if cli.classic {
        session.run().await
    } else {
        session.run_tui().await
    }
}
```

Line 270: `cli.project.as_deref().unwrap_or("")` — `cli.project` is `Option<String>`. `.as_deref()` borrows the inner string to get `Option<&str>`. `.unwrap_or("")` returns the `&str` if `Some`, or `""` if `None`. No match required for simple extraction.

`.unwrap_or_default()` on line 272: returns the `Some` value if present, or `String::default()` (empty string) if `None`. Equivalent to `match ... { Some(s) => s, None => String::new() }`.

---

## `if let`: single-variant matching

From `context.rs`, which you read in the ownership primer:

```rust
// projects/server/src/context.rs:61
if let Some(memory) = &self.memory_content {
    format!("{}\n\n---\n\n## Project Context\n\n{memory}", wolf_prompt, ...)
} else {
    wolf_prompt
}
```

`if let Some(memory) = &self.memory_content` is shorthand for a two-arm `match` where you only care about the `Some` case. `memory` is bound to the inner `&String` if the option is `Some`. The `else` branch handles `None`.

Use `if let` when you have one variant to act on and want to ignore the rest. Use `match` when you need to handle multiple variants or need exhaustiveness.

---

## `Result<T, E>` is an enum

`Result` is also an enum:

```rust
enum Result<T, E> {
    Ok(T),
    Err(E),
}
```

Every fallible function in orca returns `Result`. The `?` operator is pattern matching in disguise — it expands to a `match` that returns early on `Err` and unwraps `Ok`. See the error handling primer for the full treatment.

---

## The catch-all arm: `_` and `other`

When a variant is handled by another function, rather than inline:

```rust
// projects/server/src/main.rs:242-245
Some(Command::Daemon { action }) => match action {
    DaemonAction::Start { port } => serve::run_daemon(port, config.db_path.clone()).await,
    other => cmd::cmd_daemon(other),
},
```

`other` binds the unmatched variant and passes it to `cmd::cmd_daemon`. This is equivalent to `_ => cmd::cmd_daemon(action)` but names the value so it can be used.

The `_` wildcard discards the value:

```rust
_ => anyhow::bail!("unknown tool: {name}")
```

Use `_` when you do not need the value. Use a name when you do.
