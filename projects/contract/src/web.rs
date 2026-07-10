//! Web-route provider seam.
//!
//! A plugin that serves an HTTP surface (a SvelteKit SPA, a Scalar viewer, any
//! static/dynamic web asset tree) registers a [`WebProvider`] into a
//! process-global registry, exactly the way the `storage` / `topology` /
//! `diagnostics` domains already do. The server crate's fallback router calls
//! [`resolve`] to pick the provider for each request.
//!
//! This is how the web UI leaves orca core: the frontend becomes an
//! out-of-process plugin (`peacock`) that registers `WebRoute{prefix:"/",
//! spa_fallback:true}` and answers every request through one `web.render` tool.
//! orca no longer embeds the built assets.
//!
//! ## Exact-path ownership (NOT prefix matching)
//!
//! The registry key is the **literal path string**. `/`, `/stuff`, and
//! `/stuff/things` are INDEPENDENT keys — different plugins can own each
//! concurrently. There is NO longest-prefix subsumption: owning `/stuff` does
//! not imply ownership of `/stuff/things`.
//!
//! **Dispatch** ([`resolve`]): exact-match the request path against the
//! registered paths → that path's active provider. On a MISS, delegate to the
//! owner of `/` **iff** that owner registered `spa_fallback = true` (the root
//! SPA catch-all: serves JS/CSS assets, client-routed URLs, index.html).
//! Non-root registrations are literal exact paths only.
//!
//! ## Conflict → user chooses the owner (non-fatal)
//!
//! Each exact path has at most one ACTIVE owner. When a second plugin claims an
//! already-owned exact path (e.g. two UI plugins both want `/`), the claim is
//! recorded as a **contender** — never fatal. The incumbent (first registered)
//! keeps serving; the conflict is surfaced (WARN log by the loader, observable
//! via [`conflicts`]). The user picks the owner with [`set_owner`], and the
//! server persists that choice and replays it at boot via [`set_owner`] again.
//! No `panic!`/`.expect()`/`.unwrap()` on the registration or dispatch path — a
//! poisoned lock surfaces as a typed error and the daemon stays up.
//!
//! ## Route on the existing `BackendDef` (no ABI change)
//!
//! A web backend rides the shared `abi::BackendDef` the same way `storage` /
//! `container_runtime` overload it: `endpoint` carries the route prefix (e.g.
//! `"/"`), and `capabilities` carries feature flags — the presence of
//! `"spa_fallback"` sets [`WebRoute::spa_fallback`]. No new proto field, no wire
//! change; the loader threads those fields into [`register_from_def`].
//!
//! Async methods return [`crate::BoxFuture`] — no `async_trait` macro, per the
//! workspace rule.

use std::fmt;
use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

// ── Registry errors ────────────────────────────────────────────────────────────

/// Typed failure from the web registry. Every fallible path returns this rather
/// than panicking, so a conflict or a poisoned lock can never abort the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebRegistryError {
    /// The registry lock was poisoned by a panic in another thread. Surfaced as
    /// a typed error instead of propagating the panic, so a single poisoned lock
    /// never brings the daemon down through the registration/dispatch path.
    RegistryPoisoned,
    /// [`set_owner`] was asked to make `provider` the active owner of `path`,
    /// but no provider by that name has registered for that exact path.
    NoSuchOwner {
        /// The exact path the user tried to assign.
        path: String,
        /// The provider name that was requested but is not a candidate.
        provider: String,
    },
}

impl fmt::Display for WebRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebRegistryError::RegistryPoisoned => {
                write!(f, "web provider registry lock is poisoned")
            }
            WebRegistryError::NoSuchOwner { path, provider } => write!(
                f,
                "cannot assign path '{path}' to provider '{provider}': \
                 no provider by that name is registered for that path"
            ),
        }
    }
}

impl std::error::Error for WebRegistryError {}

/// An observable record that an exact path is contested by more than one
/// provider. Surfaced (via [`conflicts`]) so the user can see e.g. "path `/` is
/// contested by peacock and X" and pick the owner with [`set_owner`].
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct WebConflict {
    /// The contested exact path.
    pub path: String,
    /// The provider currently serving the path (the active owner).
    pub active_owner: String,
    /// The other providers that also claimed this exact path and are set aside
    /// (non-fatally) until the user chooses.
    pub contenders: Vec<String>,
}

