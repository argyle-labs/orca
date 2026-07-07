use anyhow::{Result, anyhow};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Shared context passed to every tool invocation.
///
/// Carries the global `Config` plus a type-keyed registry of abstract
/// services injected by the host. Tools whose `run` bodies need
/// server-internal behavior (agent_backend, docs, agents, etc.) fetch a
/// trait-object handle via `ctx.service::<Arc<dyn FooService>>()`
/// instead of calling server-internal modules directly.
#[derive(Clone)]
pub struct ToolCtx {
    pub config: Arc<crate::config::Config>,
    /// Ambient operator identity for this ctx. Set at `build_tool_ctx` to the
    /// host admin operator on the CLI/daemon path; overridden per-request on
    /// REST via `set_caller` when the request carries a session identity.
    /// Used to mint the signed caller token when a tool dispatches to a remote
    /// peer. `None` on unauthenticated/bootstrap paths.
    auth: Option<crate::CallerIdentity>,
    /// Target peer for this invocation. When `Some(peer)` and the tool is not
    /// `local_only`, the dispatcher (macro-emitted stanza) proxies the call
    /// via `RemoteExec` instead of running locally. Populated from the
    /// `--peer <h>` CLI flag, the `X-Orca-Peer` REST header, or an MCP
    /// envelope field. `None` runs locally. Peer routing is opt-out: tools
    /// marked `local_only = true` reject a peer target with a clear error.
    peer_target: Option<String>,
    /// Per-request correlation id, set by REST middleware from the inbound
    /// `x-correlation-id` header (or synthesized if absent). Threaded into
    /// `tracing` spans by tool handlers and propagated as the same header on
    /// outbound mesh dispatch so a single user action traces end-to-end
    /// across every host involved. `None` on CLI/MCP paths until those wire
    /// equivalent ingest points.
    correlation_id: Option<String>,
    services: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl ToolCtx {
    pub fn new(config: Arc<crate::config::Config>) -> Self {
        Self {
            config,
            auth: None,
            peer_target: None,
            correlation_id: None,
            services: HashMap::new(),
        }
    }

    /// Set the ambient operator identity. Builder-style; called once at
    /// `build_tool_ctx`.
    pub fn with_auth(mut self, auth: crate::CallerIdentity) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Replace the ambient operator identity in-place. Used by REST
    /// `http_dispatch` to swap the host-admin default for the authenticated
    /// session user before invoking a tool — that user's role is what the
    /// recipient peer will resolve from its replicated `users` table.
    pub fn set_caller(&mut self, auth: Option<crate::CallerIdentity>) {
        self.auth = auth;
    }

    /// The ambient operator identity, if one was set.
    pub fn caller(&self) -> Option<crate::CallerIdentity> {
        self.auth.clone()
    }

    /// Set the target peer in-place. Called by the CLI dispatcher when
    /// `--peer <h>` is present (and by REST/MCP middleware on equivalent
    /// per-request inputs) before the tool's `OrcaTool::run` fires.
    pub fn set_peer(&mut self, peer: Option<String>) {
        self.peer_target = peer.filter(|s| !s.trim().is_empty());
    }

    /// Builder-style peer setter, mirroring `with_auth`. Useful when
    /// constructing a one-off ctx in tests or scripted call sites.
    pub fn with_peer(mut self, peer: impl Into<String>) -> Self {
        self.set_peer(Some(peer.into()));
        self
    }

    /// The target peer for this invocation, if one was set.
    pub fn peer(&self) -> Option<&str> {
        self.peer_target.as_deref()
    }

    /// Set the per-request correlation id in-place. REST middleware calls this
    /// with the ingested (or synthesized) `x-correlation-id` value before
    /// invoking the tool.
    pub fn set_correlation_id(&mut self, cid: Option<String>) {
        self.correlation_id = cid.filter(|s| !s.trim().is_empty());
    }

    /// The per-request correlation id, if one was set.
    pub fn correlation_id(&self) -> Option<&str> {
        self.correlation_id.as_deref()
    }

    /// Insert a service handle. `T` is typically `Arc<dyn FooService>` —
    /// the trait-object Arc itself is `Sized + 'static + Send + Sync` and
    /// `Clone`, which is everything the registry needs.
    ///
    /// Coerce the concrete impl at the call site:
    /// ```ignore
    /// let svc: Arc<dyn FooService> = Arc::new(ConcreteFoo);
    /// ctx.register_service(svc);
    /// ```
    pub fn register_service<T: Clone + Send + Sync + 'static>(&mut self, svc: T) -> &mut Self {
        self.services.insert(TypeId::of::<T>(), Arc::new(svc));
        self
    }

    /// Fetch a previously-registered service handle. Errors when nothing is
    /// registered for `T` — every tool that needs a service must have its
    /// host wire one in at startup.
    pub fn service<T: Clone + Send + Sync + 'static>(&self) -> Result<T> {
        let any = self
            .services
            .get(&TypeId::of::<T>())
            .ok_or_else(|| anyhow!("no service registered for {}", std::any::type_name::<T>()))?;
        any.downcast_ref::<T>()
            .cloned()
            .ok_or_else(|| anyhow!("service downcast failed for {}", std::any::type_name::<T>()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Model};
    use std::path::PathBuf;

    fn cfg() -> Arc<Config> {
        Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Default::default(),
        })
    }

    fn id(user: &str) -> crate::CallerIdentity {
        crate::CallerIdentity {
            user_id: format!("u_{user}"),
            username: user.into(),
            role: "admin".into(),
        }
    }

    #[test]
    fn clone_preserves_services_and_independent_caller_override() {
        // Shared base ctx with one service + the host-admin ambient identity.
        let mut base = ToolCtx::new(cfg()).with_auth(id("host_admin"));
        base.register_service::<Arc<str>>("svc_value".into());

        // REST hot path: clone the shared ctx and swap the caller for this
        // request's authenticated session user.
        let mut per_req = base.clone();
        per_req.set_caller(Some(id("alice")));

        assert_eq!(per_req.caller().unwrap().username, "alice");
        assert_eq!(base.caller().unwrap().username, "host_admin");

        // Services survive the clone — Arc storage shares without copying.
        let from_base: Arc<str> = base.service().unwrap();
        let from_clone: Arc<str> = per_req.service().unwrap();
        assert_eq!(&*from_base, "svc_value");
        assert_eq!(&*from_clone, "svc_value");
        assert!(Arc::ptr_eq(&from_base, &from_clone));
    }

    #[test]
    fn set_caller_clears_with_none() {
        let mut ctx = ToolCtx::new(cfg()).with_auth(id("host_admin"));
        ctx.set_caller(None);
        assert!(ctx.caller().is_none());
    }
}
