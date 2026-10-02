//! Transport trait for dispatching a tool call to a paired system in the mesh.
//!
//! Lives at the `native` layer (not `cli`) because the macro-emitted
//! `peer_dispatch` proxy stanza needs to resolve it from any tool body — not
//! just the CLI surface. The server registers an adapter
//! (`MeshRemoteExec` in `system::mesh`) that delegates to its `MeshService`.

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
    /// The `can_mutate` capability, carried WITH the identity rather than
    /// alongside it. The execute gate authorizes at dispatch, where the HTTP
    /// request extensions that used to hold this are long gone; an identity
    /// that cannot answer "may this caller mutate?" forces the gate to guess,
    /// and a guessing authorization check is not one.
    pub can_mutate: bool,
}

/// One system in the mesh, as the transport knows it.
///
/// The minimum a domain crate needs to address a resource it does not own:
/// who is out there, and which one is us. Deliberately NOT the rich peer row —
/// a domain asking "who could own this container?" has no business reading
/// mesh membership state, and coupling it to that shape would make every
/// domain a mesh consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRef {
    /// Stable machine id. The thing a caller addresses by (#647) — never the
    /// display name, which is mutable and can duplicate.
    pub id: String,
    /// Operator-facing display name. For rendering only: resolving BY name is
    /// what #647 removes.
    pub name: String,
    /// True for the system this call is running on. It has no roster row of
    /// its own, and dialing it over the mesh would be a host calling itself.
    pub is_local: bool,
}

#[async_trait::async_trait]
pub trait RemoteExec: Send + Sync {
    /// Every system in the mesh, including this one.
    ///
    /// Exists so a domain crate can resolve "which system owns this resource?"
    /// without depending on `system` for the roster. That dependency is not
    /// available to it in any case — `system` depends on the domain crates,
    /// not the reverse — so without this a domain's only way to reach another
    /// host is for the CALLER to name one, which is exactly the host selection
    /// #647 removes.
    ///
    /// Default empty: a transport with no mesh (a plugin subprocess, a test
    /// double) reports no peers, and an owner search over no peers finds
    /// nothing rather than inventing somewhere to look.
    async fn peers(&self) -> Result<Vec<PeerRef>> {
        Ok(Vec::new())
    }

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
    #[cfg(feature = "in-process")]
    use serde_json::json;

    // Exercised only by the async round-trip tests, which are owned by the
    // `in-process` profile (the one that links tokio for `#[tokio::test]`).
    #[cfg(feature = "in-process")]
    struct EchoExec;

    #[cfg(feature = "in-process")]
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
            can_mutate: false,
        };
        let d = c.clone();
        assert_eq!(d.user_id, "u1");
        assert_eq!(d.username, "scott");
        assert_eq!(d.role, "admin");
        assert!(format!("{c:?}").contains("scott"));
    }

    #[cfg(feature = "in-process")]
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
                    can_mutate: false,
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

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn refresh_peer_runtime_default_is_noop_ok() {
        let t = EchoExec;
        t.refresh_peer_runtime("peerA").await.unwrap();
    }
}
