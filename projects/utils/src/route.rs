//! Ordered endpoint reachability paths.
//!
//! An addressable thing (a plugin endpoint, a mesh peer, a mounted share) is
//! reachable by several independent paths — a LAN `IP`, an IPv6 address, a
//! Tailscale address, an FQDN via a reverse proxy, or a local unix socket.
//! There is **no scalar `baseUrl`/`host`/`url`/`socketPath` anywhere**: every
//! addressable thing carries ONE ordered `routes: Vec<Route>`, index 0 = the
//! primary/base, the rest alternates in priority order. A consumer tries each
//! enabled [`Route`] in order until one answers.
//!
//! This is the single shared type across the whole system — the mesh
//! (`contract::ClaimAddress`, `db::PodPeerAddress`, `pod::PodPeerAddressDto`)
//! and every plugin endpoint (`#[endpoint_resource]`'s built-in `routes`
//! column) all use `Route`, not just a shared field name. `utils` is the
//! dependency-free leaf so this one type can be shared without a dependency
//! cycle.
//!
//! A `Route` is a reachability path, NOT an IP routing-table entry or an HTTP
//! route — the mandatory [`Route::kind`] tag (`lan_v4`, `tailscale_v4`, `unix`,
//! …) disambiguates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One reachable path to an addressable thing.
///
/// The URL is reconstructed at resolve time as `scheme://value[:port]` — the
/// pieces are stored separately so there is no scalar URL to drift, and so a
/// per-plugin scheme (a websocket `ws://`, a unix socket) is data, not
/// hardcoded logic.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    /// Network-path label driving priority + operator intent:
    /// `lan_v4` | `lan_v6` | `tailscale_v4` | `tailscale_v6` | `fqdn` | `unix`.
    pub kind: String,
    /// Transport scheme: `http` | `https` | `ws` | `wss` | `unix` | … .
    /// `None` for mesh peers (dialed by `value:port`, no URL scheme).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// The BARE host / IP (or filesystem path when `kind`/`scheme` = `unix`).
    /// Never a full URL — no scheme, no port.
    pub value: String,
    /// Port, when applicable. `None` for schemeless mesh entries that carry the
    /// port elsewhere, or for unix sockets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Export / share path on the server, for a route that addresses a mount
    /// source rather than an HTTP endpoint. An NFS `host:/export` source folds
    /// to `value = host`, `path = "/export"`; an SMB `//server/share` to
    /// `value = server`, `path = "/share"`. `None` for endpoint/mesh routes,
    /// which have no export path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// When `false`, resolvers skip this entry without probing.
    #[serde(default = "default_true")]
    pub enabled: bool,

    // ── mesh-only, defaulted so plugin endpoints ignore them ──────────────────
    /// How this route was learned (`mdns`, `proxmox`, `autodetect`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Human display label for the path, when the mesh has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_label: Option<String>,
    /// Epoch-millis this route was last observed reachable, when tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
}

fn default_true() -> bool {
    true
}

impl Route {
    /// An enabled plugin-endpoint route: `scheme://value:port`.
    pub fn new(
        kind: impl Into<String>,
        scheme: impl Into<String>,
        value: impl Into<String>,
        port: Option<u16>,
    ) -> Self {
        Self {
            kind: kind.into(),
            scheme: Some(scheme.into()),
            value: value.into(),
            port,
            path: None,
            enabled: true,
            source: None,
            kind_label: None,
            last_seen_at: None,
        }
    }

    /// A *learned* schemeless mesh route: dialed by `value`, tagged with where
    /// it was learned (`source`) and when it was last observed reachable
    /// (`last_seen_at`, epoch seconds). The one constructor for turning an
    /// addressing DB row (`pod_peer_addresses`, `host_addressing`) into a
    /// [`Route`] — callers never hand-assemble the `source`/`last_seen_at`
    /// `Option` wrapping. `kind_label` is left unset; it is a presentation
    /// concern stamped at the edge (the mesh label vocabulary lives in `system`).
    pub fn learned(
        kind: impl Into<String>,
        value: impl Into<String>,
        source: impl Into<String>,
        last_seen_at: i64,
    ) -> Self {
        Self {
            source: Some(source.into()),
            last_seen_at: Some(last_seen_at),
            ..Self::mesh(kind, value, None)
        }
    }