/// Where a [`WebProvider`] is mounted and how unmatched paths are handled.
/// Constructed by the loader from the plugin's `BackendDef` (`endpoint` →
/// `prefix`, `capabilities` contains `"spa_fallback"` → `spa_fallback`).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct WebRoute {
    /// The exact URL path this provider claims, leading slash included. `"/"` =
    /// the root SPA / catch-all owner. This is a literal key — it is matched
    /// exactly at dispatch, not as a prefix.
    pub prefix: String,
    /// Only meaningful for the `"/"` owner. When `true`, a request path that
    /// matches no registered exact path is dispatched to this provider so its
    /// client-side (SPA) router can resolve it. Off for asset-only / non-root
    /// providers.
    #[serde(default)]
    pub spa_fallback: bool,
    /// Dev-mode upstream origin (e.g. `"http://127.0.0.1:12001"`). When set and
    /// the daemon is in dev mode, the server proxies this provider's path to the
    /// upstream (the plugin's `npm run dev` Vite server) instead of calling
    /// `render`. `None` in prod builds where the plugin serves rendered assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_upstream: Option<String>,
}

/// `settings`-table key prefix for user-chosen web-route owners: the row
/// `web.owner.<exact-path>` → `<provider-name>` records "the user selected this
/// provider to own this contested path". Typed K/V, no `serde_json::Value`.
/// Shared so the setter (the `web` tool) and the boot-time replayer (the server)
/// agree on the key shape.
pub const WEB_OWNER_SETTING_PREFIX: &str = "web.owner.";

/// Capability string on a `BackendDef` that flips [`WebRoute::spa_fallback`].
pub const CAP_SPA_FALLBACK: &str = "spa_fallback";

/// Capability prefix on a `BackendDef` carrying the dev-mode upstream origin,
/// e.g. `"dev_upstream=http://127.0.0.1:12001"`. Parsed into
/// [`WebRoute::dev_upstream`] by the loader.
pub const CAP_DEV_UPSTREAM: &str = "dev_upstream=";

/// One request the server hands a [`WebProvider`]. Body is base64 so the typed
/// contract carries arbitrary bytes without a `serde_json::Value` escape hatch.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct WebRequest {
    /// Request path (already stripped of query string), leading slash included.
    pub path: String,
    /// HTTP method, uppercased (`"GET"`, `"POST"`, …).
    #[serde(default = "default_method")]
    pub method: String,
    /// Request headers, lowercased names → values, in receive order.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Base64-encoded request body. Empty string = no body.
    #[serde(default)]
    pub body_b64: String,
}

fn default_method() -> String {
    "GET".to_string()
}

/// One response a [`WebProvider`] returns for a [`WebRequest`]. Body is base64
/// for the same reason as the request.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct WebResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers, name → value, in emit order.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Base64-encoded response body. Empty string = empty body.
    #[serde(default)]
    pub body_b64: String,
}

impl WebResponse {
    /// A bare 404 with no body — the signal the server uses to decide whether to
    /// apply SPA fallback.
    pub fn not_found() -> Self {
        WebResponse {
            status: 404,
            headers: Vec::new(),
            body_b64: String::new(),
        }
    }
}

// ── Provider registry ────────────────────────────────────────────────────────

/// A source of HTTP responses mounted at a [`WebRoute`]. One per web plugin.
/// Registered into the process-global registry so the server's fallback router
/// can dispatch plugin-agnostically.
pub trait WebProvider: Send + Sync {
    /// Provider/registry name (e.g. `"peacock"`). Identifies a candidate within
    /// a contested path and is the key deregistered on plugin unload.
    fn name(&self) -> &str;

    /// The exact path this provider is mounted at, and how it handles misses.
    fn route(&self) -> &WebRoute;

    /// Render one request into a response.
    fn render(&self, req: WebRequest) -> BoxFuture<'_, Result<WebResponse>>;
}

/// All providers that have claimed one exact path. `providers[0]` is the
/// incumbent (first registered). `active` names the currently-serving provider —
/// the incumbent by default, or the user's chosen owner once [`set_owner`] runs.
struct PathEntry {
    path: String,
    providers: Vec<Arc<dyn WebProvider>>,
    active: String,
}

