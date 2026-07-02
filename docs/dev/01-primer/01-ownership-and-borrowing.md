# Ownership and Borrowing

Open `projects/server/src/context.rs`. Read it.

```rust
// projects/server/src/context.rs:1-9
use orca_utils::config::Config;
use anyhow::Result;

/// Resolved project context: system prompt + memory content.
#[derive(Debug, Default)]
pub struct ProjectContext {
    pub project: Option<String>,
    pub memory_content: Option<String>,
}
```

Lines 7–8: both fields are `Option<String>`. Not `Option<&str>`. The distinction matters.

`String` is a heap-allocated string that this struct owns. When a `ProjectContext` is dropped, the memory for those strings is freed automatically. Nobody else holds them. That is what "owned" means in Rust — one value, one owner, owner goes away and memory is freed.

`Option<String>` means the field is either `Some(String)` — some owned string exists — or `None` — no string at all. Rust has no null pointers. `Option` is the explicit replacement.

Now look at the function signature:

```rust
// projects/server/src/context.rs:14
pub fn resolve(name: &str, config: &Config) -> Result<Self> {
```

`name` is `&str`, not `String`. `config` is `&Config`, not `Config`.

The `&` means borrow. `resolve` is asking the caller: "let me look at your string for the duration of this call." The caller retains ownership. When `resolve` returns, the caller's string is untouched.

`&str` specifically is a borrow of string data — it points into an existing `String` or a string literal in the binary. It carries no heap allocation of its own.

The pattern is mechanical: function arguments that only need to read use borrows (`&str`, `&Config`). Struct fields that need to own data use owned types (`String`, `Config`).

---

## Walking through `resolve` line by line

```rust
// projects/server/src/context.rs:15-25
let memory_root = &config.memory_root;

// Exact match first
let exact = memory_root.join(name).join("MEMORY.md");
if exact.exists() {
    let content = std::fs::read_to_string(&exact)?;
    return Ok(ProjectContext {
        project: Some(name.to_string()),
        memory_content: Some(content),
    });
}
```

Line 15: `&config.memory_root` — borrows a field from `config`. `memory_root` is a reference; `config` still owns its data.

Line 20: `std::fs::read_to_string(&exact)?` — reads a file into a `String`. The `?` propagates the error up if reading fails (covered in the error handling primer). If it succeeds, `content` is a newly-allocated `String` that `resolve` now owns.

Line 22: `Some(name.to_string())` — `name` is `&str` (borrowed). `ProjectContext` needs `Option<String>` (owned). `.to_string()` creates a new owned `String` from the borrowed slice. This is one of the most common patterns you will write.

Line 23: `Some(content)` — `content` is already a `String`. The ownership moves into the `ProjectContext`. After this point, `content` is gone from the local scope; `ProjectContext` owns it.

```rust
// projects/server/src/context.rs:28-43
if let Ok(entries) = std::fs::read_dir(memory_root) {
    for entry in entries.flatten() {
        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();
        if dir_name.contains(name) && !dir_name.starts_with("private") {
            let memory_file = entry.path().join("MEMORY.md");
            if memory_file.exists() {
                let content = std::fs::read_to_string(&memory_file)?;
                return Ok(ProjectContext {
                    project: Some(dir_name.to_string()),
                    memory_content: Some(content),
                });
            }
        }
    }
}
```

Line 31: `entry.file_name()` returns an `OsString` — an OS-native string type. Line 32: `.to_string_lossy()` converts it to a `Cow<str>` (a type that is either borrowed or owned, depending on whether conversion was lossless). Line 37: `dir_name.to_string()` converts it to a proper owned `String` for storing in the struct.

```rust
// projects/server/src/context.rs:45-49
Ok(ProjectContext {
    project: Some(name.to_string()),
    ..Default::default()
})
```

`..Default::default()` fills remaining fields with their defaults. `Option<String>` defaults to `None`. So `memory_content` is `None` here — no memory file was found.

---

## `build_system_prompt`: borrowing `self`

