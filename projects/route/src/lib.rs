//! Generic route-realization domain. One trait, one registry — many realizers
//! (a caddy reverse-proxy route, an adguard/pihole/cloudflare DNS record, an
//! opnsense static route, a tailscale DNS entry). orca does not care *what*
//! realizes a route; a [`Route`] is fanned out to every registered realizer that
//! [`handles`](RouteRealizer::handles) it, in one pass.
//!
//! Mirrors the `service`/`storage`/`notifications` shape exactly: a trait + a
//! process-global registry (+ a JSON-proxy FFI boundary in a follow-up), so a
//! thin plugin joins the capability by implementing one small ability and
//! registering it — the logic (fan-out, capability discovery, honest failure)
//! lives here in core, not in the plugin.
//!
//! **Capability availability is dynamic and honest.** [`capabilities`] reports
//! the route kinds realizable *right now*, derived live from the registry — this
//! is what a caller reads to know what is active before calling. [`register`] of
//! a kind that NO realizer handles is a hard [`RouteError::Unavailable`], never a
//! silent success: an absent capability refuses the call rather than pretending.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Object-safe async return type — the canonical hand-desugared `BoxFuture` from
/// `contract` (one definition workspace-wide; no `async_trait` macro).
pub use contract::BoxFuture;
/// The first-class reachability primitive a realizer acts on.
pub use utils::route::{Route, Routes};

// ── Model ───────────────────────────────────────────────────────────────────

/// What a route is realized *toward* — the concrete destination a realizer
/// points the name/address at. Kept small and generic; each realizer reads only
/// the fields its facet needs (a DNS realizer uses `host`; a reverse proxy uses
/// `host`+`port`+`scheme`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RealizeTarget {
    /// Destination host/IP the name or route resolves/forwards to (the A-record
    /// value a DNS realizer writes, the upstream a reverse proxy forwards to).
    pub host: String,
    /// Destination port, when relevant (a reverse-proxy upstream). `None` = the
    /// realizer's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Upstream scheme for a reverse proxy (`http`/`https`). `None` = realizer
    /// default. Irrelevant to pure-DNS realizers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
}

/// One realizer's result within a [`register`]/[`withdraw`] fan-out. A single
/// provider's failure is reported here, not fatal to the pass (contrast
/// [`RouteError::Unavailable`], which is a whole-call refusal).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RealizeOutcome {
    pub provider: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// The route kinds realizable *right now*, each mapped to the providers backing
/// it — derived live from the registry. A caller reads this to know which
/// registration capabilities are active before calling [`register`]. An empty
/// `kinds` map means nothing can be realized (no realizer is registered).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RouteCapabilities {
    /// `kind` (`"fqdn"`, `"lan_v4"`, …) → sorted provider names that realize it.
    pub kinds: BTreeMap<String, Vec<String>>,
}

impl RouteCapabilities {
    /// Whether some registered realizer can realize `kind` right now.
    pub fn supports(&self, kind: &str) -> bool {
        self.kinds.contains_key(kind)
    }
}

#[derive(Debug, Error)]
pub enum RouteError {
    /// No registered realizer offers the capability for this route kind. A call
    /// to realize an unavailable capability is a refusal, never a silent success.
    #[error("no provider offers the `{0}` route-registration capability")]
    Unavailable(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("{0}")]
    Other(String),
}

// ── Adapter trait ────────────────────────────────────────────────────────────

/// One route-realization integration — the single ability a realizer plugin
/// implements. The generic [`register`]/[`withdraw`]/[`capabilities`] functions
/// drive it; the plugin owns only *its* facet (write a caddy route, a DNS
/// record, a static route) and stays thin.
pub trait RouteRealizer: Send + Sync {
    /// Provider name (`"caddy"`, `"adguard"`, `"opnsense"`). Unique in the registry.
    fn provider(&self) -> &str;

    /// The route kinds this realizer can realize (`"fqdn"`, `"lan_v4"`, …). Drives
    /// capability discovery. A realizer whose selection depends on more than kind
    /// also overrides [`handles`](Self::handles).
    fn kinds(&self) -> Vec<String>;

    /// Whether this realizer will act on `route`. Defaults to kind membership
    /// against [`kinds`](Self::kinds); override for finer selection.
    fn handles(&self, route: &Route) -> bool {
        self.kinds().iter().any(|k| k == &route.kind)
    }