    /// A schemeless mesh route: dialed by `value` (+ optional `port`), no URL.
    pub fn mesh(kind: impl Into<String>, value: impl Into<String>, port: Option<u16>) -> Self {
        Self {
            kind: kind.into(),
            scheme: None,
            value: value.into(),
            port,
            path: None,
            enabled: true,
            source: None,
            kind_label: None,
            last_seen_at: None,
        }
    }

    /// Reconstruct the base URL `scheme://value[:port]`, or `None` when this
    /// route has no scheme (a schemeless mesh entry that is not URL-addressable).
    /// An IPv6 literal `value` is bracketed. No trailing slash, no path — the
    /// caller appends any plugin-specific suffix.
    pub fn base_url(&self) -> Option<String> {
        let scheme = self.scheme.as_ref()?;
        let host = if self.value.contains(':') && !self.value.starts_with('[') {
            // Bare IPv6 literal → bracket it for URL authority form.
            format!("[{}]", self.value)
        } else {
            self.value.clone()
        };
        Some(match self.port {
            Some(p) => format!("{scheme}://{host}:{p}"),
            None => format!("{scheme}://{host}"),
        })
    }
}

/// An ordered, priority-ranked set of reachability [`Route`]s for one
/// addressable thing.
///
/// This is the first-class collection the "no scalar URL — one ordered
/// `routes[]`" rule is built on: index 0 is the primary/base, the rest are
/// alternates in descending priority, and a consumer walks them in order until
/// one answers. The ordering-is-priority invariant, dedup, and the primary /
/// enabled accessors live HERE (as methods) instead of being re-implemented at
/// every call site over a bare `Vec<Route>`.
///
/// `#[serde(transparent)]` so it is wire- and column-identical to the
/// `Vec<Route>` it replaces (a bare JSON array) — no migration of stored rows.
/// Deref to `[Route]` gives every slice method (`iter`, `first`, `is_empty`,
/// `len`, `contains`) for free, and deref coercion lets a `&Routes` pass
/// wherever a `&[Route]` is expected (e.g. the plugin-toolkit resolver).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct Routes(Vec<Route>);

impl Routes {
    /// An empty set.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Append `route` unless an equal-identity path (same `kind` + `value`,
    /// matching the `pod_peer_addresses` PK and the inventory union semantics)
    /// is already present. Preserves priority order — the first occurrence
    /// wins, so a higher-priority earlier entry is never displaced by a later
    /// duplicate.
    pub fn push(&mut self, route: Route) {
        if !self
            .0
            .iter()
            .any(|r| r.kind == route.kind && r.value == route.value)
        {
            self.0.push(route);
        }
    }

    /// Number of routes in the set (enabled or not).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the set holds no routes. Named inherently (not only via the
    /// `Deref` slice method) so it is reachable as a `Routes::is_empty` path in
    /// `#[serde(skip_serializing_if = "Routes::is_empty")]`.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The primary reachable path: the first enabled route. `None` when the set
    /// is empty or every route is disabled.
    pub fn primary(&self) -> Option<&Route> {
        self.0.iter().find(|r| r.enabled)
    }

    /// The enabled routes in priority order — the sequence a resolver probes.
    pub fn enabled(&self) -> impl Iterator<Item = &Route> {
        self.0.iter().filter(|r| r.enabled)
    }

    /// The highest-priority route of a given `kind` (`"lan_v4"`, `"fqdn"`, …).
    pub fn find_kind(&self, kind: &str) -> Option<&Route> {
        self.0.iter().find(|r| r.kind == kind)
    }

    /// Consume into the underlying `Vec<Route>`.
    pub fn into_vec(self) -> Vec<Route> {
        self.0
    }
}

