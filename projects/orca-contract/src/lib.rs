//! `orca-contract` — contract shared across every OrcaTool surface.
//!
//! Exposes the metadata traits (`OrcaToolDef`, `OrcaOp`), `OrcaError` /
//! `ErrorKind` / `OrcaResult`, `JsonAny`, the LLM protocol types
//! (`ToolCall`, `ToolDef`, `ToolResult`), and the runtime trait anchors —
//! `OrcaTool`, `ToolCtx`, `RemoteExec`.
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

mod ctx;
mod remote;
mod tool;

pub use ctx::ToolCtx;
pub use remote::RemoteExec;
pub use tool::OrcaTool;
