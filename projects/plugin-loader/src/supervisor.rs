//! Out-of-process plugin supervisor — the counterpart to `plugin_toolkit::serve`
//! on the plugin side.
//!
//! Where [`load_plugin`](crate::load_plugin) `dlopen`s a cdylib into the daemon
//! (no crash isolation, libc/ABI coupling), the supervisor **spawns the plugin
//! as a child process** and talks to it over a Unix-domain socket using the
//! [`plugin_proto`] wire protocol:
//!
//! 1. bind a per-plugin UDS, hand its path to the child via `ORCA_PLUGIN_SOCKET`,
//!    spawn the executable, and `accept()` its connection;
//! 2. read the child's [`Hello`](Frame::Hello) (identity + tool/backend/schema
//!    surface), reply [`Welcome`](Frame::Welcome) advertising the daemon's
//!    [capabilities](crate::capability::CAPABILITIES) — or refuse on a
//!    wire-protocol major mismatch (replaces the `abi_stable` layout gate);
//! 3. [`invoke`](PluginProcess::invoke) a tool as a synchronous round-trip:
//!    write [`Invoke`](Frame::Invoke), then pump the socket — servicing the
//!    plugin's [`Cap`](Frame::Cap) requests through
//!    [`capability::handle_cap`](crate::capability::handle_cap) and forwarding
//!    [`Log`](Frame::Log) lines — until the matching [`Result`](Frame::Result).
//!
//! ## Serial contract
//!
//! Exactly one `Invoke` is in flight per plugin at a time (a `Mutex` around the
//! stream enforces it), so the mid-invoke exchange is a simple synchronous loop:
//! the only frames expected while a tool runs are that tool's `Cap` requests and
//! `Log` lines. This matches the plugin-side contract in `plugin_proto::session`
//! — no per-plugin read multiplexing.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use plugin_proto::{
    BackendDef, Frame, PROTOCOL_VERSION, ToolDef, protocol_compatible, read_frame, write_frame,
};
use serde_json::Value;

use crate::capability::{self, CAPABILITIES};

/// Env var naming the UDS path a plugin connects back on. Mirrors
/// `plugin_toolkit::serve::SOCKET_ENV`.
pub const SOCKET_ENV: &str = "ORCA_PLUGIN_SOCKET";

/// The plugin surface learned from the handshake `Hello`.
#[derive(Debug, Clone)]
pub struct Handshake {
    pub software: String,
    pub semver: String,
    pub manifest: Vec<ToolDef>,
    pub backends: Vec<BackendDef>,
    /// Declared SQL schema, verbatim (applied by the caller that owns the db).
    pub schema: Value,
}

/// Read the child's `Hello`, reply `Welcome` (advertising `capabilities`), and
/// return the declared surface. Refuses on a wire-protocol major mismatch — the
/// runtime-negotiated replacement for the compiled `abi_stable` layout tag.
pub fn handshake<S: Read + Write>(stream: &mut S, capabilities: &[&str]) -> Result<Handshake> {
    let hello = read_frame(stream)
        .context("reading plugin Hello")?
        .ok_or_else(|| anyhow!("plugin closed the socket before Hello"))?;
    let Frame::Hello {
        protocol,
        plugin,
        version,
        manifest,
        backends,
        schema,
    } = hello
    else {
        bail!("first plugin frame was not Hello");
    };
    if !protocol_compatible(&protocol, PROTOCOL_VERSION) {
        bail!(
            "plugin '{plugin}' speaks protocol {protocol}, daemon speaks {PROTOCOL_VERSION} (incompatible major)"
        );
    }
    write_frame(
        stream,
        &Frame::Welcome {
            protocol: PROTOCOL_VERSION.to_string(),
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
        },
    )
    .context("sending Welcome")?;
    Ok(Handshake {
        software: plugin,
        semver: version,
        manifest,
        backends,
        schema,
    })
}

