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
//! ## One owner per path; a taken path is refused
//!
//! A plugin may only register paths under `/plugins/<its id>` ([`path_in_scope`]);
//! `/` is reserved for the ids in [`ROOT_PATH_PLUGINS`]. Each exact path has
//! exactly one owner: a different provider claiming an owned path is refused
//! with [`WebRegistryError::PathTaken`] (the loader fails that plugin's load).
//! When the owner deregisters the path is empty — nothing is promoted.
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

// The erased-invoke boundary carries args/results/errors as `serde_json::Value`
// (the wire already produces a parsed value; no String hop). This module names
// that type in its `InvokeThunk`/proxy — the sanctioned opaque seam, scoped here.
#![allow(clippy::disallowed_types)]

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
    /// [`register_provider`] was asked to mount `provider` at `path`, which a
    /// different provider already owns.
    PathTaken {
        /// The contested exact path.
        path: String,
        /// The provider that tried to register.
        provider: String,
        /// The provider that owns `path`.
        owner: String,
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
            WebRegistryError::PathTaken {
                path,
                provider,
                owner,
            } => write!(
                f,
                "web provider '{provider}' cannot register path '{path}': already owned by '{owner}'"
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
    /// Dev-mode upstream origin (e.g. `"http://127.0.0.1:12004"`). When set and
    /// the daemon is in dev mode, the server proxies this provider's path to the
    /// upstream (the plugin's `npm run dev` Vite server) instead of calling
    /// `render`. `None` in prod builds where the plugin serves rendered assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_upstream: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub csp_script_hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub csp_style_hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub csp_style_attr_hashes: Vec<String>,
}

impl WebRoute {
    /// Read a route off a `BackendDef`'s shared axes: `endpoint` is the prefix
    /// (empty → `"/"`), `capabilities` carry `spa_fallback` / `dev_upstream=…`.
    pub fn from_def_axes(endpoint: &str, capabilities: &[String]) -> Self {
        WebRoute {
            prefix: if endpoint.is_empty() {
                "/".to_string()
            } else {
                endpoint.to_string()
            },
            spa_fallback: capabilities.iter().any(|c| c == CAP_SPA_FALLBACK),
            dev_upstream: capabilities
                .iter()
                .find_map(|c| c.strip_prefix(CAP_DEV_UPSTREAM))
                .map(str::to_string),
            csp_script_hashes: csp_hashes(capabilities, CAP_CSP_SCRIPT_HASH),
            csp_style_hashes: csp_hashes(capabilities, CAP_CSP_STYLE_HASH),
            csp_style_attr_hashes: csp_hashes(capabilities, CAP_CSP_STYLE_ATTR_HASH),
        }
    }
}

fn csp_hashes(capabilities: &[String], prefix: &str) -> Vec<String> {
    capabilities
        .iter()
        .filter_map(|c| c.strip_prefix(prefix))
        .filter(|h| is_csp_hash(h))
        .map(str::to_string)
        .collect()
}

pub fn is_csp_hash(s: &str) -> bool {
    let Some(inner) = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) else {
        return false;
    };
    let Some((alg, b64)) = inner.split_once('-') else {
        return false;
    };
    let want = match alg {
        "sha256" => 32,
        "sha384" => 48,
        "sha512" => 64,
        _ => return false,
    };
    let body = b64.trim_end_matches('=');
    b64.len() - body.len() <= 2
        && !body.contains('=')
        && utils::encoding::base64_decode(b64).is_ok_and(|d| d.len() == want)
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
/// e.g. `"dev_upstream=http://127.0.0.1:12004"`. Parsed into
/// [`WebRoute::dev_upstream`] by the loader.
pub const CAP_DEV_UPSTREAM: &str = "dev_upstream=";

pub const CAP_CSP_SCRIPT_HASH: &str = "csp_script_hash=";

pub const CAP_CSP_STYLE_HASH: &str = "csp_style_hash=";

pub const CAP_CSP_STYLE_ATTR_HASH: &str = "csp_style_attr_hash=";

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
    /// Provider/registry name (e.g. `"peacock"`): the path's owner and the key
    /// deregistered on plugin unload.
    fn name(&self) -> &str;

    /// The exact path this provider is mounted at, and how it handles misses.
    fn route(&self) -> &WebRoute;

    /// Render one request into a response.
    fn render(&self, req: WebRequest) -> BoxFuture<'_, Result<WebResponse>>;
}

