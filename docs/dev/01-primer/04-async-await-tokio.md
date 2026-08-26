# Async/Await and Tokio

> Open the linked source alongside this page; the code wins if a snippet drifts.

Orca is an async program: it serves HTTP, reads stdin for MCP, spawns background
tasks, and handles OS signals — all concurrently, on a thread pool managed by
Tokio. The clearest place to learn the model is the daemon: read `run_daemon`
(and its sibling `run`) in
[`projects/server/src/serve/mod.rs`](../../../projects/server/src/serve/mod.rs).

## The runtime and `async fn`

`main` in [`projects/server/src/main.rs`](../../../projects/server/src/main.rs)
is annotated `#[tokio::main]`:

```rust
#[tokio::main]
async fn main() -> Result<()> { /* ... */ }
```

`#[tokio::main]` wraps `main()` in a Tokio runtime — the scheduler that actually
runs async code. Without a runtime, futures do nothing. `async fn main` compiles
to a state machine that Tokio drives to completion.

`async fn` means: this function can be paused at any `.await` point and resumed
later. It does not block the thread while waiting; other tasks run in the
meantime.

---

## Ordinary code before the async

`run_daemon` opens with plain synchronous Rust: it parses a `SocketAddr` with
`format!("0.0.0.0:{port}").parse()?` (the `?` propagates a parse error), builds
the axum router, and writes a small JSON state file so other processes (like the
CLI) can learn the daemon's PID and port. That write is done with
`let _ = state::write(...)` — the `let _` discards the `Result`, saying "I know
this can fail and the daemon continues anyway." (See the
[error-handling primer](05-error-handling.md) for `let _` and `?`.)

---

## Signals as async streams

Tokio wraps Unix signals as async streams. Instead of installing a C-style
signal handler, you register once and `.recv().await` later — just like
receiving from a channel:

```rust
// illustrative
let mut sigterm = signal(SignalKind::terminate())?;
// ... later, inside a select! ...
_ = sigterm.recv() => { /* SIGTERM arrived */ }
```

The `?` propagates the error if registration fails (e.g., an unsupported signal
kind).

---

## `tokio::select!`: race several futures

`run_daemon` is built around `tokio::select!`. Read it as: start all the listed
futures simultaneously, wait for whichever finishes first, run that arm, and
cancel the rest. The main serve loop races the HTTP server against the shutdown
and port-handoff signals:

```rust
// illustrative — the shape of the daemon's main select!
let parked = tokio::select! {
    result = axum::serve(listener, app.clone()) => { result?; false }
    _ = sigusr1.recv() => true,                      // park: release the port
    _ = sigterm.recv() => { state::clear(); return Ok(()); }  // clean shutdown
    _ = tokio::signal::ctrl_c() => { state::clear(); return Ok(()); }
};
```

Key properties to take from the real code:

- `axum::serve(...)` runs the HTTP server; under normal operation its future
  never completes, so the daemon sits in `select!` until a signal fires.
- `select!` does not busy-poll. Every future is registered with the scheduler
  and the thread is free for other work until one becomes ready.
- When a winner is picked, the losing futures are **cancelled and dropped**. If
  SIGUSR1 wins, the `axum::serve` future is dropped — and dropping the
  `listener` it owns releases the port. This is Rust's ownership model enabling
  clean resource release: drop the value, drop the resource.
- `let parked = tokio::select! { ... }` — `select!` is an expression; each arm
  that doesn't `return` early yields a value (here a `bool`).

The daemon also uses `select!` with a `tokio::time::sleep(Duration::from_secs(5))`
arm as a periodic timer: every five seconds it re-checks the state file, but if a
signal arrives first the sleep is simply cancelled and never fires. Read the
parked-wait loop in the source for the full pattern — note that the SIGUSR2
handler is registered *before* the state is written to `Parked`, so a signal
arriving in the gap can't fall through to the default (process-terminating)
disposition.

---

## `tokio::spawn`: background tasks

`run` launches fire-and-forget background work with `tokio::spawn`, e.g.
`tokio::spawn(system::commands::startup_update_check())`:

```rust
// illustrative
tokio::spawn(some_task());   // runs concurrently; caller does not wait
```

`tokio::spawn` launches a future as an independent task on the thread pool. The
current task does not wait — execution continues immediately. The return value
is a `JoinHandle`; here it is discarded because nobody needs the result. If you
do need it, keep the handle and `.await` it later:

```rust
let handle = tokio::spawn(some_task());
let result = handle.await?;
```

Spawned tasks must be `'static` — they cannot borrow from the current stack
frame, because the spawning frame might be gone before the task finishes. That
is why closures passed to `spawn` use `move` to capture values by ownership.

---

## `Arc<Mutex<T>>` in async code

Back in
[`projects/model/src/backend/mod.rs`](../../../projects/model/src/backend/mod.rs),
the `OutputSink` alias (`Arc<Mutex<Box<dyn Write + Send>>>`) is what lets
multiple async tasks share one output target — `Arc` makes the clone cheap. To
write, a task locks:

```rust
// illustrative — sink_write's shape
if let Ok(mut guard) = sink.lock() {
    let _ = guard.write_all(data.as_bytes());
}   // guard drops → lock released
```

Note this is `std::sync::Mutex`, **not** `tokio::sync::Mutex`. The distinction:
`std::sync::Mutex::lock()` blocks the *thread* if another thread holds the lock;
`tokio::sync::Mutex::lock().await` suspends the *task* instead, freeing the
thread. Use the `std` mutex for short, non-async critical sections (like writing
a few bytes and flushing). Use `tokio::sync::Mutex` only when you must hold the
lock across an `.await` point.
