//! Out-of-process plugin entrypoint: the loop a plugin's `main()` runs.
//!
//! A subprocess plugin connects the socket orca handed it (`$ORCA_PLUGIN_SOCKET`),
//! declares its surface with a [`Frame::Hello`], then serves `Invoke → dispatch →
//! Result` until orca sends `Shutdown` (or closes). Tools reach orca's DB/secret
//! services through the capability sink ([`crate::runtime::with_cap_sink`]) — HTTP
//! stays in-process for now (see the reqwest-shedding follow-up).
//!
//! ## Single-thread by design
//!
//! The loop drives tool futures on a **current-thread** tokio runtime so a tool's
//! `db_op`/`secret_op` (which read the thread-local cap sink) run on the same
//! thread that owns the socket. Combined with orca's serial-dispatch contract
//! (one `Invoke` in flight per plugin), the socket needs no locking beyond the
//! `RefCell` that lets the loop and the cap round-trip share it.
#![cfg(all(feature = "tools", feature = "db"))]
// serve() carries tool args/results as JSON `Value` across the socket — the same
// transport-dynamic boundary as the FFI's `args_json`; typing happens in the
// tools against their schemas. Sanctioned escape hatch, scoped to this seam.
#![allow(clippy::disallowed_types)]

use std::cell::Cell;
use std::io::{Read, Write};
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use plugin_proto::{
    Frame, PROTOCOL_VERSION, ToolDef, protocol_compatible, read_frame, write_frame,
};
use serde_json::Value;

use crate::export::{manifest_for_prefixes, minimal_ctx};
use crate::runtime::{CapSink, with_cap_sink};

/// What a plugin declares about itself when it starts serving. The tool manifest
/// is derived from the linked `#[orca_tool]` inventory filtered to `prefixes`.
pub struct PluginSpec {
    /// Plugin name (== `target_software`).
    pub name: String,
    /// Plugin's own semantic version.
    pub version: String,
    /// Tool namespaces this plugin owns, each trailing-dot included
    /// (e.g. `["proxmox."]`, or `["sonarr.", "radarr."]` for a multi-app plugin).
    pub prefixes: Vec<String>,
    /// The plugin's `backends()` JSON (topology/unit/host_facts/… backends).
    pub backends_json: String,
    /// The plugin's `schemas()` JSON (declared SQL). Empty-decl is fine.
    pub schema_json: String,
    /// Optional hybrid backend dispatch — the subprocess counterpart to the
    /// cdylib hybrid `invoke`'s first arm. Given `(tool, args_json)`, returns
    /// `Some(result)` if this call is a bespoke backend op (e.g. proxmox's
    /// `proxmox.__unit.*`), or `None` to fall through to the `#[orca_tool]`
    /// dispatch surface. `None` here means a pure tool plugin. Same signature as
    /// `export_tool_plugin!`'s `backend_dispatch`.
    pub backend_dispatch: Option<BackendDispatch>,
}

/// Hybrid backend dispatch fn: `(tool, args_json) -> Option<Result<result_json,
/// error>>`. Identical shape to the cdylib export macro's `backend_dispatch`.
pub type BackendDispatch = fn(&str, &str) -> Option<std::result::Result<String, String>>;

/// Environment variable orca's supervisor sets to the per-plugin socket path.
pub const SOCKET_ENV: &str = "ORCA_PLUGIN_SOCKET";

/// Connect the orca-provided socket and serve until shutdown. A plugin's
/// `main()` calls this and returns its result.
pub fn serve(spec: PluginSpec) -> Result<()> {
    let path = std::env::var(SOCKET_ENV)
        .with_context(|| format!("{SOCKET_ENV} not set — this binary must be run by orca"))?;
    let stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("connect plugin socket {path}"))?;
    serve_on(stream, spec)
}

