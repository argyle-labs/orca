//! Transport trait for dispatching a tool call to a paired pod peer.
//!
//! Lives at the `native` layer (not `cli`) because the macro-emitted
//! `peer_dispatch` proxy stanza needs to resolve it from any tool body — not
//! just the CLI surface. The server registers an adapter
//! (`PodRemoteExec` in `fleet::pod`) that delegates to its `PodService`.

use anyhow::Result;

#[async_trait::async_trait]
pub trait RemoteExec: Send + Sync {
    /// Dispatch one tool call to `peer` over the host's mesh transport.
    /// Args/output are JSON-RPC wire payloads; callers deserialize the typed
    /// `OrcaToolDef::Output` immediately on receipt so opaque values never
    /// reach user code.
    #[allow(clippy::disallowed_types)]
    async fn exec(
        &self,
        peer: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value>;
}