```rust
// projects/server/src/context.rs:54-70
pub fn build_system_prompt(&self, config: &Config) -> String {
    let wolf_prompt = orca_agents::load_agent_prompt("wolf", &config.agents_dir())
        .unwrap_or_else(|| {
            eprintln!("warning: wolf.md not found — using minimal fallback prompt");
            "You are an AI assistant. Be precise, efficient, and honest.".to_string()
        });

    if let Some(memory) = &self.memory_content {
        format!(
            "{}\n\n---\n\n## Project Context\n\nProject: {}\n\n{memory}",
            wolf_prompt,
            self.project.as_deref().unwrap_or("unknown"),
        )
    } else {
        wolf_prompt
    }
}
```

`&self` — this method borrows the `ProjectContext`. It does not consume it. After the call, the caller still has their `ProjectContext`.

Line 61: `&self.memory_content` — borrows the `Option<String>` field. `if let Some(memory) = &self.memory_content` binds `memory` as `&String` — a borrow of the inner string, not a move out of it. The struct keeps ownership.

Line 65: `self.project.as_deref()` — converts `&Option<String>` to `Option<&str>`. This is the idiomatic way to look at an optional string without taking it. `.unwrap_or("unknown")` returns the `&str` inside or falls back to the literal.

---

## `Arc<Mutex<T>>`: ownership across threads

Now open `projects/model/src/backend/mod.rs`.

```rust
// projects/model/src/backend/mod.rs:15-16
use std::sync::{Arc, Mutex};
```

```rust
// projects/model/src/backend/mod.rs:27
pub type OutputSink = Arc<Mutex<Box<dyn Write + Send>>>;
```

Read that type inside out.

`Box<dyn Write + Send>` — a heap-allocated writer. `Box` owns it. `dyn Write` means "any type that can write bytes." `Send` means it can be moved to another thread.

`Mutex<...>` — a mutual exclusion wrapper. Only one thread can access the inner value at a time. `.lock()` returns a guard; when the guard drops, the lock releases.

`Arc<...>` — Atomically Reference Counted. Multiple owners, all of them equal. Cloning an `Arc` does not copy the data — it increments a counter. When the last `Arc` clone is dropped, the counter reaches zero and the data is freed.

The combination solves a specific problem: an async task needs to send output, but the output target needs to be shared with other tasks. You cannot give each task its own `Box<dyn Write>` because there is only one stdout. `Arc` lets multiple tasks share one writer; `Mutex` ensures they do not write simultaneously.

```rust
// projects/model/src/backend/mod.rs:36-40
pub fn buffer_sink() -> (OutputSink, Arc<Mutex<Vec<u8>>>) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = BufferWriter(buf.clone());
    (Arc::new(Mutex::new(Box::new(writer))), buf)
}
```

Line 37: `Arc::new(Mutex::new(Vec::new()))` — creates the buffer. `Arc` wraps `Mutex` wraps `Vec`.

Line 38: `buf.clone()` — clones the `Arc`, not the `Vec`. This is cheap: it increments an atomic counter. Both `buf` and `writer` now point to the same `Vec<u8>`.

Line 39: the function returns both the sink (used to write) and `buf` (used to read after the job finishes). They share the same underlying buffer through the `Arc`.

```rust
// projects/model/src/backend/mod.rs:59-64
pub fn sink_write(sink: &OutputSink, data: &str) {
    if let Ok(mut w) = sink.lock() {
        let _ = w.write_all(data.as_bytes());
        let _ = w.flush();
    }
}
```

`sink.lock()` — acquires the mutex. Returns `Ok(MutexGuard)` or `Err` if the mutex is poisoned (another thread panicked while holding it). `if let Ok(mut w) = ...` only runs the body when locking succeeds; `w` is the `MutexGuard` and dereferences as `Box<dyn Write>`. When `w` drops at the end of the block, the mutex releases automatically.

---

## Three rules

1. Every value has one owner. When the owner goes out of scope, the value is freed. No manual `free`.

2. You can have any number of shared borrows (`&T`) at once, or exactly one mutable borrow (`&mut T`) — never both simultaneously. This is the borrow checker. It eliminates data races at compile time.

3. When you need shared ownership across threads, use `Arc<T>`. When you need to mutate through shared ownership, wrap it: `Arc<Mutex<T>>`. The `Arc` counts owners; the `Mutex` serializes writes.
