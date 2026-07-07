//! `orca-contract` — contract shared across every OrcaTool surface.
//!
//! Exposes the metadata traits (`OrcaToolDef`, `OrcaOp`), `OrcaError` /
//! `ErrorKind` / `OrcaResult`, `JsonAny`, the LLM protocol types
//! (`ToolCall`, `ToolDef`, `ToolResult`), and the runtime trait anchors —
//! `OrcaTool`, `ToolCtx`, `RemoteExec`.
//!
//! No inventory, no axum, no tokio — those live in `orca-dispatch`.

pub mod config;

/// Canonical hand-desugared async return for capability traits (no `async_trait`
/// macro, per the workspace rule). One definition here; every domain crate
/// (`unit`, `service`, …) re-uses it instead of redeclaring the alias.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

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

pub mod cluster_roster;
mod ctx;
mod remote;
mod tool;

pub use cluster_roster::{AggregateClusterRoster, ClusterEntry, ClusterNode, ClusterRoster};
pub use ctx::ToolCtx;
pub use remote::{CallerIdentity, RemoteExec};
pub use tool::OrcaTool;

pub mod topology;
pub use topology::TopologyClaim;

pub mod unit;
