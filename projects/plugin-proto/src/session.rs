//! Plugin-side session state machine: the blocking serve loop a plugin process
//! runs after connecting its socket.
//!
//! Kept here (not in `plugin-toolkit`) so it stays dependency-light and
//! unit-testable over any `Read + Write` — the toolkit layer wraps [`serve`]
//! with the real `dispatch::dispatch` tool handler and a capability-backed
//! `ToolCtx`.
//!
//! ## Serial contract
//!
//! orca dispatches **one `Invoke` at a time per plugin** (the supervisor
//! enforces it). So while the handler runs, the only frames the plugin expects
//! are the [`CapResult`](Frame::CapResult)s for capabilities *it* requested (or
//! a [`Shutdown`](Frame::Shutdown)). This keeps the loop and the capability
//! round-trip a simple synchronous exchange over one socket — no per-plugin
//! read multiplexing.

use serde_json::Value;

use crate::{Frame, PROTOCOL_VERSION, ProtoError, protocol_compatible, read_frame, write_frame};

/// Capability channel handed to the invoke handler: lets a tool call back into
/// orca for a host capability (`http.request`, `db.op`, `secret.get`, …) as a
/// synchronous round-trip on the session socket.
pub struct Caps<'a, S: std::io::Read + std::io::Write> {
    stream: &'a mut S,
    next_id: &'a mut u64,
}

impl<S: std::io::Read + std::io::Write> Caps<'_, S> {
    /// Request a host capability and block until its result. `Err` carries the
    /// daemon's error message (or a session error rendered as a string).
    pub fn call(&mut self, cap: &str, args: Value) -> Result<Value, String> {
        let id = *self.next_id;
        *self.next_id = self.next_id.wrapping_add(1);
        write_frame(
            self.stream,
            &Frame::Cap {
                id,
                cap: cap.to_string(),
                args,
            },
        )
        .map_err(|e| format!("capability {cap}: send failed: {e}"))?;
        loop {
            match read_frame(self.stream).map_err(|e| format!("capability {cap}: {e}"))? {
                Some(Frame::CapResult {
                    id: rid,
                    ok,
                    value,
                    error,
                }) if rid == id => {
                    return if ok {
                        Ok(value)
                    } else {
                        Err(error.unwrap_or_else(|| format!("capability {cap} failed")))
                    };
                }
                Some(Frame::Shutdown) => {
                    return Err(format!("capability {cap}: shutdown received"));
                }
                // Serial contract: nothing else should arrive mid-capability.
                // Ignore stray frames defensively rather than deadlock.
                Some(_) => continue,
                None => return Err(format!("capability {cap}: connection closed")),
            }
        }
    }

    /// Request a STREAMING host capability and drive each chunk through `on_chunk`
    /// as it arrives, blocking until the stream ends. The daemon answers the
    /// `Cap` with zero or more [`Frame::CapStreamChunk`] (each `data` passed to
    /// `on_chunk`) then one [`Frame::CapStreamEnd`]. `Ok(())` on a clean end;
    /// `Err` carries a mid-stream failure or a session error. `on_chunk`'s own
    /// `Err` aborts consumption early (the caller stops pulling; the daemon's
    /// remaining chunks are drained-and-dropped by the next exchange).
    ///
    /// Additive to [`call`](Caps::call): a one-shot capability still returns a
    /// single `CapResult`, which this method also accepts as a zero-chunk stream
    /// whose `value` is delivered as the sole chunk — so a caller can treat any
    /// capability uniformly if it wishes.
    pub fn call_stream(
        &mut self,
        cap: &str,
        args: Value,
        mut on_chunk: impl FnMut(u64, Value) -> Result<(), String>,
    ) -> Result<(), String> {
        let id = *self.next_id;
        *self.next_id = self.next_id.wrapping_add(1);
        write_frame(
            self.stream,
            &Frame::Cap {
                id,
                cap: cap.to_string(),
                args,
            },
        )
        .map_err(|e| format!("capability {cap}: send failed: {e}"))?;
        loop {
            match read_frame(self.stream).map_err(|e| format!("capability {cap}: {e}"))? {
                Some(Frame::CapStreamChunk { id: rid, seq, data }) if rid == id => {
                    on_chunk(seq, data)?;
                }
                Some(Frame::CapStreamEnd { id: rid, ok, error }) if rid == id => {
                    return if ok {
                        Ok(())
                    } else {
                        Err(error.unwrap_or_else(|| format!("capability {cap} stream failed")))
                    };
                }
                // A one-shot daemon answered with a single CapResult: deliver its
                // value as the sole chunk, then end.
                Some(Frame::CapResult {
                    id: rid,
                    ok,
                    value,
                    error,
                }) if rid == id => {
                    return if ok {
                        on_chunk(0, value)
                    } else {
                        Err(error.unwrap_or_else(|| format!("capability {cap} failed")))
                    };
                }
                Some(Frame::Shutdown) => {
                    return Err(format!("capability {cap}: shutdown received"));
                }
                Some(_) => continue,
                None => return Err(format!("capability {cap}: connection closed")),
            }
        }
    }
}

