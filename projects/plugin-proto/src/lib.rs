//! Out-of-process plugin wire protocol.
//!
//! A plugin is a child process of the orca daemon connected over a Unix-domain
//! socket. This crate defines the frames that cross that socket and a
//! length-prefixed JSON codec for them. See `docs/OUT-OF-PROCESS-PLUGINS.md`.
//!
//! Two directions share one connection:
//!
//! * **orca → plugin**: [`Frame::Invoke`] a tool, [`Frame::CapResult`] answer a
//!   capability call, [`Frame::Welcome`]/[`Frame::Shutdown`] lifecycle.
//! * **plugin → orca**: [`Frame::Hello`] handshake, [`Frame::Result`] answer an
//!   invoke, [`Frame::Cap`] request a host capability, [`Frame::Log`].
//!
//! `id` correlates request↔response *within each direction* (monotonic per
//! direction). Tool args/results and capability payloads are carried as
//! [`serde_json::Value`] — the transport-dynamic boundary, exactly as today's
//! FFI passes them as JSON strings; per-tool typing happens above this layer
//! against the declared schemas.

// The tool `args`/`value` and capability payloads are the transport-dynamic
// boundary: their concrete type is per-tool and is validated ABOVE this layer
// against each tool's declared JSON Schema — exactly as today's FFI carries them
// as an opaque `args_json` string. Modeling them as `Value` here (rather than
// double-encoding JSON inside a String) keeps the wire clean; per clippy.toml
// this is the sanctioned use of the escape hatch, scoped to this transport crate.
#![allow(clippy::disallowed_types)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};

pub mod session;
pub use session::{Caps, serve};

/// Wire-protocol version. Compatibility is negotiated at the handshake by
/// MAJOR: a plugin and daemon interoperate iff their protocol majors match.
/// This replaces the compiled `abi_stable` layout/version gate — a plugin built
/// against protocol `1.x` connects to any daemon on `1.y`.
pub const PROTOCOL_VERSION: &str = "1.0";

/// Largest frame we will read, guarding against a corrupt/hostile length
/// prefix. Tool payloads are small JSON; 64 MiB is generous.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// One tool the plugin contributes. The JSON shape matches the existing
/// `ToolDef` that already crosses the FFI as a string, so porting is lossless.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema (Draft 2020-12) for the tool's args.
    pub input_schema: Value,
    /// JSON Schema for the tool's output.
    pub output_schema: Value,
}