    /// Realize `route` toward `target` idempotently — the same call re-run is a
    /// no-op, so a periodic reconcile heals drift.
    fn realize<'a>(
        &'a self,
        route: &'a Route,
        target: &'a RealizeTarget,
    ) -> BoxFuture<'a, Result<(), RouteError>>;

    /// Withdraw a previously realized `route` idempotently (absent = success).
    fn withdraw<'a>(&'a self, route: &'a Route) -> BoxFuture<'a, Result<(), RouteError>>;
}

// ── Registry ─────────────────────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn RouteRealizer>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register (or replace, by provider name) a route realizer.
pub fn register_realizer(realizer: Arc<dyn RouteRealizer>) {
    let mut g = GLOBAL.write().expect("route registry poisoned");
    let name = realizer.provider().to_string();
    if let Some(slot) = g.iter_mut().find(|r| r.provider() == name) {
        *slot = realizer;
    } else {
        g.push(realizer);
    }
}

/// Every registered realizer.
pub fn realizers() -> Vec<Arc<dyn RouteRealizer>> {
    GLOBAL.read().expect("route registry poisoned").clone()
}

/// Remove a realizer by provider name. Returns whether one was removed.
pub fn deregister_realizer(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("route registry poisoned");
    let before = g.len();
    g.retain(|r| r.provider() != name);
    before != g.len()
}

/// The route kinds realizable right now, mapped to the providers backing each —
/// derived live from the registry. The dynamic capability surface a caller reads
/// before calling [`register`].
pub fn capabilities() -> RouteCapabilities {
    let mut kinds: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in realizers() {
        for k in r.kinds() {
            kinds.entry(k).or_default().push(r.provider().to_string());
        }
    }
    for providers in kinds.values_mut() {
        providers.sort();
        providers.dedup();
    }
    RouteCapabilities { kinds }
}

/// Realize `route` toward `target` across every registered realizer that
/// [`handles`](RouteRealizer::handles) it, in ONE pass.
///
/// Honest availability: when NO realizer handles the route's kind this returns
/// [`RouteError::Unavailable`] — a call to an absent capability must not succeed.
/// When at least one handles it, every match runs and its result is collected;
/// a single provider's failure is reported in its [`RealizeOutcome`], not fatal.
pub async fn register(
    route: &Route,
    target: &RealizeTarget,
) -> Result<Vec<RealizeOutcome>, RouteError> {
    let chosen: Vec<Arc<dyn RouteRealizer>> = realizers()
        .into_iter()
        .filter(|r| r.handles(route))
        .collect();
    if chosen.is_empty() {
        return Err(RouteError::Unavailable(route.kind.clone()));
    }
    let mut out = Vec::with_capacity(chosen.len());
    for r in chosen {
        let outcome = match r.realize(route, target).await {
            Ok(()) => RealizeOutcome {
                provider: r.provider().to_string(),
                ok: true,
                detail: String::new(),
            },
            Err(e) => RealizeOutcome {
                provider: r.provider().to_string(),
                ok: false,
                detail: e.to_string(),
            },
        };
        out.push(outcome);
    }
    Ok(out)
}