/// Drive the plugin session to completion over `stream`.
///
/// 1. sends `hello` (must be [`Frame::Hello`]),
/// 2. awaits [`Frame::Welcome`] and checks protocol major compatibility,
/// 3. loops: on [`Frame::Invoke`] runs `handler(tool, args, caps)` and replies
///    with [`Frame::Result`]; on [`Frame::Shutdown`] returns cleanly; on a
///    clean EOF returns cleanly.
///
/// `handler` returns `Ok(value)` (tool output) or `Err(msg)` (tool error);
/// either is encoded into the `Result` frame. It may use `caps` to call host
/// capabilities.
pub fn serve<S, F>(mut stream: S, hello: Frame, mut handler: F) -> Result<(), ProtoError>
where
    S: std::io::Read + std::io::Write,
    F: FnMut(&str, Value, &mut Caps<S>) -> Result<Value, String>,
{
    if !matches!(hello, Frame::Hello { .. }) {
        return Err(ProtoError::Handshake("first frame must be Hello".into()));
    }
    write_frame(&mut stream, &hello)?;

    match read_frame(&mut stream)? {
        Some(Frame::Welcome { protocol, .. }) => {
            if !protocol_compatible(&protocol, PROTOCOL_VERSION) {
                return Err(ProtoError::Handshake(format!(
                    "daemon protocol {protocol} incompatible with plugin {PROTOCOL_VERSION}"
                )));
            }
        }
        Some(other) => {
            return Err(ProtoError::Handshake(format!(
                "expected Welcome, got {}",
                frame_kind(&other)
            )));
        }
        None => return Err(ProtoError::Handshake("closed before Welcome".into())),
    }

    let mut cap_id = 0u64;
    while let Some(frame) = read_frame(&mut stream)? {
        match frame {
            Frame::Invoke { id, tool, args } => {
                let result = {
                    let mut caps = Caps {
                        stream: &mut stream,
                        next_id: &mut cap_id,
                    };
                    handler(&tool, args, &mut caps)
                };
                let reply = match result {
                    Ok(value) => Frame::Result {
                        id,
                        ok: true,
                        value,
                        error: None,
                    },
                    Err(error) => Frame::Result {
                        id,
                        ok: false,
                        value: Value::Null,
                        error: Some(error),
                    },
                };
                write_frame(&mut stream, &reply)?;
            }
            Frame::Shutdown => break,
            // Stray frames (a late CapResult, an unexpected Hello) — ignore.
            _ => {}
        }
    }
    Ok(())
}