impl PathEntry {
    fn active_provider(&self) -> Option<Arc<dyn WebProvider>> {
        self.providers
            .iter()
            .find(|p| p.name() == self.active)
            .cloned()
            // Defensive: if the active name somehow references a departed
            // provider, fall back to the incumbent rather than serving nothing.
            .or_else(|| self.providers.first().cloned())
    }
}

static GLOBAL: LazyLock<RwLock<Vec<PathEntry>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a web provider at its exact path.
///
/// - First claimant of a path becomes the active owner.
/// - Re-registering the same `name()` at the same path replaces that candidate
///   in place (a dev rebuild / reload never duplicates it) and keeps it active
///   if it already was.
/// - A different-named provider claiming an already-owned path is recorded as a
///   **contender**: the incumbent keeps serving (non-fatal), the conflict is
///   surfaced via [`conflicts`]. Returns `Ok(())` — a conflict is not an error;
///   it is a state the user resolves with [`set_owner`].
///
/// A poisoned lock is the only failure and is returned typed, never panicked.
pub fn register_provider(
    provider: Arc<dyn WebProvider>,
) -> std::result::Result<(), WebRegistryError> {
    let mut g = GLOBAL
        .write()
        .map_err(|_| WebRegistryError::RegistryPoisoned)?;
    let path = provider.route().prefix.clone();
    let name = provider.name().to_string();

    if let Some(entry) = g.iter_mut().find(|e| e.path == path) {
        if let Some(slot) = entry.providers.iter_mut().find(|p| p.name() == name) {
            // Same provider re-registering: replace in place, preserve active.
            *slot = provider;
        } else {
            // New contender on an already-owned path. Incumbent holds.
            entry.providers.push(provider);
        }
    } else {
        g.push(PathEntry {
            path,
            active: name,
            providers: vec![provider],
        });
    }
    Ok(())
}

/// Snapshot of the active provider for every registered exact path.
pub fn providers() -> Vec<Arc<dyn WebProvider>> {
    let Ok(g) = GLOBAL.read() else {
        tracing::warn!("web registry poisoned; treating as empty");
        return Vec::new();
    };
    g.iter().filter_map(|e| e.active_provider()).collect()
}

/// Every currently-contested path, for surfacing in plugin / route status.
/// Empty when no path has more than one claimant.
pub fn conflicts() -> Vec<WebConflict> {
    let Ok(g) = GLOBAL.read() else {
        tracing::warn!("web registry poisoned; reporting no conflicts");
        return Vec::new();
    };
    g.iter()
        .filter(|e| e.providers.len() > 1)
        .map(|e| WebConflict {
            path: e.path.clone(),
            active_owner: e.active.clone(),
            contenders: e
                .providers
                .iter()
                .map(|p| p.name().to_string())
                .filter(|n| n != &e.active)
                .collect(),
        })
        .collect()
}

/// Make `provider` the active owner of the exact `path`. This is the user's
/// choice, replayed at boot from the persisted assignment. The displaced
/// incumbent is set aside non-fatally (kept as a contender, still deregisterable
/// on unload). Fails typed if `provider` never claimed `path`.
pub fn set_owner(path: &str, provider: &str) -> std::result::Result<(), WebRegistryError> {
    let mut g = GLOBAL
        .write()
        .map_err(|_| WebRegistryError::RegistryPoisoned)?;
    let Some(entry) = g.iter_mut().find(|e| e.path == path) else {
        return Err(WebRegistryError::NoSuchOwner {
            path: path.to_string(),
            provider: provider.to_string(),
        });
    };
    if !entry.providers.iter().any(|p| p.name() == provider) {
        return Err(WebRegistryError::NoSuchOwner {
            path: path.to_string(),
            provider: provider.to_string(),
        });
    }
    entry.active = provider.to_string();
    Ok(())
}

/// The provider name currently active for `path`, if any is registered there.
pub fn active_owner(path: &str) -> Option<String> {
    let g = GLOBAL.read().ok()?;
    g.iter().find(|e| e.path == path).map(|e| e.active.clone())
}

