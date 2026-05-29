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
    pub config: Arc<orca_utils::config::Config>,
    /// Ambient operator identity for this ctx. Set at `build_tool_ctx` to the
    /// host admin operator on the CLI/daemon path; overridden per-request on
    /// REST via `set_caller` when the request carries a session identity.
    /// Used to mint the signed caller token when a tool dispatches to a remote
    /// peer. `None` on unauthenticated/bootstrap paths.
    auth: Option<crate::CallerIdentity>,
    services: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl ToolCtx {
    pub fn new(config: Arc<orca_utils::config::Config>) -> Self {
        Self {
            config,
            auth: None,
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