fn frame_kind(f: &Frame) -> &'static str {
    match f {
        Frame::Hello { .. } => "Hello",
        Frame::Welcome { .. } => "Welcome",
        Frame::Invoke { .. } => "Invoke",
        Frame::Result { .. } => "Result",
        Frame::Cap { .. } => "Cap",
        Frame::CapResult { .. } => "CapResult",
        Frame::CapStreamChunk { .. } => "CapStreamChunk",
        Frame::CapStreamEnd { .. } => "CapStreamEnd",
        Frame::Log { .. } => "Log",
        Frame::Shutdown => "Shutdown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn hello() -> Frame {
        Frame::Hello {
            protocol: PROTOCOL_VERSION.into(),
            plugin: "test".into(),
            version: "0.0.0".into(),
            manifest: vec![],
            backends: vec![],
            schema: Value::Null,
        }
    }

    #[test]
    fn handshake_rejects_incompatible_welcome() {
        // Preload a Welcome with an incompatible major on the read side.
        let mut buf = Vec::new();
        buf.extend(crate::encode(&hello()).unwrap()); // (ignored on read side)
        let welcome = crate::encode(&Frame::Welcome {
            protocol: "2.0".into(),
            capabilities: vec![],
        })
        .unwrap();
        // A duplex mock: reads the welcome, discards writes.
        struct Mock {
            read: Cursor<Vec<u8>>,
        }
        impl std::io::Read for Mock {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.read.read(b)
            }
        }
        impl std::io::Write for Mock {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let _ = buf;
        let mock = Mock {
            read: Cursor::new(welcome),
        };
        let err = serve(mock, hello(), |_, _, _| Ok(Value::Null)).unwrap_err();
        assert!(matches!(err, ProtoError::Handshake(_)));
    }

    #[test]
    fn full_session_invoke_and_capability() {
        let (plugin_end, orca_end) = UnixStream::pair().unwrap();

        // Plugin: echoes args, and for tool "with_cap" makes an http.request cap.
        let plugin = thread::spawn(move || {
            serve(plugin_end, hello(), |tool, args, caps| {
                if tool == "with_cap" {
                    let got = caps.call("http.request", json!({"url": "x"}))?;
                    Ok(json!({"cap_said": got}))
                } else {
                    Ok(args)
                }
            })
        });

        // Orca side of the socket.
        let mut orca = orca_end;
        // 1. read Hello
        let h = read_frame(&mut orca).unwrap().unwrap();
        assert!(matches!(h, Frame::Hello { .. }));
        // 2. send Welcome
        write_frame(
            &mut orca,
            &Frame::Welcome {
                protocol: "1.0".into(),
                capabilities: vec!["http.request".into()],
            },
        )
        .unwrap();
        // 3. plain invoke → echo
        write_frame(
            &mut orca,
            &Frame::Invoke {
                id: 1,
                tool: "echo".into(),
                args: json!({"a": 1}),
            },
        )
        .unwrap();
        match read_frame(&mut orca).unwrap().unwrap() {
            Frame::Result { id, ok, value, .. } => {
                assert_eq!(id, 1);
                assert!(ok);
                assert_eq!(value, json!({"a": 1}));
            }
            f => panic!("expected Result, got {f:?}"),
        }
        // 4. invoke that triggers a capability round-trip
        write_frame(
            &mut orca,
            &Frame::Invoke {
                id: 2,
                tool: "with_cap".into(),
                args: Value::Null,
            },
        )
        .unwrap();
        match read_frame(&mut orca).unwrap().unwrap() {
            Frame::Cap { id, cap, args } => {
                assert_eq!(cap, "http.request");
                assert_eq!(args, json!({"url": "x"}));
                write_frame(
                    &mut orca,
                    &Frame::CapResult {
                        id,
                        ok: true,
                        value: json!({"status": 200}),
                        error: None,
                    },
                )
                .unwrap();
            }
            f => panic!("expected Cap, got {f:?}"),
        }
        match read_frame(&mut orca).unwrap().unwrap() {
            Frame::Result { id, ok, value, .. } => {
                assert_eq!(id, 2);
                assert!(ok);
                assert_eq!(value, json!({"cap_said": {"status": 200}}));
            }
            f => panic!("expected Result, got {f:?}"),
        }
        // 5. shutdown ends the loop cleanly
        write_frame(&mut orca, &Frame::Shutdown).unwrap();
        plugin.join().unwrap().unwrap();
    }
}