/// Deregister the provider named `name` wherever it appears. When it was a
/// path's sole owner the path entry is dropped; when it was the active owner of
/// a contested path, the incumbent-most surviving candidate takes over so the
/// path stays served. Returns `true` if anything was removed.
pub fn deregister_provider(name: &str) -> bool {
    let Ok(mut g) = GLOBAL.write() else {
        tracing::warn!("web registry poisoned; deregister of '{name}' skipped");
        return false;
    };
    let mut removed = false;
    for entry in g.iter_mut() {
        let before = entry.providers.len();
        entry.providers.retain(|p| p.name() != name);
        if entry.providers.len() != before {
            removed = true;
            if entry.active == name
                && let Some(next) = entry.providers.first()
            {
                entry.active = next.name().to_string();
            }
        }
    }
    g.retain(|e| !e.providers.is_empty());
    removed
}

/// The provider that owns `"/"` (the SPA / catch-all fallback), if any. The
/// server uses this both as the last-resort route and to repopulate the mesh
/// `frontend` field ("which provider owns `/`").
pub fn root_owner() -> Option<Arc<dyn WebProvider>> {
    let g = GLOBAL.read().ok()?;
    g.iter()
        .find(|e| e.path == "/")
        .and_then(|e| e.active_provider())
}

/// Dispatch a request to the provider that owns the **exact** `path`. On a miss,
/// fall back to the `"/"` owner **only if** it registered `spa_fallback = true`.
/// Returns `None` when nothing matches and there is no SPA catch-all (headless
/// build, or an asset-only registry). SPA index re-render is applied by the
/// caller, which knows the matched provider's route.
pub fn resolve(path: &str) -> Option<Arc<dyn WebProvider>> {
    let Ok(g) = GLOBAL.read() else {
        tracing::warn!("web registry poisoned; no route resolved");
        return None;
    };
    // Exact match first — literal key, no prefix subsumption.
    if let Some(entry) = g.iter().find(|e| e.path == path)
        && let Some(p) = entry.active_provider()
    {
        return Some(p);
    }
    // Miss → root SPA catch-all, but only if it opted into spa_fallback.
    g.iter()
        .find(|e| e.path == "/")
        .and_then(|e| e.active_provider())
        .filter(|p| p.route().spa_fallback)
}

// ── cdylib / subprocess FFI proxy ─────────────────────────────────────────────

/// The synchronous invoke thunk a plugin's provider is driven through:
/// `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn` of
/// strings so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation name the [`WebProxy`] invokes across the FFI boundary. The plugin
/// exposes a tool `"{invoke_prefix}.{RENDER_OP}"` taking a [`WebRequest`] and
/// returning a [`WebResponse`].
pub const RENDER_OP: &str = "render";

/// Build and register a [`WebProvider`] from a plugin backend descriptor plus an
/// [`InvokeThunk`]. The plugin-loader calls this from its domain dispatch table
/// for `domain = "web"`, threading the route it read off the `BackendDef`
/// (`endpoint` → prefix, `capabilities` → `spa_fallback` / `dev_upstream`).
///
/// Registration is non-fatal: a conflict (a second plugin on an owned exact
/// path) is recorded and surfaced, not returned as an error. The only error is a
/// poisoned registry lock, which the loader logs and continues past.
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(
    name: String,
    route: WebRoute,
    invoke: InvokeThunk,
) -> std::result::Result<(), WebRegistryError> {
    register_provider(Arc::new(WebProxy {
        name,
        route,
        invoke,
    }))
}