/// Withdraw `route` across every realizer that handles it — inverse of
/// [`register`], same one-pass fan-out and same honest-availability refusal.
pub async fn withdraw(route: &Route) -> Result<Vec<RealizeOutcome>, RouteError> {
    let chosen: Vec<Arc<dyn RouteRealizer>> = realizers()
        .into_iter()
        .filter(|r| r.handles(route))
        .collect();
    if chosen.is_empty() {
        return Err(RouteError::Unavailable(route.kind.clone()));
    }
    let mut out = Vec::with_capacity(chosen.len());
    for r in chosen {
        let outcome = match r.withdraw(route).await {
            Ok(()) => RealizeOutcome {
                provider: r.provider().to_string(),
                ok: true,
                detail: String::new(),
            },
            Err(e) => RealizeOutcome {
                provider: r.provider().to_string(),
                ok: false,
                detail: e.to_string(),
            },
        };
        out.push(outcome);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The registry is process-global; serialize the tests that mutate it and
    // start each from a clean slate so parallel runs don't cross-contaminate.
    static LOCK: Mutex<()> = Mutex::new(());

    struct Guard<'a>(#[allow(dead_code)] std::sync::MutexGuard<'a, ()>);

    fn isolate() -> Guard<'static> {
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for r in realizers() {
            deregister_realizer(r.provider());
        }
        Guard(g)
    }

    /// A test realizer that handles the given kinds and records or fails calls.
    struct MockRealizer {
        name: &'static str,
        kinds: Vec<String>,
        fail: bool,
    }

    impl MockRealizer {
        fn arc(name: &'static str, kinds: &[&str], fail: bool) -> Arc<dyn RouteRealizer> {
            Arc::new(MockRealizer {
                name,
                kinds: kinds.iter().map(|s| s.to_string()).collect(),
                fail,
            })
        }
    }

    impl RouteRealizer for MockRealizer {
        fn provider(&self) -> &str {
            self.name
        }
        fn kinds(&self) -> Vec<String> {
            self.kinds.clone()
        }
        fn realize<'a>(
            &'a self,
            _route: &'a Route,
            _target: &'a RealizeTarget,
        ) -> BoxFuture<'a, Result<(), RouteError>> {
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(RouteError::Other("boom".into()))
                } else {
                    Ok(())
                }
            })
        }
        fn withdraw<'a>(&'a self, _route: &'a Route) -> BoxFuture<'a, Result<(), RouteError>> {
            Box::pin(async move { Ok(()) })
        }
    }

    // Each test uses distinct provider names so the process-global registry does
    // not cross-contaminate (tests share one registry).
    fn fqdn(host: &str) -> Route {
        Route::new("fqdn", "https", host, None)
    }

    #[tokio::test]
    async fn register_fans_out_to_every_handler() {
        let _g = isolate();
        register_realizer(MockRealizer::arc("caddy-a", &["fqdn"], false));
        register_realizer(MockRealizer::arc("adguard-a", &["fqdn"], false));
        let out = register(&fqdn("x.example.com"), &RealizeTarget::default())
            .await
            .expect("has handlers");
        let mut names: Vec<_> = out.iter().map(|o| o.provider.clone()).collect();
        names.sort();
        assert!(names.contains(&"caddy-a".to_string()));
        assert!(names.contains(&"adguard-a".to_string()));
        assert!(out.iter().all(|o| o.ok));
        deregister_realizer("caddy-a");
        deregister_realizer("adguard-a");
    }

    #[tokio::test]
    async fn unavailable_capability_is_a_refusal_not_a_silent_success() {
        let _g = isolate();
        // No realizer handles `lan_v4` here → register must ERROR, not return an
        // empty successful fan-out.
        register_realizer(MockRealizer::arc("caddy-b", &["fqdn"], false));
        let err = register(
            &Route::new("lan_v4", "http", "10.0.0.5", Some(80)),
            &RealizeTarget::default(),
        )
        .await
        .expect_err("no lan_v4 realizer");
        assert!(matches!(err, RouteError::Unavailable(k) if k == "lan_v4"));
        deregister_realizer("caddy-b");
    }

    #[tokio::test]
    async fn one_provider_failure_is_reported_not_fatal() {
        let _g = isolate();
        register_realizer(MockRealizer::arc("caddy-c", &["fqdn"], false));
        register_realizer(MockRealizer::arc("adguard-c", &["fqdn"], true));
        let out = register(&fqdn("y.example.com"), &RealizeTarget::default())
            .await
            .expect("has handlers");
        let ok: Vec<_> = out.iter().filter(|o| o.ok).map(|o| &o.provider).collect();
        let bad: Vec<_> = out.iter().filter(|o| !o.ok).map(|o| &o.provider).collect();
        assert_eq!(ok, vec![&"caddy-c".to_string()]);
        assert_eq!(bad, vec![&"adguard-c".to_string()]);
        assert!(out.iter().find(|o| !o.ok).unwrap().detail.contains("boom"));
        deregister_realizer("caddy-c");
        deregister_realizer("adguard-c");
    }

    #[tokio::test]
    async fn capabilities_report_active_kinds_and_providers() {
        let _g = isolate();
        register_realizer(MockRealizer::arc("caddy-d", &["fqdn"], false));
        register_realizer(MockRealizer::arc("opnsense-d", &["fqdn", "lan_v4"], false));
        let caps = capabilities();
        assert!(caps.supports("fqdn"));
        assert!(caps.supports("lan_v4"));
        assert!(
            caps.kinds
                .get("fqdn")
                .unwrap()
                .contains(&"caddy-d".to_string())
        );
        assert!(
            caps.kinds
                .get("fqdn")
                .unwrap()
                .contains(&"opnsense-d".to_string())
        );
        assert_eq!(
            caps.kinds.get("lan_v4").unwrap(),
            &vec!["opnsense-d".to_string()]
        );
        deregister_realizer("caddy-d");
        deregister_realizer("opnsense-d");
    }
}
