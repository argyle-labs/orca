//! `orca-contract` — wasm-safe contract shared across every OrcaTool surface.
//!
//! Default feature: wasm-safe metadata (`OrcaToolDef`, `OrcaOp`), `OrcaError` /
//! `ErrorKind` / `OrcaResult`, `JsonAny`, and the LLM protocol types
//! (`ToolCall`, `ToolDef`, `ToolResult`).
//! `native` feature: native-only trait anchors — `OrcaTool`, `ToolCtx`,
//! `RemoteExec`. Brings `orca-utils` (for `Config`), `anyhow`, `async-trait`.
//!
//! No inventory, no axum, no tokio — those live in `orca-dispatch`.

mod def;
pub use def::{OrcaOp, OrcaToolDef};

mod error;
pub use error::{ErrorKind, OrcaError, OrcaResult};

pub mod json_any;
// The re-export itself triggers the disallowed-type lint workspace-wide;
// defining + exposing the type is exactly what this crate exists to do.
#[allow(clippy::disallowed_types)]
pub use json_any::JsonAny;

mod types;
pub use types::{ToolCall, ToolDef, ToolResult};

#[cfg(feature = "native")]
mod ctx;
#[cfg(feature = "native")]
mod remote;
#[cfg(feature = "native")]
mod tool;

#[cfg(feature = "native")]
pub use ctx::ToolCtx;
#[cfg(feature = "native")]
pub use remote::RemoteExec;
#[cfg(feature = "native")]
pub use tool::OrcaTool;