/// Drive one tool invocation to completion over `stream`. Writes `Invoke{id}`,
/// then services `Cap`/`Log` frames until the `Result` with the matching `id`.
/// A `Cap` is executed via [`capability::handle_cap`] and answered with a
/// `CapResult`; a tool error becomes an `Err`.
pub fn invoke_on<S: Read + Write>(
    stream: &mut S,
    id: u64,
    tool: &str,
    args: Value,
) -> Result<Value> {
    write_frame(
        stream,
        &Frame::Invoke {
            id,
            tool: tool.to_string(),
            args,
        },
    )
    .with_context(|| format!("sending Invoke for '{tool}'"))?;

    loop {
        let frame = read_frame(stream)
            .with_context(|| format!("awaiting Result for '{tool}'"))?
            .ok_or_else(|| anyhow!("plugin closed the socket during '{tool}'"))?;
        match frame {
            Frame::Result {
                id: rid,
                ok,
                value,
                error,
            } if rid == id => {
                return if ok {
                    Ok(value)
                } else {
                    Err(anyhow!(
                        "plugin tool '{tool}' failed: {}",
                        error.unwrap_or_else(|| "unknown error".into())
                    ))
                };
            }
            Frame::Cap {
                id: cap_id,
                cap,
                args,
            } => {
                let reply = match capability::handle_cap(&cap, args) {
                    Ok(value) => Frame::CapResult {
                        id: cap_id,
                        ok: true,
                        value,
                        error: None,
                    },
                    Err(e) => Frame::CapResult {
                        id: cap_id,
                        ok: false,
                        value: Value::Null,
                        error: Some(e.to_string()),
                    },
                };
                write_frame(stream, &reply)
                    .with_context(|| format!("answering capability '{cap}'"))?;
            }
            Frame::Log { level, msg, .. } => match level.as_str() {
                "error" => tracing::error!(target: "plugin", "{msg}"),
                "warn" => tracing::warn!(target: "plugin", "{msg}"),
                "debug" | "trace" => tracing::debug!(target: "plugin", "{msg}"),
                _ => tracing::info!(target: "plugin", "{msg}"),
            },
            // Serial contract: a stray Result for another id, or any other frame,
            // shouldn't arrive mid-invoke. Ignore defensively rather than wedge.
            _ => {}
        }
    }
}

/// A spawned plugin subprocess and its session socket.
///
/// Drops send `Shutdown` (best-effort) and reap the child; a plugin crash is
/// isolated to this process — the daemon logs it and can respawn, never dies
/// with the plugin.
pub struct PluginProcess {
    pub software: String,
    pub semver: String,
    pub manifest: Vec<ToolDef>,
    pub backends: Vec<BackendDef>,
    pub schema: Value,
    child: Child,
    stream: Mutex<std::os::unix::net::UnixStream>,
    next_id: AtomicU64,
}

impl PluginProcess {
    /// Spawn `exe`, connect its session socket, complete the handshake, and
    /// return a live process advertising the daemon's capabilities.
    pub fn spawn(exe: &Path) -> Result<Self> {
        use std::os::unix::net::UnixListener;

        let sock_path = socket_path_for(exe);
        // Bind BEFORE spawn so the child's connect() can't race an unbound path.
        _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path)
            .with_context(|| format!("binding plugin socket {sock_path:?}"))?;

        let child = Command::new(exe)
            .env(SOCKET_ENV, &sock_path)
            .spawn()
            .with_context(|| format!("spawning plugin executable {exe:?}"))?;

        // The child connects back; accept its single session connection.
        let (mut stream, _addr) = listener
            .accept()
            .with_context(|| format!("accepting connection from plugin {exe:?}"))?;
        // The path is only needed for the connect rendezvous; unlink now so a
        // crash can't leave a stale socket blocking a respawn.
        _ = std::fs::remove_file(&sock_path);

        let hs = handshake(&mut stream, CAPABILITIES)?;
        tracing::info!(
            plugin = %hs.software,
            version = %hs.semver,
            tools = hs.manifest.len(),
            backends = hs.backends.len(),
            "spawned out-of-process plugin"
        );

