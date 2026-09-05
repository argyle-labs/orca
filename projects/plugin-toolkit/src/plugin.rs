//! Unified multi-facet plugin entrypoint: the [`Plugin`] builder.
//!
//! A plugin is not one domain. Audiobookshelf is a deployable **service** AND a
//! media **server**; lazylibrarian is a **service** AND a media **acquirer**. The
//! per-domain `serve_*_plugin!` macros each emit their own `fn main` advertising
//! exactly one domain, so historically a plugin could only be one thing per
//! binary. That is an accident of how the macros grew, not a property of the wire
//! — [`PluginSpec`](crate::serve::PluginSpec) already carries `backends_json` as a
//! JSON *array* and a single `backend_dispatch`, so one binary can advertise any
//! mix of domain backends.
//!
//! [`Plugin`] is that unified seam. It collects N facets across domains, derives
//! one combined [`BackendDef`] array (via the typed `*_backend_def` builders +
//! the single [`backends_json`](crate::backend_def::backends_json)), and composes
//! each facet's dispatcher into one — so a facet's proxied ops route to that
//! facet's backend, and anything unmatched falls through to the `#[orca_tool]`
//! surface.
//!
//! ```rust,ignore
//! fn main() -> anyhow::Result<()> {
//!     plugin_toolkit::instrument::bootstrap!();
//!     plugin_toolkit::plugin::Plugin::named("audiobookshelf")
//!         .version(env!("CARGO_PKG_VERSION"))
//!         .service(AudiobookshelfBackend::new("audiobookshelf"))
//!         .media(AbsMedia::served(MediaType::Audiobooks))
//!         .media(AbsMedia::served(MediaType::Podcasts))
//!         .serve()
//! }
//! ```
#![cfg(all(feature = "tools", feature = "db"))]
// The builder composes per-facet dispatchers that carry tool args/results as JSON
// `Value` across the socket — the same transport-dynamic boundary serve() lives
// on. Sanctioned escape hatch, scoped to this seam.
#![allow(clippy::disallowed_types)]

use serde_json::Value;

use crate::abi::{BackendDef, TableDef};
use crate::serve::{BackendDispatch, PluginSpec, serve};

/// One facet's proxied-op router: strips its own `{invoke_prefix}` and answers
/// the bare op, or returns `None` so the next facet — or the `#[orca_tool]`
/// surface — gets a shot. Boxed so facets capturing different backend types
/// compose into one homogeneous list.
type Dispatcher = Box<dyn Fn(&str, Value) -> Option<Result<Value, Value>>>;

/// A multi-facet subprocess plugin. Build it up with facet methods, then
/// [`serve`](Plugin::serve). Each dispatch-backed facet (`service`/`storage`/
/// `media`) both advertises its [`BackendDef`] and registers a dispatcher;
/// tool-backed backends (`tool_backend`) advertise a def whose ops are served by
/// the plugin's `#[orca_tool]`s.
pub struct Plugin {
    name: String,
    version: String,
    prefixes: Vec<String>,
    defs: Vec<BackendDef>,
    dispatchers: Vec<Dispatcher>,
    schema_json: String,
}