/// A [`WebProvider`] backed by a plugin reached over the JSON-proxy boundary.
/// `render` offloads the synchronous [`InvokeThunk`] onto `spawn_blocking` and
/// (de)serializes JSON at the seam.
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct WebProxy {
    name: String,
    route: WebRoute,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl WebProvider for WebProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn route(&self) -> &WebRoute {
        &self.route
    }

    fn render(&self, req: WebRequest) -> BoxFuture<'_, Result<WebResponse>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let args_json = serde_json::to_string(&req).unwrap_or_else(|_| "{}".to_string());
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(RENDER_OP, args_json))
                .await
                .map_err(|e| anyhow::anyhow!("web '{name}' render task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("web '{name}' render failed: {e}"))?;
            serde_json::from_str(&out)
                .map_err(|e| anyhow::anyhow!("web '{name}' render returned invalid JSON: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        name: &'static str,
        route: WebRoute,
    }
    impl WebProvider for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn route(&self) -> &WebRoute {
            &self.route
        }
        fn render(&self, _req: WebRequest) -> BoxFuture<'_, Result<WebResponse>> {
            Box::pin(async { Ok(WebResponse::not_found()) })
        }
    }

    fn route(prefix: &str, spa: bool) -> WebRoute {
        WebRoute {
            prefix: prefix.to_string(),
            spa_fallback: spa,
            dev_upstream: None,
        }
    }

    // The registry is process-global, so tests must not run concurrently
    // against it. This mutex serializes them; each test fully clears the
    // registry while holding the guard.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Drain GLOBAL wholesale — `providers()` returns only active owners, so
        // deregistering those would leave non-active contenders from a prior
        // test behind (e.g. an `otherui` contender on `/`), contaminating the
        // next test's registration.
        if let Ok(mut reg) = GLOBAL.write() {
            reg.clear();
        }
        g
    }

    #[test]
    fn exact_paths_are_independent_keys_no_prefix_subsumption() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "root",
            route: route("/", true),
        }))
        .unwrap();
        register_provider(Arc::new(Fake {
            name: "stuff",
            route: route("/stuff", false),
        }))
        .unwrap();

        // Exact keys resolve to their own owner.
        assert_eq!(resolve("/stuff").unwrap().name(), "stuff");
        // Owning /stuff does NOT imply /stuff/things — that misses /stuff and
        // falls through to the spa_fallback root.
        assert_eq!(resolve("/stuff/things").unwrap().name(), "root");
        // Arbitrary miss → spa_fallback root.
        assert_eq!(resolve("/dashboard").unwrap().name(), "root");
    }

    #[test]
    fn miss_without_spa_fallback_root_resolves_nothing() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "root",
            route: route("/", false), // NOT a SPA catch-all
        }))
        .unwrap();
        assert!(resolve("/nope").is_none());
        assert_eq!(resolve("/").unwrap().name(), "root");
    }

    #[test]
    fn conflict_is_non_fatal_incumbent_holds_and_is_surfaced() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "peacock",
            route: route("/", true),
        }))
        .unwrap();
        // Second claimant of the same exact path: NON-FATAL, incumbent holds.
        register_provider(Arc::new(Fake {
            name: "otherui",
            route: route("/", true),
        }))
        .unwrap();

        // Incumbent still active — the UI never went offline.
        assert_eq!(resolve("/").unwrap().name(), "peacock");
        assert_eq!(root_owner().unwrap().name(), "peacock");

        // Conflict is observable for the user.
        let c = conflicts();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].path, "/");
        assert_eq!(c[0].active_owner, "peacock");
        assert_eq!(c[0].contenders, vec!["otherui".to_string()]);
    }

    #[test]
    fn user_can_choose_the_owner_and_choice_takes_effect() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "peacock",
            route: route("/", true),
        }))
        .unwrap();
        register_provider(Arc::new(Fake {
            name: "otherui",
            route: route("/", true),
        }))
        .unwrap();

        // User selects the other UI for `/`.
        set_owner("/", "otherui").unwrap();
        assert_eq!(resolve("/").unwrap().name(), "otherui");
        assert_eq!(active_owner("/").as_deref(), Some("otherui"));

        // Choosing an unregistered provider is a typed error, not a panic.
        assert_eq!(
            set_owner("/", "ghost"),
            Err(WebRegistryError::NoSuchOwner {
                path: "/".to_string(),
                provider: "ghost".to_string(),
            })
        );
    }

    #[test]
    fn deregister_active_owner_promotes_survivor() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "peacock",
            route: route("/", true),
        }))
        .unwrap();
        register_provider(Arc::new(Fake {
            name: "otherui",
            route: route("/", true),
        }))
        .unwrap();
        set_owner("/", "otherui").unwrap();

        // The active owner unloads; the surviving candidate takes over so `/`
        // keeps serving.
        assert!(deregister_provider("otherui"));
        assert_eq!(resolve("/").unwrap().name(), "peacock");
    }
}