        Ok(Self {
            software: hs.software,
            semver: hs.semver,
            manifest: hs.manifest,
            backends: hs.backends,
            schema: hs.schema,
            child,
            stream: Mutex::new(stream),
            next_id: AtomicU64::new(1),
        })
    }

    /// Invoke a tool. Serialized by the stream `Mutex` — one `Invoke` in flight
    /// per plugin, per the serial contract.
    pub fn invoke(&self, tool: &str, args: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| anyhow!("plugin '{}' session mutex poisoned", self.software))?;
        invoke_on(&mut *stream, id, tool, args)
    }

    /// Best-effort graceful shutdown: send `Shutdown`, then terminate + reap.
    pub fn shutdown(&mut self) {
        if let Ok(mut stream) = self.stream.lock() {
            _ = write_frame(&mut *stream, &Frame::Shutdown);
        }
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A per-plugin socket rendezvous path under the temp dir. The plugin name plus
/// the daemon pid keeps it unique across plugins and daemon restarts without
/// needing a clock or RNG (both unavailable / undesirable here).
fn socket_path_for(exe: &Path) -> std::path::PathBuf {
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("plugin");
    std::env::temp_dir().join(format!("orca-plugin-{stem}-{}.sock", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_proto::session::serve;
    use serde_json::json;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn fake_hello() -> Frame {
        Frame::Hello {
            protocol: PROTOCOL_VERSION.into(),
            plugin: "fake".into(),
            version: "0.1.0".into(),
            manifest: vec![],
            backends: vec![],
            schema: Value::Null,
        }
    }

    #[test]
    fn handshake_reads_surface_and_sends_welcome() {
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        // Plugin side: just send Hello, then read the Welcome back.
        let plugin = thread::spawn(move || {
            let mut s = plugin_end;
            write_frame(&mut s, &fake_hello()).unwrap();
            read_frame(&mut s).unwrap().unwrap()
        });

        let hs = handshake(&mut orca_end, CAPABILITIES).unwrap();
        assert_eq!(hs.software, "fake");
        assert_eq!(hs.semver, "0.1.0");

        match plugin.join().unwrap() {
            Frame::Welcome {
                protocol,
                capabilities,
            } => {
                assert_eq!(protocol, PROTOCOL_VERSION);
                assert!(capabilities.contains(&"db.op".to_string()));
            }
            f => panic!("expected Welcome, got {f:?}"),
        }
    }

    #[test]
    fn handshake_rejects_incompatible_protocol() {
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let mut s = plugin_end;
            write_frame(
                &mut s,
                &Frame::Hello {
                    protocol: "2.0".into(),
                    plugin: "fake".into(),
                    version: "0.1.0".into(),
                    manifest: vec![],
                    backends: vec![],
                    schema: Value::Null,
                },
            )
            .unwrap();
        });
        let err = handshake(&mut orca_end, CAPABILITIES)
            .unwrap_err()
            .to_string();
        assert!(err.contains("incompatible major"), "got: {err}");
    }

    #[test]
    fn invoke_echoes_tool_result() {
        // A fake plugin running the real plugin-side serve loop; the daemon side
        // drives it through handshake + invoke_on. `db.op`/`secret.op` caps are
        // not exercised here (they'd need a live db) — routing is covered in the
        // `capability` module's tests.
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || {
            serve(plugin_end, fake_hello(), |tool, args, _caps| {
                if tool == "echo" {
                    Ok(args)
                } else {
                    Err(format!("no such tool: {tool}"))
                }
            })
        });

        let hs = handshake(&mut orca_end, CAPABILITIES).unwrap();
        assert_eq!(hs.software, "fake");

        let out = invoke_on(&mut orca_end, 1, "echo", json!({"n": 7})).unwrap();
        assert_eq!(out, json!({"n": 7}));

        let err = invoke_on(&mut orca_end, 2, "missing", Value::Null)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such tool"), "got: {err}");

        write_frame(&mut orca_end, &Frame::Shutdown).unwrap();
        plugin.join().unwrap().unwrap();
    }
}