impl std::ops::Deref for Routes {
    type Target = [Route];
    fn deref(&self) -> &[Route] {
        &self.0
    }
}

impl From<Vec<Route>> for Routes {
    /// Adopt an existing vec as-is (order preserved; NOT de-duplicated — the
    /// caller asserted the order). Use [`Routes::push`] to dedup on insert.
    fn from(v: Vec<Route>) -> Self {
        Self(v)
    }
}

impl FromIterator<Route> for Routes {
    /// Collect with dedup-on-insert via [`Routes::push`], preserving priority.
    fn from_iter<I: IntoIterator<Item = Route>>(iter: I) -> Self {
        let mut routes = Routes::new();
        for r in iter {
            routes.push(r);
        }
        routes
    }
}

impl IntoIterator for Routes {
    type Item = Route;
    type IntoIter = std::vec::IntoIter<Route>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Routes {
    type Item = &'a Route;
    type IntoIter = std::slice::Iter<'a, Route>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_with_port() {
        let r = Route::new("lan_v4", "http", "10.0.0.15", Some(8989));
        assert_eq!(r.base_url().as_deref(), Some("http://10.0.0.15:8989"));
    }

    #[test]
    fn base_url_no_port() {
        let r = Route::new("fqdn", "https", "sonarr.example.com", None);
        assert_eq!(r.base_url().as_deref(), Some("https://sonarr.example.com"));
    }

    #[test]
    fn base_url_ipv6_bracketed() {
        let r = Route::new("lan_v6", "http", "fd00::1", Some(80));
        assert_eq!(r.base_url().as_deref(), Some("http://[fd00::1]:80"));
    }

    #[test]
    fn schemeless_mesh_route_has_no_base_url() {
        let r = Route::mesh("lan_v4", "10.0.0.15", Some(7777));
        assert_eq!(r.base_url(), None);
    }

    #[test]
    fn serde_omits_none_and_defaults_enabled() {
        let json =
            serde_json::to_string(&Route::new("lan_v4", "http", "10.0.0.15", Some(80))).unwrap();
        // No null scheme/port noise; camelCase; enabled present.
        assert!(json.contains("\"kind\":\"lan_v4\""));
        assert!(json.contains("\"value\":\"10.0.0.15\""));
        assert!(!json.contains("lastSeenAt"));
        // Round-trips.
        let back: Route = serde_json::from_str(&json).unwrap();
        assert!(back.enabled);
    }

    #[test]
    fn routes_push_dedups_by_kind_and_value_preserving_priority() {
        let mut routes = Routes::new();
        routes.push(Route::new("lan_v4", "http", "10.0.0.5", Some(80)));
        // Same (kind, value) — dropped even though port differs (first wins).
        routes.push(Route::new("lan_v4", "https", "10.0.0.5", Some(443)));
        routes.push(Route::new("fqdn", "https", "x.example.com", None));
        assert_eq!(routes.len(), 2);
        assert_eq!(routes.primary().unwrap().scheme.as_deref(), Some("http"));
        assert_eq!(routes.find_kind("fqdn").unwrap().value, "x.example.com");
    }

    #[test]
    fn routes_primary_skips_disabled() {
        let routes = Routes::from(vec![
            Route {
                enabled: false,
                ..Route::new("lan_v4", "http", "10.0.0.5", Some(80))
            },
            Route::new("fqdn", "https", "x.example.com", None),
        ]);
        assert_eq!(routes.primary().unwrap().kind, "fqdn");
        assert_eq!(routes.enabled().count(), 1);
    }

    #[test]
    fn routes_serde_transparent_is_a_bare_array() {
        let routes = Routes::from(vec![Route::mesh("lan_v4", "10.0.0.5", None)]);
        let json = serde_json::to_string(&routes).unwrap();
        assert!(json.starts_with('['), "expected bare array, got {json}");
        let back: Routes = serde_json::from_str(&json).unwrap();
        assert_eq!(back, routes);
    }
}