impl Plugin {
    /// Start building a plugin named `name` (== its `target_software`). Version
    /// defaults to `"0"`; set it with [`version`](Plugin::version) — plugins pass
    /// `env!("CARGO_PKG_VERSION")` so the handshake reports the real build.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "0".to_string(),
            prefixes: Vec::new(),
            defs: Vec::new(),
            dispatchers: Vec::new(),
            schema_json: crate::backend_def::EMPTY_SCHEMAS.to_string(),
        }
    }

    /// Set the plugin's semantic version (typically `env!("CARGO_PKG_VERSION")`).
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Declare tool namespaces this plugin owns (each trailing-dot included, e.g.
    /// `"audiobookshelf."`). The manifest is derived from the linked
    /// `#[orca_tool]` inventory filtered to these prefixes. Call once with all
    /// prefixes, or repeatedly — they accumulate.
    #[must_use]
    pub fn tools<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.prefixes.extend(prefixes.into_iter().map(Into::into));
        self
    }

    /// Declare plugin-scoped SQL tables (materialized by core at load). Replaces
    /// the default empty declaration.
    #[must_use]
    pub fn schemas(mut self, namespace: &str, tables: Vec<TableDef>) -> Self {
        self.schema_json = crate::backend_def::schemas_json(namespace, tables);
        self
    }

    /// Set the plugin-scoped SQL schema from a pre-built `schemas_json` string —
    /// the escape hatch for a plugin whose schema helper already returns the
    /// serialized form (e.g. `crate::backend_def::schemas_json(ns, tables)`).
    /// Prefer [`schemas`](Plugin::schemas) when you hold the `TableDef`s directly.
    #[must_use]
    pub fn schema_json(mut self, schema_json: impl Into<String>) -> Self {
        self.schema_json = schema_json.into();
        self
    }

    /// Register a **service** facet: this plugin deploys/backs-up/serves an app.
    #[must_use]
    pub fn service<B: crate::service::ServiceBackend + 'static>(mut self, backend: B) -> Self {
        let prefix = format!("service.__backend.{}", backend.provider());
        self.defs
            .push(crate::backend_def::service_backend_def(&backend, &prefix));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(crate::service::dispatch_op(
                    &backend, op, args,
                )))
            }));
        self
    }

    /// Register a **storage** facet: this plugin realizes/lists a storage backend.
    #[must_use]
    pub fn storage<B: crate::storage::StorageBackend + 'static>(mut self, backend: B) -> Self {
        let prefix = format!("storage.__backend.{}", backend.name());
        self.defs
            .push(crate::backend_def::storage_backend_def(&backend, &prefix));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(crate::storage::dispatch_op(
                    &backend, op, args,
                )))
            }));
        self
    }

    /// Register a **media** facet: this plugin acquires (`downloaded_by`) and/or
    /// serves (`served_by`) one media type. Register once per media type the app
    /// handles — the prefix carries the type, so `audiobooks` and `podcasts`
    /// facets of one server never collide.
    #[must_use]
    pub fn media<B: crate::media::MediaBackend + 'static>(mut self, backend: B) -> Self {
        let prefix = format!(
            "media.__backend.{}.{}",
            backend.name(),
            backend.media_type().as_str()
        );
        self.defs
            .push(crate::backend_def::media_backend_def(&backend, &prefix));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(crate::media::dispatch_op(
                    &backend, op, args,
                )))
            }));
        self
    }

    /// Register a **unit** facet: this plugin manages declarative units (a
    /// container-compose stack, an LXC guest, …). The plugin implements the typed
    /// [`UnitProvider`](crate::contract::unit::UnitProvider); the builder emits the
    /// prefix router + `dispatch_op` glue — no hand-written op strings.
    #[must_use]
    pub fn unit<P: crate::contract::unit::UnitProvider + 'static>(mut self, provider: P) -> Self {
        let prefix = format!("unit.__backend.{}", provider.name());
        self.defs
            .push(crate::backend_def::unit_backend_def(&provider, &prefix));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(
                    crate::contract::unit::dispatch_op(&provider, op, args),
                ))
            }));
        self
    }

    /// Register a **replication** facet: this plugin observes the sync health of a
    /// replication relationship (the mount-converge failover gate reads it). The
    /// plugin implements the typed
    /// [`ReplicationStatusProvider`](crate::storage::replication_status::ReplicationStatusProvider);
    /// the builder emits the router + `dispatch_op` glue.
    #[must_use]
    pub fn replication<P>(mut self, provider: P) -> Self
    where
        P: crate::storage::replication_status::ReplicationStatusProvider + 'static,
    {
        let prefix = format!("replication.__backend.{}", provider.name());
        self.defs.push(crate::backend_def::replication_backend_def(
            provider.name(),
            &prefix,
        ));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(
                    crate::storage::replication_status::dispatch_op(&provider, op, args),
                ))
            }));
        self
    }

    /// Register a **topology** facet: this plugin reports [`TopologyClaim`]s (one
    /// per colocated child workload) for fleet parent-host inference. The plugin
    /// implements the typed
    /// [`TopologyCollector`](crate::contract::topology::TopologyCollector); the
    /// builder emits the router + `dispatch_op` glue.
    #[must_use]
    pub fn topology<C>(mut self, collector: C) -> Self
    where
        C: crate::contract::topology::TopologyCollector + 'static,
    {
        let prefix = format!("topology.__backend.{}", collector.name());
        self.defs.push(crate::backend_def::topology_backend_def(
            collector.name(),
            &prefix,
        ));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(
                    crate::contract::topology::dispatch_op(&collector, op, args),
                ))
            }));
        self
    }

    /// Register a **container_runtime** facet: this plugin drives a container
    /// engine (docker, lxc, …). The plugin implements the typed
    /// [`RuntimeAdapter`](crate::containers::RuntimeAdapter); the builder emits
    /// the router + `dispatch_op` glue.
    #[cfg(feature = "containers")]
    #[must_use]
    pub fn container_runtime<A: crate::containers::RuntimeAdapter + 'static>(
        mut self,
        adapter: A,
    ) -> Self {
        let kind = adapter.kind().as_str();
        let prefix = format!("container_runtime.__backend.{kind}");
        self.defs.push(BackendDef {
            domain: "container_runtime".to_string(),
            name: kind.to_string(),
            kind: kind.to_string(),
            invoke_prefix: prefix.clone(),
            ..Default::default()
        });
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(crate::containers::dispatch_op(
                    &adapter, op, args,
                )))
            }));
        self
    }

    /// Register a **deploy_target** facet: this plugin realizes a deploy target
    /// (a Proxmox guest, an LXC host, …). The plugin implements the typed
    /// [`DeployTarget`](crate::deploy_target::DeployTarget).
    #[must_use]
    pub fn deploy_target<T: crate::deploy_target::DeployTarget + 'static>(
        mut self,
        target: T,
    ) -> Self {
        let prefix = format!("deploy_target.__backend.{}", target.host());
        self.defs
            .push(crate::backend_def::deploy_backend_def(&target, &prefix));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(crate::deploy_target::dispatch_op(
                    &target, op, args,
                )))
            }));
        self
    }

    /// Register a **subprocess_env** facet: this plugin injects env vars into the
    /// subprocesses orca spawns (e.g. `DOCKER_HOST`). Implements the typed
    /// [`EnvProvider`](crate::contract::subprocess_env::EnvProvider).
    #[must_use]
    pub fn subprocess_env<P: crate::contract::subprocess_env::EnvProvider + 'static>(
        mut self,
        provider: P,
    ) -> Self {
        let prefix = format!("subprocess_env.__backend.{}", provider.name());
        self.defs.push(BackendDef {
            domain: "subprocess_env".to_string(),
            name: provider.name().to_string(),
            invoke_prefix: prefix.clone(),
            ..Default::default()
        });
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(
                    crate::contract::subprocess_env::dispatch_op(&provider, op, args),
                ))
            }));
        self
    }

    /// Register a **secrets** facet: this plugin resolves secret references
    /// (1Password, Vaultwarden, …). Implements the typed
    /// [`SecretsBackend`](crate::contract::secrets_backend::SecretsBackend).
    #[must_use]
    pub fn secrets<P: crate::contract::secrets_backend::SecretsBackend + 'static>(
        mut self,
        provider: P,
    ) -> Self {
        let prefix = format!("secrets_backend.__backend.{}", provider.name());
        self.defs.push(crate::backend_def::secrets_backend_def(
            provider.name(),
            &prefix,
        ));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(
                    crate::contract::secrets_backend::dispatch_op(&provider, op, args),
                ))
            }));
        self
    }

    /// Register a **ups** facet: this plugin reports/manages UPS power state.
    /// Implements the typed [`UpsProvider`](crate::contract::ups::UpsProvider).
    #[must_use]
    pub fn ups<P: crate::contract::ups::UpsProvider + 'static>(mut self, provider: P) -> Self {
        let prefix = format!("ups.__backend.{}", provider.name());
        self.defs.push(BackendDef {
            domain: "ups".to_string(),
            name: provider.name().to_string(),
            invoke_prefix: prefix.clone(),
            ..Default::default()
        });
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(crate::contract::ups::dispatch_op(
                    &provider, op, args,
                )))
            }));
        self
    }

    /// Register a **diagnostics** facet: this plugin diagnoses/repairs an
    /// external subsystem. Implements the typed
    /// [`DiagnosticsProvider`](crate::contract::diagnostics::DiagnosticsProvider).
    #[must_use]
    pub fn diagnostics<P: crate::contract::diagnostics::DiagnosticsProvider + 'static>(
        mut self,
        provider: P,
    ) -> Self {
        let prefix = format!("diagnostics.__backend.{}", provider.name());
        self.defs.push(BackendDef {
            domain: "diagnostics".to_string(),
            name: provider.name().to_string(),
            invoke_prefix: prefix.clone(),
            ..Default::default()
        });
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(
                    crate::contract::diagnostics::dispatch_op(&provider, op, args),
                ))
            }));
        self
    }

    /// Register a **host_facts** facet: this plugin contributes host inventory
    /// facts. Implements the typed
    /// [`HostFactsProvider`](crate::contract::host_facts::HostFactsProvider).
    #[must_use]
    pub fn host_facts<P: crate::contract::host_facts::HostFactsProvider + 'static>(
        mut self,
        provider: P,
    ) -> Self {
        let prefix = format!("host_facts.__backend.{}", provider.name());
        self.defs.push(crate::backend_def::host_facts_backend_def(
            provider.name(),
            &prefix,
        ));
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let op = tool
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                Some(crate::reactor::block_on(
                    crate::contract::host_facts::dispatch_op(&provider, op, args),
                ))
            }));
        self
    }

    /// Register a **notify** facet (dynamic backend set): this plugin serves N
    /// notification endpoints (one per DB row). Implements the typed
    /// [`NotifyProvider`](crate::notify::NotifyProvider) — the builder advertises
    /// one def per endpoint and routes `notify.__backend.<endpoint>.emit` to the
    /// resolved typed [`Backend`](crate::notify::Backend).
    #[cfg(feature = "notify")]
    #[must_use]
    pub fn notify<P: crate::notify::NotifyProvider + 'static>(mut self, provider: P) -> Self {
        for ep in provider.endpoints() {
            self.defs.push(BackendDef {
                domain: "notifications".to_string(),
                name: ep.name.clone(),
                endpoint: ep.base_url.clone(),
                capabilities: vec!["emit".to_string()],
                invoke_prefix: format!("notify.__backend.{}", ep.name),
                ..Default::default()
            });
        }
        self.dispatchers
            .push(Box::new(move |tool: &str, args: Value| {
                let rest = tool.strip_prefix("notify.__backend.")?;
                let (endpoint, op) = rest.rsplit_once('.')?;
                Some(crate::reactor::block_on(crate::notify::dispatch_op(
                    &provider, endpoint, op, args,
                )))
            }));
        self
    }

    /// Register an **agents** facet: contribute agents, slash-commands, hooks,
    /// skills, and prompt-fragments to orca core's roster. ANY plugin may add its
    /// own agents this way — the `agents` plugin is just the baseline roster, not
    /// a privileged special case. Call it repeatedly to push several providers.
    ///
    /// The builder advertises the `domain = "agents"` trigger backend; the
    /// `agents.register` capability needs a live cap sink, which only exists while
    /// the plugin is servicing an `Invoke`, so the push happens the first time
    /// orca drives the backend (idempotent thereafter).
    #[must_use]
    pub fn agents(mut self, registration: crate::abi::AgentRegistration) -> Self {
        let prefix = format!("agents.__backend.{}", registration.name);
        self.defs.push(BackendDef {
            domain: "agents".to_string(),
            name: registration.name.clone(),
            invoke_prefix: prefix.clone(),
            ..Default::default()
        });
        let registered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.dispatchers
            .push(Box::new(move |tool: &str, _args: Value| {
                tool.strip_prefix(&prefix)
                    .and_then(|r| r.strip_prefix('.'))?;
                if !registered.load(std::sync::atomic::Ordering::Acquire) {
                    match crate::agents::register(registration.clone()) {
                        Ok(()) => registered.store(true, std::sync::atomic::Ordering::Release),
                        Err(e) => {
                            return Some(Err(Value::String(format!(
                                "agents.register failed: {e}"
                            ))));
                        }
                    }
                }
                Some(Ok(serde_json::json!([])))
            }));
        self
    }

    /// Advertise a domain backend whose proxied ops are served by the plugin's
    /// own `#[orca_tool]` surface (service_identity) — def only, no dispatcher.
    /// Build the def with the matching `crate::backend_def::*_backend_def` helper.
    #[must_use]
    pub fn tool_backend(mut self, def: BackendDef) -> Self {
        self.defs.push(def);
        self
    }

    /// Escape hatch: advertise a backend with a custom dispatcher, for a domain
    /// the typed facet methods don't cover yet (deploy / replication / backup).
    /// `dispatcher` must strip its own `{invoke_prefix}` and answer the bare op,
    /// or return `None` to fall through.
    #[must_use]
    pub fn backend(mut self, def: BackendDef, dispatcher: Dispatcher) -> Self {
        self.defs.push(def);
        self.dispatchers.push(dispatcher);
        self
    }

    /// Serve until orca shuts the plugin down. Composes every facet's dispatcher
    /// into the single [`PluginSpec::backend_dispatch`] (first match wins; a miss
    /// falls through to `#[orca_tool]` dispatch) and serializes all facet defs
    /// into one `backends_json`.
    pub fn serve(self) -> crate::anyhow::Result<()> {
        let dispatchers = self.dispatchers;
        let backend_dispatch: Option<BackendDispatch> = if dispatchers.is_empty() {
            None
        } else {
            Some(Box::new(move |tool: &str, args: Value| {
                for d in &dispatchers {
                    if let Some(res) = d(tool, args.clone()) {
                        return Some(res);
                    }
                }
                None
            }))
        };
        serve(PluginSpec {
            name: self.name,
            version: self.version,
            prefixes: self.prefixes,
            backends_json: crate::backend_def::backends_json(self.defs),
            schema_json: self.schema_json,
            backend_dispatch,
        })
    }

    /// Build the [`PluginSpec`] without serving — the composed dispatch + merged
    /// backends. Split out so a test can drive facet routing over an in-memory
    /// socket pair (see [`crate::serve::serve_on`]).
    pub fn into_spec(self) -> PluginSpec {
        let dispatchers = self.dispatchers;
        let backend_dispatch: Option<BackendDispatch> = if dispatchers.is_empty() {
            None
        } else {
            Some(Box::new(move |tool: &str, args: Value| {
                for d in &dispatchers {
                    if let Some(res) = d(tool, args.clone()) {
                        return Some(res);
                    }
                }
                None
            }))
        };
        PluginSpec {
            name: self.name,
            version: self.version,
            prefixes: self.prefixes,
            backends_json: crate::backend_def::backends_json(self.defs),
            schema_json: self.schema_json,
            backend_dispatch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Capability, MediaBackend, MediaError, MediaType, MediaUrl};
    use crate::service::{Runtime, ServiceBackend, ServiceCapability};

    // A media served_by backend for one type, answering `url`.
    struct DemoMedia {
        media_type: MediaType,
    }
    #[crate::orca_async]
    impl MediaBackend for DemoMedia {
        fn name(&self) -> &str {
            "audiobookshelf"
        }
        fn media_type(&self) -> MediaType {
            self.media_type
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::ServedBy, Capability::Url]
        }
        fn endpoint(&self) -> String {
            "http://abs:13378".into()
        }
        async fn url(&self) -> std::result::Result<MediaUrl, MediaError> {
            Ok(MediaUrl {
                primary: "http://abs:13378".into(),
                alternates: vec![],
            })
        }
    }

    struct DemoService;
    impl ServiceBackend for DemoService {
        fn provider(&self) -> &str {
            "audiobookshelf"
        }
        fn runtimes(&self) -> Vec<Runtime> {
            vec![Runtime::Docker]
        }
        fn default_port(&self) -> u16 {
            13378
        }
        fn capabilities(&self) -> Vec<ServiceCapability> {
            vec![ServiceCapability::Deploy]
        }
        fn endpoint(&self) -> String {
            "http://abs:13378".into()
        }
    }

    #[test]
    fn one_plugin_advertises_service_and_two_media_facets() {
        let spec = Plugin::named("audiobookshelf")
            .version("1.2.3")
            .service(DemoService)
            .media(DemoMedia {
                media_type: MediaType::Audiobooks,
            })
            .media(DemoMedia {
                media_type: MediaType::Podcasts,
            })
            .into_spec();

        // All three facets ride ONE backends array.
        let defs: Vec<BackendDef> = serde_json::from_str(&spec.backends_json).unwrap();
        assert_eq!(defs.len(), 3);
        let domains: Vec<&str> = defs.iter().map(|d| d.domain.as_str()).collect();
        assert!(domains.contains(&"service"));
        assert_eq!(domains.iter().filter(|d| **d == "media").count(), 2);
        // Media facets get distinct, type-qualified invoke prefixes (no collision).
        let media_prefixes: Vec<&str> = defs
            .iter()
            .filter(|d| d.domain == "media")
            .map(|d| d.invoke_prefix.as_str())
            .collect();
        assert!(media_prefixes.contains(&"media.__backend.audiobookshelf.audiobooks"));
        assert!(media_prefixes.contains(&"media.__backend.audiobookshelf.podcasts"));
    }

    #[test]
    fn composed_dispatch_routes_each_facet_and_falls_through() {
        let spec = Plugin::named("audiobookshelf")
            .media(DemoMedia {
                media_type: MediaType::Audiobooks,
            })
            .into_spec();
        let dispatch = spec.backend_dispatch.expect("has a dispatcher");
        // The audiobooks media facet answers its `url` op.
        let got = dispatch(
            "media.__backend.audiobookshelf.audiobooks.url",
            serde_json::json!(null),
        );
        let out = got.expect("facet matched").expect("op ok");
        assert_eq!(out["primary"], "http://abs:13378");
        // An unrelated tool falls through (None → #[orca_tool] dispatch).
        assert!(dispatch("audiobookshelf.some_tool", serde_json::json!({})).is_none());
    }
}
