# Async/Await and Tokio

Open `projects/server/src/serve/mod.rs`. Read `run_daemon`.

Start at the entry point:

```rust
// projects/server/src/main.rs:189
#[tokio::main]
async fn main() -> Result<()> {
```

`#[tokio::main]` is a macro. It wraps `main()` in a Tokio runtime — the scheduler that actually runs async code. Without a runtime, futures do nothing. `async fn main` compiles to a state machine that Tokio drives to completion.

`async fn` means: this function can be paused at any `.await` point and resumed later. It does not block the thread while waiting. Other tasks run in the meantime.

Now the daemon:

```rust
// projects/server/src/serve/mod.rs:66-69
pub async fn run_daemon(port: u16, db_path: std::path::PathBuf) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let app = build_router(false, db_path);
```

Line 69: `format!("0.0.0.0:{port}").parse()?` — `format!` builds a `String` like `"0.0.0.0:12000"`. `.parse()` on a `String` attempts to parse it as a `SocketAddr`. The `?` propagates the error if parsing fails. This is ordinary Rust — no async here yet.

---

## Writing state before the loop

```rust
// projects/server/src/serve/mod.rs:72-82
let binary = resolve_daemon_binary();

let _ = state::write(&DaemonState {
    daemon_pid: std::process::id(),
    active_pid: std::process::id(),
    port,
    mode: DaemonMode::Daemon,
    binary,
    version: env!("CARGO_PKG_VERSION").to_string(),
    started_at: chrono::Utc::now(),
});
```

`state::write(...)` writes a JSON file so other processes (like the CLI) can know the daemon's PID and port. `let _ = ...` discards the `Result` — if the write fails, the daemon continues anyway. The underscore is intentional: "I know this can fail and I don't care."

---

## Signal registration

```rust
// projects/server/src/serve/mod.rs:84
let mut sigterm = signal(SignalKind::terminate())?;
```

Tokio wraps Unix signals as async streams. Instead of installing a C-style signal handler function, you call `signal(...)` once and then `.recv().await` it later — just like receiving from a channel. The signal arrives at the next `.recv()` call.

`?` propagates the error if registration fails (e.g., if the signal kind is unsupported).

---

## The crash-recovery wait

```rust
// projects/server/src/serve/mod.rs:88-113
if let Ok(Some(mut s)) = state::read() {
    if s.mode == DaemonMode::Dev {
        println!("[orca] restarted while dev session active — waiting for dev to exit");
        s.daemon_pid = std::process::id();
        let _ = state::write(&s);

        let mut sigusr2 = signal(SignalKind::user_defined2())?;
        loop {
            tokio::select! {
                _ = sigusr2.recv() => break,
                _ = sigterm.recv() => {
                    let _ = state::clear();
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    if let Ok(Some(s)) = state::read() {
                        if s.mode != DaemonMode::Dev || !pid_alive(s.active_pid) { break; }
                    } else {
                        break;
                    }
                }
            }
        }
    }
}
```

The first `tokio::select!` you encounter. Read it as: start all three futures simultaneously, wait for whichever finishes first.

- `sigusr2.recv()` — SIGUSR2 arrives. The dev server is done; break out and bind the port.
- `sigterm.recv()` — SIGTERM arrives. Shut down cleanly.
- `tokio::time::sleep(Duration::from_secs(5))` — a 5-second timer. Every 5 seconds, check if the dev process is still alive. If it died or the state file disappeared, break out.

`tokio::select!` does not poll in a busy loop. All three futures are registered with the Tokio scheduler. The thread is free to do other work. When any one of them is ready, the scheduler wakes this task.

The `break` inside the sleep arm exits the inner `loop {}`. The outer `loop {}` (the main serve loop) follows.

---

## The main serve loop

```rust
// projects/server/src/serve/mod.rs:116-143
loop {
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        anyhow::anyhow!("failed to bind {addr}: {e} — is port {port} already in use?")
    })?;
    println!("[orca] daemon listening on http://localhost:{port}");
    let _ = state::set_mode(DaemonMode::Daemon);
    let _ = state::set_active_pid(std::process::id());

    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    let parked = tokio::select! {
        result = axum::serve(listener, app.clone()) => { result?; false }
        _ = sigusr1.recv() => true,
        _ = sigterm.recv() => {
            println!("[orca] daemon shutting down");
            let _ = state::clear();
            return Ok(());
        }
        _ = tokio::signal::ctrl_c() => {
            println!("[orca] daemon shutting down");
            let _ = state::clear();
            return Ok(());
        }
    };
```

`tokio::net::TcpListener::bind(addr).await` — binds the port. The `.await` suspends here if the OS takes a moment to bind; other tasks can run. `.map_err(...)` converts the OS error to a human-readable message.

`axum::serve(listener, app.clone())` — runs the HTTP server. This future only completes if the server crashes. Under normal operation it runs forever.

Four things race in `select!`:

