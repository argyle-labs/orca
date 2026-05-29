//! Transport trait for dispatching a tool call to a paired pod peer.
//!
//! Lives at the `native` layer (not `cli`) because the macro-emitted
//! `peer_dispatch` proxy stanza needs to resolve it from any tool body — not
//! just the CLI surface. The server registers an adapter
//! (`PodRemoteExec` in `fleet::pod`) that delegates to its `PodService`.

use anyhow::Result;

/// Identity of the local operator on whose behalf a remote call is made.
/// The transport mints a signed caller token from this so the recipient can
/// derive the effective role from its own replicated `users` table. On the
/// CLI/daemon path this is the host admin operator; on REST it is the
/// authenticated session user.
#[derive(Debug, Clone)]
pub struct CallerIdentity {
    pub user_id: String,
    pub username: String,
    pub role: String,
}

#[async_trait::async_trait]
pub trait RemoteExec: Send + Sync {
    /// Dispatch one tool call to `peer` over the host's mesh transport.
    /// Args/output are JSON-RPC wire payloads; callers deserialize the typed
    /// `OrcaToolDef::Output` immediately on receipt so opaque values never
    /// reach user code. `caller` is the local operator's identity — `Some(..)`
    /// from a CLI/REST call that already passed local auth (the transport mints
    /// a signed token from it), `None` for unauthenticated paths.
    #[allow(clippy::disallowed_types)]
    async fn exec(
        &self,
        peer: &str,
        tool: &str,
        args: serde_json::Value,
        caller: Option<CallerIdentity>,
    ) -> Result<serde_json::Value>;
}