/// A single frame on the wire. `kind` tags the variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    /// plugin → orca, first frame after connect. Declares identity + surface.
    Hello {
        /// Wire-protocol semver the plugin was built against.
        protocol: String,
        /// Plugin name (== `target_software`; the catalog/install key).
        plugin: String,
        /// Plugin's own semantic version.
        version: String,
        #[serde(default)]
        manifest: Vec<ToolDef>,
        /// The plugin's domain backends (topology / unit / host_facts / …),
        /// each element the **verbatim** backend-def JSON the daemon parses into
        /// its own `BackendDef`. Carried as opaque `Value` — not a proto struct —
        /// so every field of the daemon's richer shape (kind / runtime /
        /// endpoint / capabilities / …) survives the wire losslessly, exactly as
        /// it already does across the cdylib FFI as a JSON string.
        #[serde(default)]
        backends: Vec<Value>,
        /// Declared SQL schema, verbatim (applied by the daemon). `null` = none.
        #[serde(default, skip_serializing_if = "Value::is_null")]
        schema: Value,
    },
    /// orca → plugin, accepts the handshake. Lists capabilities the daemon offers.
    Welcome {
        protocol: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    /// orca → plugin: invoke a tool. `id` is the daemon's request id.
    Invoke { id: u64, tool: String, args: Value },
    /// plugin → orca: result of an [`Frame::Invoke`] with the matching `id`.
    Result {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// plugin → orca: call a host capability. `id` is the plugin's request id.
    Cap { id: u64, cap: String, args: Value },
    /// orca → plugin: result of a [`Frame::Cap`] with the matching `id`.
    CapResult {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// plugin → orca: structured log line (fire-and-forget, no `id`).
    Log {
        level: String,
        msg: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        fields: Value,
    },
    /// orca → plugin: begin graceful shutdown.
    Shutdown,
}

/// Whether two protocol version strings interoperate — MAJOR must match.
/// Missing/malformed versions are treated as incompatible (fail closed).
pub fn protocol_compatible(a: &str, b: &str) -> bool {
    fn major(v: &str) -> Option<u64> {
        v.split('.').next()?.trim().parse().ok()
    }
    match (major(a), major(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("frame exceeds MAX_FRAME_BYTES ({len} > {max})")]
    TooLarge { len: u32, max: u32 },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Handshake / session-level protocol violation (unexpected frame, version
    /// mismatch, peer closed mid-exchange).
    #[error("protocol: {0}")]
    Handshake(String),
}

/// Serialize a frame to its on-wire bytes: `u32` LE length prefix + JSON body.
pub fn encode(frame: &Frame) -> Result<Vec<u8>, ProtoError> {
    let body = serde_json::to_vec(frame)?;
    let len = u32::try_from(body.len()).map_err(|_| ProtoError::TooLarge {
        len: u32::MAX,
        max: MAX_FRAME_BYTES,
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(ProtoError::TooLarge {
            len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Write one frame to a blocking writer.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> Result<(), ProtoError> {
    let bytes = encode(frame)?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Read one frame from a blocking reader. Returns `Ok(None)` on a clean EOF at
/// a frame boundary (peer closed) so callers can end their loop without error.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<Frame>, ProtoError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ProtoError::TooLarge {
            len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    let frame = serde_json::from_slice(&body)?;
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(f: &Frame) {
        let bytes = encode(f).unwrap();
        // length prefix is correct
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        assert_eq!(len as usize, bytes.len() - 4);
        let mut cur = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cur).unwrap().unwrap();
        assert_eq!(&got, f);
    }

    #[test]
    fn frames_roundtrip() {
        roundtrip(&Frame::Hello {
            protocol: "1.0".into(),
            plugin: "proxmox".into(),
            version: "0.1.1-rc.3".into(),
            manifest: vec![ToolDef {
                name: "proxmox.get_facts".into(),
                description: "host facts".into(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
            }],
            backends: vec![json!({
                "domain": "host_facts",
                "name": "proxmox",
                "invoke_prefix": "proxmox",
                "kind": "",
                "runtime": "",
                "endpoint": "",
            })],
            schema: Value::Null,
        });
        roundtrip(&Frame::Invoke {
            id: 42,
            tool: "proxmox.get_facts".into(),
            args: json!({"node": "frigg"}),
        });
        roundtrip(&Frame::Result {
            id: 42,
            ok: true,
            value: json!({"cluster": "yggdrasil"}),
            error: None,
        });
        roundtrip(&Frame::Cap {
            id: 1,
            cap: "http.request".into(),
            args: json!({"method": "GET", "url": "https://x"}),
        });
        roundtrip(&Frame::Shutdown);
    }

    #[test]
    fn clean_eof_is_none() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut empty).unwrap().is_none());
    }

    #[test]
    fn oversize_length_rejected() {
        let mut bytes = (MAX_FRAME_BYTES + 1).to_le_bytes().to_vec();
        bytes.push(0);
        let mut cur = std::io::Cursor::new(bytes);
        assert!(matches!(
            read_frame(&mut cur),
            Err(ProtoError::TooLarge { .. })
        ));
    }

    #[test]
    fn protocol_compat_by_major() {
        assert!(protocol_compatible("1.0", "1.7"));
        assert!(protocol_compatible("1.0", "1.0"));
        assert!(!protocol_compatible("1.0", "2.0"));
        assert!(!protocol_compatible("1.0", "garbage"));
    }
}