/// Serve over an already-connected stream. Split from [`serve`] so the loop is
/// testable over an in-memory `UnixStream` pair.
pub fn serve_on<S: Read + Write + 'static>(stream: S, spec: PluginSpec) -> Result<()> {
    let stream = Rc::new(std::cell::RefCell::new(stream));

    // ── Handshake: Hello → Welcome (major-compatible). ──
    let manifest: Vec<ToolDef> = {
        let prefixes: Vec<&str> = spec.prefixes.iter().map(String::as_str).collect();
        serde_json::from_str(&manifest_for_prefixes(&prefixes)).context("parse tool manifest")?
    };
    // Verbatim passthrough: the daemon parses each element into its own richer
    // `BackendDef`, so we must not narrow it through a proto struct here.
    let backends: Vec<Value> =
        serde_json::from_str(&spec.backends_json).context("parse backends json")?;
    let schema: Value = serde_json::from_str(&spec.schema_json).context("parse schema json")?;
    let hello = Frame::Hello {
        protocol: PROTOCOL_VERSION.to_string(),
        plugin: spec.name.clone(),
        version: spec.version.clone(),
        manifest,
        backends,
        schema,
    };
    write_frame(&mut *stream.borrow_mut(), &hello)?;

    match read_frame(&mut *stream.borrow_mut())? {
        Some(Frame::Welcome { protocol, .. }) => {
            if !protocol_compatible(&protocol, PROTOCOL_VERSION) {
                bail!("daemon protocol {protocol} incompatible with plugin {PROTOCOL_VERSION}");
            }
        }
        Some(other) => bail!("expected Welcome, got {other:?}"),
        None => bail!("daemon closed before Welcome"),
    }

    // ── Serve loop on a current-thread runtime. ──
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build plugin current-thread runtime")?;
    let ctx = minimal_ctx();
    let cap_id = Rc::new(Cell::new(0u64));

    loop {
        let frame = read_frame(&mut *stream.borrow_mut())?;
        let Some(frame) = frame else { break }; // clean EOF
        match frame {
            Frame::Invoke { id, tool, args } => {
                let sink = cap_sink(&stream, &cap_id);
                let result = with_cap_sink(sink, || {
                    // Hybrid arm first (mirrors the cdylib `invoke`): a bespoke
                    // backend op (e.g. `proxmox.__unit.*`) is handled by
                    // `backend_dispatch`; anything it declines falls through to
                    // the `#[orca_tool]` dispatch surface.
                    if let Some(bd) = spec.backend_dispatch {
                        let args_json =
                            serde_json::to_string(&args).unwrap_or_else(|_| "null".to_string());
                        if let Some(res) = bd(&tool, &args_json) {
                            return res.map_err(|e| anyhow::anyhow!("{e}")).and_then(|s| {
                                serde_json::from_str(&s).with_context(|| {
                                    format!("backend '{tool}' returned invalid JSON")
                                })
                            });
                        }
                    }
                    rt.block_on(crate::dispatch::dispatch(&tool, args, &ctx))
                });
                let reply = match result {
                    Ok(value) => Frame::Result {
                        id,
                        ok: true,
                        value,
                        error: None,
                    },
                    Err(e) => Frame::Result {
                        id,
                        ok: false,
                        value: Value::Null,
                        error: Some(format!("{e:#}")),
                    },
                };
                write_frame(&mut *stream.borrow_mut(), &reply)?;
            }
            Frame::Shutdown => break,
            // Stray frames outside an active capability round-trip — ignore.
            _ => {}
        }
    }
    Ok(())
}

