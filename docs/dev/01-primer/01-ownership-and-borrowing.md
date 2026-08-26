# Ownership and Borrowing

> Open the linked source alongside this page — the code wins, so if a snippet
> here has drifted, the file it points at is right.

Open [`projects/conversation/src/sessions/context.rs`](../../../projects/conversation/src/sessions/context.rs)
and read `ProjectContext`. It is a small struct — a resolved project context
holding a system prompt plus memory content — and every field that stores data
is an *owned* type, not a borrow.

## Owned vs. borrowed types

The struct's fields are `Option<String>`, not `Option<&str>`. The distinction is
the whole game:

```rust
// illustrative — the pattern, not a copy of any file
struct Holder {
    name: Option<String>, // OWNS its bytes; freed when Holder drops
}
```

`String` is a heap-allocated string that the struct owns. When the value is
dropped, the memory for those strings is freed automatically. Nobody else holds
them. That is what "owned" means in Rust: one value, one owner; owner goes away,
memory is freed.

`Option<String>` means the field is either `Some(String)` — some owned string
exists — or `None` — no string at all. Rust has no null pointers. `Option` is
the explicit replacement.

Now look at the `resolve` method's signature — it takes `name: &str` and
`config: &Config`, not `String` and `Config`:

```rust
// illustrative
fn resolve(name: &str, config: &Config) -> Result<Self> { /* ... */ }
```

The `&` means *borrow*. `resolve` is asking the caller: "let me look at your
string for the duration of this call." The caller retains ownership. When
`resolve` returns, the caller's string is untouched.

`&str` specifically is a borrow of string data — it points into an existing
`String` or a string literal in the binary. It carries no heap allocation of its
own.

The pattern is mechanical: function arguments that only need to read use borrows
(`&str`, `&Config`). Struct fields that need to own data use owned types
(`String`, `Config`).

---

## Reading `resolve`: where owned strings come from

Walk `ProjectContext::resolve` in the source. Three ownership moves recur:

- `&config.memory_root` borrows a field from `config`; `config` still owns its
  data, and you hold a reference to look through.
- `std::fs::read_to_string(&exact)?` reads a file into a brand-new `String` that
  `resolve` now owns. (The `?` propagates the read error — see the
  [error-handling primer](05-error-handling.md).)
- `name.to_string()` turns a borrowed `&str` into a fresh owned `String` so it
  can be stored in the struct's `Option<String>` field. This is one of the most
  common conversions you will write.

When the method returns `Some(content)`, ownership of `content` *moves* into the
returned `ProjectContext`; the local binding is gone afterward. When it fills
remaining fields with `..Default::default()`, an `Option<String>` defaults to
`None`.

One subtlety worth reading in place: a directory entry's `file_name()` returns
an `OsString` (an OS-native string), `.to_string_lossy()` yields a `Cow<str>`
(borrowed *or* owned depending on whether the conversion was lossless), and a
final `.to_string()` produces the owned `String` the struct needs.

---

## Borrowing `self`: `build_system_prompt`

`ProjectContext::build_system_prompt(&self, config: &Config) -> String` borrows
the context — `&self` — rather than consuming it. After the call the caller
still has their `ProjectContext`.

Inside, `if let Some(memory) = &self.memory_content` binds `memory` as a
`&String` — a borrow of the inner string, not a move out of the struct. And
`self.project.as_deref()` converts `&Option<String>` to `Option<&str>`, the
idiomatic way to look at an optional string without taking ownership; a trailing
`.unwrap_or("unknown")` supplies a fallback `&str`.

```rust
// illustrative — the borrow-through-Option pattern
if let Some(text) = &self.memory_content {   // text: &String, struct keeps it
    use_it(text.as_str());
}
let name = self.project.as_deref().unwrap_or("unknown"); // Option<&str>
```

---

## `Arc<Mutex<T>>`: ownership across threads

Now open [`projects/model/src/backend/mod.rs`](../../../projects/model/src/backend/mod.rs)
and read the `OutputSink` type alias. It is
`Arc<Mutex<Box<dyn Write + Send>>>`. Read it inside out:

- `Box<dyn Write + Send>` — a heap-allocated writer. `Box` owns it. `dyn Write`
  means "any type that can write bytes." `Send` means it can be moved to another
  thread.
- `Mutex<...>` — a mutual-exclusion wrapper. Only one thread accesses the inner
  value at a time. `.lock()` returns a guard; when the guard drops, the lock
  releases.
- `Arc<...>` — Atomically Reference Counted. Multiple equal owners. Cloning an
  `Arc` does not copy the data — it increments a counter. When the last clone
  drops, the counter reaches zero and the data is freed.

The combination solves a specific problem: an async task needs to send output,
but the output target is shared with other tasks. You cannot give each task its
own `Box<dyn Write>` because there is only one stdout. `Arc` lets multiple tasks
share one writer; `Mutex` ensures they do not write simultaneously.

The `buffer_sink()` factory in the same file shows the cheap-clone idiom: it
creates an `Arc<Mutex<Vec<u8>>>` buffer, then hands one `Arc` clone to a writer
and returns the other for reading back after a job finishes. `buf.clone()`
clones the `Arc`, not the `Vec` — it increments an atomic counter, and both
handles point at the same underlying buffer.

The `sink_write` helper shows locking:

```rust
// illustrative — lock, use, auto-release
if let Ok(mut guard) = sink.lock() {
    let _ = guard.write_all(data.as_bytes());
}   // guard drops here → mutex released
```

`.lock()` returns `Ok(MutexGuard)` or `Err` if the mutex is poisoned (a thread
panicked while holding it). The `if let Ok(...)` runs the body only when locking
succeeds; when the guard drops at the end of the block, the mutex releases
automatically.

---

## Three rules

1. Every value has one owner. When the owner goes out of scope, the value is
   freed. No manual `free`.

2. You can have any number of shared borrows (`&T`) at once, or exactly one
   mutable borrow (`&mut T`) — never both simultaneously. This is the borrow
   checker. It eliminates data races at compile time.

3. When you need shared ownership across threads, use `Arc<T>`. When you need to
   mutate through shared ownership, wrap it: `Arc<Mutex<T>>`. The `Arc` counts
   owners; the `Mutex` serializes writes.