/// Plugin ids allowed to own `/` (the SPA catch-all).
pub const ROOT_PATH_PLUGINS: &[&str] = &["peacock"];

/// Whether plugin `plugin` may register the exact web `path`: `/plugins/<plugin>`
/// or anything beneath it, or `/` for an id in [`ROOT_PATH_PLUGINS`].
pub fn path_in_scope(plugin: &str, path: &str) -> bool {
    if path == "/" {
        return ROOT_PATH_PLUGINS.contains(&plugin);
    }
    let base = format!("/plugins/{plugin}");
    let in_base = path == base
        || path
            .strip_prefix(&base)
            .is_some_and(|rest| rest.starts_with('/'));
    in_base && !path.split('/').any(|seg| seg == "." || seg == "..")
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn WebProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a web provider at its exact path.
///
/// - First claimant of a path owns it.
/// - Re-registering the same `name()` at the same path replaces it in place (a
///   dev rebuild / reload never duplicates it).
/// - A different-named provider claiming an owned path is refused with
///   [`WebRegistryError::PathTaken`].
pub fn register_provider(
    provider: Arc<dyn WebProvider>,
) -> std::result::Result<(), WebRegistryError> {
    let mut g = GLOBAL
        .write()
        .map_err(|_| WebRegistryError::RegistryPoisoned)?;
    let path = &provider.route().prefix;
    match g.iter_mut().find(|p| &p.route().prefix == path) {
        Some(slot) if slot.name() == provider.name() => *slot = provider,
        Some(slot) => {
            return Err(WebRegistryError::PathTaken {
                path: path.clone(),
                provider: provider.name().to_string(),
                owner: slot.name().to_string(),
            });
        }
        None => g.push(provider),
    }
    Ok(())
}

/// Snapshot of the owner of every registered exact path.
pub fn providers() -> Vec<Arc<dyn WebProvider>> {
    let Ok(g) = GLOBAL.read() else {
        tracing::warn!("web registry poisoned; treating as empty");
        return Vec::new();
    };
    g.clone()
}

/// Every currently-contested path. Always empty: a taken path is refused at
/// registration, so no path has more than one claimant.
pub fn conflicts() -> Vec<WebConflict> {
    Vec::new()
}

/// Confirm `provider` as the owner of the exact `path` (the persisted choice
/// replayed at boot). Fails typed unless `provider` is the path's owner.
pub fn set_owner(path: &str, provider: &str) -> std::result::Result<(), WebRegistryError> {
    let g = GLOBAL
        .read()
        .map_err(|_| WebRegistryError::RegistryPoisoned)?;
    if g.iter()
        .any(|p| p.route().prefix == path && p.name() == provider)
    {
        Ok(())
    } else {
        Err(WebRegistryError::NoSuchOwner {
            path: path.to_string(),
            provider: provider.to_string(),
        })
    }
}

/// The provider name that owns `path`, if any is registered there.
pub fn active_owner(path: &str) -> Option<String> {
    let g = GLOBAL.read().ok()?;
    g.iter()
        .find(|p| p.route().prefix == path)
        .map(|p| p.name().to_string())
}

/// Deregister the provider named `name` wherever it appears; its paths become
/// empty. Returns `true` if anything was removed.
pub fn deregister_provider(name: &str) -> bool {
    let Ok(mut g) = GLOBAL.write() else {
        tracing::warn!("web registry poisoned; deregister of '{name}' skipped");
        return false;
    };
    let before = g.len();
    g.retain(|p| p.name() != name);
    g.len() != before
}

/// The provider that owns `"/"` (the SPA / catch-all fallback), if any. The
/// server uses this both as the last-resort route and to repopulate the mesh
/// `frontend` field ("which provider owns `/`").
pub fn root_owner() -> Option<Arc<dyn WebProvider>> {
    let g = GLOBAL.read().ok()?;
    g.iter().find(|p| p.route().prefix == "/").cloned()
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
    if let Some(p) = g.iter().find(|p| p.route().prefix == path) {
        return Some(p.clone());
    }
    // Miss → root SPA catch-all, but only if it opted into spa_fallback.
    g.iter()
        .find(|p| p.route().prefix == "/" && p.route().spa_fallback)
        .cloned()
}

// ── Host-side loaded-plugin proxy ─────────────────────────────────────────────

/// The synchronous invoke thunk a plugin's provider is driven through:
/// `(op, args) -> Result<result, error>`, all as `serde_json::Value`. Plain `Fn`
/// of `Value`s so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk = Arc<
    dyn Fn(
            &str,
            serde_json::Value,
            Option<crate::invocation_scope::BackendCall>,
        ) -> std::result::Result<serde_json::Value, serde_json::Value>
        + Send
        + Sync
        + 'static,
>;

/// Operation name the [`WebProxy`] invokes across the FFI boundary. The plugin
/// exposes a tool `"{invoke_prefix}.{RENDER_OP}"` taking a [`WebRequest`] and
/// returning a [`WebResponse`].
pub const RENDER_OP: &str = "render";

/// Build and register a [`WebProvider`] from a plugin backend descriptor plus an
/// [`InvokeThunk`]. The plugin-loader calls this from its domain dispatch table
/// for `domain = "web"`, threading the route it read off the `BackendDef`
/// (`endpoint` → prefix, `capabilities` → `spa_fallback` / `dev_upstream`).
///
/// Fails with [`WebRegistryError::PathTaken`] when another provider owns the
/// path, or on a poisoned registry lock.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
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
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
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
        let args = serde_json::to_value(&req).unwrap_or_else(|_| serde_json::json!({}));
        Box::pin(async move {
            let call = crate::invocation_scope::capture("web", &name);
            let out = tokio::task::spawn_blocking(move || invoke(RENDER_OP, args, call))
                .await
                .map_err(|e| anyhow::anyhow!("web '{name}' render task panicked: {e}"))?
                .map_err(|e| {
                    anyhow::anyhow!(
                        "web '{name}' render failed: {}",
                        crate::render_invoke_error(&e)
                    )
                })?;
            serde_json::from_value(out)
                .map_err(|e| anyhow::anyhow!("web '{name}' render returned invalid JSON: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_hash_requires_exact_digest_length() {
        let s256 = "'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='";
        let s384 = "'sha384-xgelKlDcc+BYp0O+ZYzFesLnWCgGpEbXVVZWNQT6WyZYygN01tbEMygnjPXb5OPI'";
        assert!(is_csp_hash(s256));
        assert!(is_csp_hash(s384));
        assert!(!is_csp_hash(
            "'sha512-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='"
        ));
        assert!(!is_csp_hash("'sha256-abc='"));
        assert!(!is_csp_hash(
            "'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI'"
        ));
        assert!(!is_csp_hash(
            "'sha256-bhHHL3z2vDgxU=t0W3dWQOrprscmda2Y5pLsLg4GF+pI'"
        ));
        assert!(!is_csp_hash(
            "'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI==='"
        ));
        assert!(!is_csp_hash(
            "'md5-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='"
        ));
        assert!(!is_csp_hash(
            "sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI="
        ));
    }

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
            csp_script_hashes: Vec::new(),
            csp_style_hashes: Vec::new(),
            csp_style_attr_hashes: Vec::new(),
        }
    }

    // The registry is process-global, so tests must not run concurrently
    // against it. This mutex serializes them; each test fully clears the
    // registry while holding the guard.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    fn taken_path_is_refused_and_owner_holds() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "peacock",
            route: route("/", true),
        }))
        .unwrap();
        assert_eq!(
            register_provider(Arc::new(Fake {
                name: "otherui",
                route: route("/", true),
            })),
            Err(WebRegistryError::PathTaken {
                path: "/".to_string(),
                provider: "otherui".to_string(),
                owner: "peacock".to_string(),
            })
        );
        assert_eq!(resolve("/").unwrap().name(), "peacock");
        assert!(conflicts().is_empty());
        set_owner("/", "peacock").unwrap();
        assert_eq!(
            set_owner("/", "otherui"),
            Err(WebRegistryError::NoSuchOwner {
                path: "/".to_string(),
                provider: "otherui".to_string(),
            })
        );
    }

    #[test]
    fn deregister_owner_empties_the_path() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "peacock",
            route: route("/", true),
        }))
        .unwrap();
        assert!(deregister_provider("peacock"));
        assert!(resolve("/").is_none());
        assert!(active_owner("/").is_none());
        // The emptied path is free for a new owner.
        register_provider(Arc::new(Fake {
            name: "otherui",
            route: route("/", true),
        }))
        .unwrap();
        assert_eq!(resolve("/").unwrap().name(), "otherui");
    }

    #[test]
    fn path_scope_is_the_plugins_namespace_or_allow_listed_root() {
        assert!(path_in_scope("peacock", "/"));
        assert!(!path_in_scope("evil", "/"));
        assert!(path_in_scope("gitea", "/plugins/gitea"));
        assert!(path_in_scope("gitea", "/plugins/gitea/ui"));
        assert!(!path_in_scope("gitea", "/plugins/giteax"));
        assert!(!path_in_scope("gitea", "/plugins/other/x"));
        assert!(!path_in_scope("gitea", "/plugins/gitea/../other"));
        assert!(!path_in_scope("gitea", "/api/v1"));
        assert!(!path_in_scope("gitea", ""));
    }

    #[test]
    fn deregister_sole_owner_drops_path_entry_entirely() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "solo",
            route: route("/only", false),
        }))
        .unwrap();
        assert_eq!(resolve("/only").unwrap().name(), "solo");

        assert!(deregister_provider("solo"));
        // The path entry is gone — nothing resolves and no root fallback exists.
        assert!(resolve("/only").is_none());
        assert!(active_owner("/only").is_none());
        assert!(providers().is_empty());
    }

    #[test]
    fn deregister_unknown_provider_removes_nothing() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "keep",
            route: route("/", true),
        }))
        .unwrap();
        // Nothing named "ghost" is registered.
        assert!(!deregister_provider("ghost"));
        assert_eq!(resolve("/").unwrap().name(), "keep");
    }

    #[test]
    fn set_owner_on_unregistered_path_is_typed_error() {
        let _g = guard();
        // No provider has ever claimed "/missing".
        assert_eq!(
            set_owner("/missing", "whoever"),
            Err(WebRegistryError::NoSuchOwner {
                path: "/missing".to_string(),
                provider: "whoever".to_string(),
            })
        );
    }

    #[test]
    fn reregister_same_name_replaces_in_place() {
        let _g = guard();
        for _ in 0..2 {
            register_provider(Arc::new(Fake {
                name: "peacock",
                route: route("/", true),
            }))
            .unwrap();
        }
        assert_eq!(providers().len(), 1);
        assert_eq!(active_owner("/").as_deref(), Some("peacock"));
    }

    #[test]
    fn providers_snapshot_lists_one_active_per_path() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "root",
            route: route("/", true),
        }))
        .unwrap();
        register_provider(Arc::new(Fake {
            name: "assets",
            route: route("/assets", false),
        }))
        .unwrap();

        let mut names: Vec<String> = providers().iter().map(|p| p.name().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["assets".to_string(), "root".to_string()]);
    }

    #[test]
    fn no_conflicts_when_every_path_has_one_claimant() {
        let _g = guard();
        register_provider(Arc::new(Fake {
            name: "root",
            route: route("/", true),
        }))
        .unwrap();
        assert!(conflicts().is_empty());
    }

    #[test]
    fn root_owner_and_active_owner_are_none_on_empty_registry() {
        let _g = guard();
        assert!(root_owner().is_none());
        assert!(active_owner("/").is_none());
        assert!(resolve("/anything").is_none());
        assert!(providers().is_empty());
        assert!(conflicts().is_empty());
    }

    #[test]
    fn web_response_not_found_shape() {
        let r = WebResponse::not_found();
        assert_eq!(r.status, 404);
        assert!(r.headers.is_empty());
        assert!(r.body_b64.is_empty());
    }

    #[test]
    fn web_request_defaults_method_to_get() {
        // `method`, `headers`, `body_b64` all default when absent from the wire.
        let req: WebRequest = serde_json::from_str(r#"{"path":"/x"}"#).unwrap();
        assert_eq!(req.path, "/x");
        assert_eq!(req.method, "GET");
        assert!(req.headers.is_empty());
        assert!(req.body_b64.is_empty());
        // The default_method helper is the source of that "GET".
        assert_eq!(default_method(), "GET");
    }

    #[test]
    fn web_route_dev_upstream_is_skipped_when_none() {
        let r = route("/", true);
        let json = serde_json::to_string(&r).unwrap();
        // `skip_serializing_if` drops the None upstream from the wire.
        assert!(!json.contains("dev_upstream"));
        assert!(json.contains("spa_fallback"));

        let with = WebRoute {
            prefix: "/".to_string(),
            spa_fallback: false,
            dev_upstream: Some("http://127.0.0.1:12004".to_string()),
            csp_script_hashes: Vec::new(),
            csp_style_hashes: Vec::new(),
            csp_style_attr_hashes: Vec::new(),
        };
        let json2 = serde_json::to_string(&with).unwrap();
        assert!(json2.contains("dev_upstream"));
        let back: WebRoute = serde_json::from_str(&json2).unwrap();
        assert_eq!(back, with);
    }

    #[test]
    fn registry_error_display_and_is_std_error() {
        let poisoned = WebRegistryError::RegistryPoisoned;
        assert_eq!(
            poisoned.to_string(),
            "web provider registry lock is poisoned"
        );

        let no_owner = WebRegistryError::NoSuchOwner {
            path: "/".to_string(),
            provider: "ghost".to_string(),
        };
        let msg = no_owner.to_string();
        assert!(msg.contains("cannot assign path '/'"));
        assert!(msg.contains("provider 'ghost'"));

        // Usable as a boxed std::error::Error.
        let taken = WebRegistryError::PathTaken {
            path: "/".to_string(),
            provider: "evil".to_string(),
            owner: "peacock".to_string(),
        }
        .to_string();
        assert!(taken.contains("'evil'") && taken.contains("'/'") && taken.contains("'peacock'"));

        let boxed: Box<dyn std::error::Error> = Box::new(no_owner);
        assert!(boxed.source().is_none());
    }

    #[test]
    fn web_conflict_round_trips_through_serde() {
        let c = WebConflict {
            path: "/".to_string(),
            active_owner: "peacock".to_string(),
            contenders: vec!["otherui".to_string()],
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: WebConflict = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn web_proxy_render_success_deserializes_response() {
        let _g = guard();
        let invoke: InvokeThunk = Arc::new(|op: &str, args: serde_json::Value, _call| {
            assert_eq!(op, RENDER_OP);
            // The request path threads through as JSON.
            assert_eq!(args["path"], "/hello");
            Ok(serde_json::json!({
                "status": 200,
                "headers": [["content-type", "text/plain"]],
                "body_b64": "aGk="
            }))
        });
        register_from_def("peacock".to_string(), route("/", true), invoke).unwrap();

        let provider = resolve("/").unwrap();
        assert_eq!(provider.name(), "peacock");
        assert_eq!(provider.route().prefix, "/");
        // Hold only the provider Arc across the await — release the serialize
        // guard (clippy: await_holding_lock).
        drop(_g);

        let resp = provider
            .render(WebRequest {
                path: "/hello".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_b64, "aGk=");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn web_proxy_render_propagates_invoke_error() {
        let _g = guard();
        let invoke: InvokeThunk =
            Arc::new(|_op, _args, _call| Err(serde_json::json!({"message": "boom"})));
        register_from_def("peacock".to_string(), route("/", true), invoke).unwrap();

        let provider = resolve("/").unwrap();
        drop(_g);
        let err = provider.render(WebRequest::default()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("web 'peacock' render failed"), "got: {msg}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn web_proxy_render_rejects_invalid_json_response() {
        let _g = guard();
        // `status` is a required u16 — a string cannot deserialize into WebResponse.
        let invoke: InvokeThunk =
            Arc::new(|_op, _args, _call| Ok(serde_json::json!({"status": "not-a-number"})));
        register_from_def("peacock".to_string(), route("/", true), invoke).unwrap();

        let provider = resolve("/").unwrap();
        drop(_g);
        let err = provider.render(WebRequest::default()).await.unwrap_err();
        assert!(err.to_string().contains("returned invalid JSON"));
    }
}