/// Build the capability sink for one `Invoke`: a closure that performs a
/// `Cap → CapResult` round-trip on the shared socket. `db_op`/`secret_op` reach
/// it through the thread-local installed by [`with_cap_sink`].
fn cap_sink<S: Read + Write + 'static>(
    stream: &Rc<std::cell::RefCell<S>>,
    cap_id: &Rc<Cell<u64>>,
) -> CapSink {
    let stream = Rc::clone(stream);
    let cap_id = Rc::clone(cap_id);
    Box::new(move |cap: &str, op_json: &str| {
        let args: Value = serde_json::from_str(op_json)
            .map_err(|e| format!("capability {cap}: bad op json: {e}"))?;
        let id = cap_id.get();
        cap_id.set(id.wrapping_add(1));
        let mut s = stream.borrow_mut();
        write_frame(
            &mut *s,
            &Frame::Cap {
                id,
                cap: cap.to_string(),
                args,
            },
        )
        .map_err(|e| format!("capability {cap}: send: {e}"))?;
        loop {
            match read_frame(&mut *s).map_err(|e| format!("capability {cap}: {e}"))? {
                Some(Frame::CapResult {
                    id: rid,
                    ok,
                    value,
                    error,
                }) if rid == id => {
                    return if ok {
                        serde_json::to_string(&value)
                            .map_err(|e| format!("capability {cap}: bad reply: {e}"))
                    } else {
                        Err(error.unwrap_or_else(|| format!("capability {cap} failed")))
                    };
                }
                Some(Frame::Shutdown) => return Err(format!("capability {cap}: shutdown")),
                Some(_) => continue,
                None => return Err(format!("capability {cap}: connection closed")),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn spec() -> PluginSpec {
        PluginSpec {
            name: "test".into(),
            version: "0.0.0".into(),
            prefixes: vec!["test.".into()],
            backends_json: "[]".into(),
            schema_json: r#"{"namespace":"","tables":[]}"#.into(),
            backend_dispatch: None,
        }
    }

    /// A hybrid backend dispatch that owns `test.__be.*` and declines the rest,
    /// mirroring how a real plugin routes its `__unit.*` backend ops.
    fn be_dispatch(tool: &str, args_json: &str) -> Option<std::result::Result<String, String>> {
        let op = tool.strip_prefix("test.__be.")?;
        Some(Ok(format!(r#"{{"op":"{op}","echo":{args_json}}}"#)))
    }

    #[test]
    fn hybrid_backend_dispatch_handles_backend_ops() {
        let (plugin_end, orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || {
            let mut s = spec();
            s.backend_dispatch = Some(be_dispatch);
            serve_on(plugin_end, s)
        });

        let mut orca = orca_end;
        let _ = read_frame(&mut orca).unwrap().unwrap(); // Hello
        write_frame(
            &mut orca,
            &Frame::Welcome {
                protocol: PROTOCOL_VERSION.into(),
                capabilities: vec![],
            },
        )
        .unwrap();
        // A backend op → handled by backend_dispatch, not tool dispatch.
        write_frame(
            &mut orca,
            &Frame::Invoke {
                id: 1,
                tool: "test.__be.start".into(),
                args: json!({"unit": "vm/100"}),
            },
        )
        .unwrap();
        match read_frame(&mut orca).unwrap().unwrap() {
            Frame::Result { id, ok, value, .. } => {
                assert_eq!(id, 1);
                assert!(ok, "backend op should succeed");
                assert_eq!(value["op"], "start");
                assert_eq!(value["echo"]["unit"], "vm/100");
            }
            f => panic!("expected Result, got {f:?}"),
        }
        write_frame(&mut orca, &Frame::Shutdown).unwrap();
        plugin.join().unwrap().unwrap();
    }

    #[test]
    fn handshake_then_unknown_tool_errors_then_shutdown() {
        let (plugin_end, orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || serve_on(plugin_end, spec()));

        let mut orca = orca_end;
        // 1. Hello arrives with our declared identity.
        match read_frame(&mut orca).unwrap().unwrap() {
            Frame::Hello {
                plugin, protocol, ..
            } => {
                assert_eq!(plugin, "test");
                assert_eq!(protocol, PROTOCOL_VERSION);
            }
            f => panic!("expected Hello, got {f:?}"),
        }
        // 2. Welcome.
        write_frame(
            &mut orca,
            &Frame::Welcome {
                protocol: PROTOCOL_VERSION.into(),
                capabilities: vec![],
            },
        )
        .unwrap();
        // 3. Invoke an unregistered tool → dispatch errors → Result{ok:false}.
        write_frame(
            &mut orca,
            &Frame::Invoke {
                id: 7,
                tool: "test.nope".into(),
                args: json!({}),
            },
        )
        .unwrap();
        match read_frame(&mut orca).unwrap().unwrap() {
            Frame::Result { id, ok, error, .. } => {
                assert_eq!(id, 7);
                assert!(!ok);
                assert!(error.is_some());
            }
            f => panic!("expected Result, got {f:?}"),
        }
        // 4. Shutdown ends the loop cleanly.
        write_frame(&mut orca, &Frame::Shutdown).unwrap();
        plugin.join().unwrap().unwrap();
    }

    #[test]
    fn incompatible_welcome_is_rejected() {
        let (plugin_end, orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || serve_on(plugin_end, spec()));
        let mut orca = orca_end;
        let _ = read_frame(&mut orca).unwrap().unwrap(); // Hello
        write_frame(
            &mut orca,
            &Frame::Welcome {
                protocol: "2.0".into(),
                capabilities: vec![],
            },
        )
        .unwrap();
        assert!(plugin.join().unwrap().is_err());
    }
}
