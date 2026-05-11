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
/// instead of calling server-internal modules directly — keeps tool
/// definitions wasm-safe.
pub struct ToolCtx {
    pub config: Arc<crate::config::Config>,
    services: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl ToolCtx {
    pub fn new(config: Arc<crate::config::Config>) -> Self {
        Self {
            config,
            services: HashMap::new(),
        }
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
        self.services.insert(TypeId::of::<T>(), Box::new(svc));
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