1. `axum::serve(...)` — server crashes (unexpected). `result?` propagates the error. Yields `false` (not parked).
2. `sigusr1.recv()` — SIGUSR1 arrives. The dev tool is asking the daemon to release the port. Yields `true` (parked).
3. `sigterm.recv()` — SIGTERM. Clean shutdown. Returns early — never reaches the `parked` variable.
4. `ctrl_c()` — Ctrl-C. Same as SIGTERM.

When `select!` picks a winner, the other futures are cancelled immediately. If SIGUSR1 wins, the `axum::serve` future is dropped — and dropping the `listener` inside it releases the port. This is how Rust's ownership model enables clean resource release: drop the value, drop the resource.

`let parked = tokio::select! { ... }` — `select!` here is an expression that returns a value. Each arm that doesn't `return` early produces a `bool`.

---

## The parked wait loop

```rust
// projects/server/src/serve/mod.rs:148-181
let mut sigusr2 = signal(SignalKind::user_defined2())?;

let _ = state::set_mode(DaemonMode::Parked);
println!("[orca] daemon parked — port {port} released");

loop {
    tokio::select! {
        _ = sigusr2.recv() => {
            println!("[orca] daemon reclaiming port {port}");
            break;
        }
        _ = sigterm.recv() => {
            println!("[orca] daemon shutting down (while parked)");
            let _ = state::clear();
            return Ok(());
        }
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            if let Ok(Some(s)) = state::read() {
                let abandoned = match s.mode {
                    DaemonMode::Dev    => !pid_alive(s.active_pid),
                    DaemonMode::Parked => s.active_pid == s.daemon_pid,
                    DaemonMode::Daemon => false,
                };
                if abandoned {
                    println!("[orca] auto-reclaiming port {port} (dev abandoned)");
                    break;
                }
            }
        }
    }
}
// Outer loop: rebind and serve again
```

Notice that `sigusr2` is registered *before* `state::set_mode(DaemonMode::Parked)`. The comment in the actual code explains why: if SIGUSR2 arrives between writing `Parked` and registering the handler, the default Unix disposition would terminate the process. Register the handler first, then write state.

Each iteration of this inner loop runs a `select!` with three arms:

- `sigusr2.recv()` — reclaim the port. `break` exits the inner loop; the outer loop iteration continues, rebinding and serving again.
- `sigterm.recv()` — shut down entirely.
- `tokio::time::sleep(Duration::from_secs(5))` — every 5 seconds, check the state file. If the dev process died (`!pid_alive`) or the state is inconsistent, auto-reclaim.

`tokio::time::sleep(Duration::from_secs(5))` — a future that completes after a fixed duration. Inside `select!`, it competes with the signal receivers. If a signal arrives in the first second, the sleep is cancelled and never fires. If the sleep fires, the signals are not consumed — they will be waited on again next iteration.

After `break`, execution falls out of the inner loop and back to the outer `loop {}`, which rebinds `TcpListener::bind` and starts serving again. The daemon never exits unless it receives SIGTERM or Ctrl-C.

---

## `tokio::spawn`: background tasks

```rust
// projects/server/src/serve/mod.rs:52
tokio::spawn(orca_commands::startup_update_check());
```

`tokio::spawn` launches a future as an independent task on the Tokio thread pool. The current task does not wait for it — execution continues to the next line immediately. The spawned task runs concurrently.

The return value is a `JoinHandle`. Here it is discarded entirely: the update check runs, prints a message if a new version is available, and finishes. Nobody needs its result.

If you need the result later, keep the handle and `.await` it:

```rust
let handle = tokio::spawn(some_task());
// ... other work ...
let result = handle.await?;
```

Spawned tasks must be `'static` — they cannot borrow from the current stack frame, because the spawning frame might be gone before the task finishes. This is why closures passed to `spawn` use `move` to capture values by ownership rather than by reference.

---

## `Arc<Mutex<T>>` in async code

Back to `projects/model/src/backend/mod.rs`:

```rust
// projects/model/src/backend/mod.rs:27
pub type OutputSink = Arc<Mutex<Box<dyn Write + Send>>>;
```

Multiple async tasks can hold a clone of `OutputSink` at the same time — `Arc` makes that possible. When a task wants to write, it calls `sink.lock()`:

```rust
// projects/model/src/backend/mod.rs:59-64
pub fn sink_write(sink: &OutputSink, data: &str) {
    if let Ok(mut w) = sink.lock() {
        let _ = w.write_all(data.as_bytes());
        let _ = w.flush();
    }
}
```

`sink.lock()` — acquires the `std::sync::Mutex`. Returns `Ok(MutexGuard)`. The guard dereferences to the writer. When the `if let` block ends, the guard drops and the lock releases.

This is `std::sync::Mutex`, not `tokio::sync::Mutex`. The distinction: `std::sync::Mutex::lock()` blocks the *thread* if another thread holds the lock. `tokio::sync::Mutex::lock().await` suspends the *task* instead, freeing the thread for other work. Use `std` mutex for short, non-async critical sections (like this one — write a few bytes and flush). Use `tokio::sync::Mutex` when you need to hold the lock across an `.await` point.
