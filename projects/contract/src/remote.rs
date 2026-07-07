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
        correlation_id: Option<String>,
    ) -> Result<serde_json::Value>;

    /// Best-effort: force-refresh the runtime snapshot (version / channel /
    /// mode / target) the controller caches for `peer`. Called by tools whose
    /// success mutates the peer's reported runtime — notably `system.update` —
    /// so the UI reflects the new state without waiting for the next sync
    /// tick. Default no-op for transports that don't maintain a runtime cache.
    async fn refresh_peer_runtime(&self, _peer: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoExec;

    #[async_trait::async_trait]
    impl RemoteExec for EchoExec {
        #[allow(clippy::disallowed_types)]
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: serde_json::Value,
            caller: Option<CallerIdentity>,
            _correlation_id: Option<String>,
        ) -> Result<serde_json::Value> {
            Ok(json!({
                "peer": peer,
                "tool": tool,
                "args": args,
                "caller_user": caller.map(|c| c.user_id),
            }))
        }
    }

    #[test]
    fn caller_identity_is_clone_and_debug() {
        let c = CallerIdentity {
            user_id: "u1".into(),
            username: "scott".into(),
            role: "admin".into(),
        };
        let d = c.clone();
        assert_eq!(d.user_id, "u1");
        assert_eq!(d.username, "scott");
        assert_eq!(d.role, "admin");
        assert!(format!("{c:?}").contains("scott"));
    }

    #[tokio::test]
    async fn exec_round_trips_caller_and_args() {
        let t = EchoExec;
        let out = t
            .exec(
                "peerA",
                "system.detail",
                json!({"x": 1}),
                Some(CallerIdentity {
                    user_id: "u1".into(),
                    username: "scott".into(),
                    role: "admin".into(),
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(out["peer"], "peerA");
        assert_eq!(out["tool"], "system.detail");
        assert_eq!(out["args"], json!({"x": 1}));
        assert_eq!(out["caller_user"], "u1");
    }

    #[tokio::test]
    async fn refresh_peer_runtime_default_is_noop_ok() {
        let t = EchoExec;
        t.refresh_peer_runtime("peerA").await.unwrap();
    }
}
