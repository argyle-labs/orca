//! Endpoint reachability resolution over the shared [`Route`] type.
//!
//! The [`Route`] type itself lives in `utils` (the dependency-free leaf) so the
//! mesh and every plugin share ONE type. This module re-exports it and adds the
//! plugin-facing bits: the `--route` clap parser and the reachability resolver
//! with last-good caching.
//!
//! An endpoint may be reachable by several independent paths (LAN `IP:port`,
//! IPv6, Tailscale, an FQDN via reverse proxy). **No machine is assumed to have
//! any particular path.** The resolver tries each enabled [`Route`] in registered
//! order and uses whichever answers first. This is the single connection-fallback
//! primitive every `endpoint_resource!` plugin inherits — `routes` is a built-in
//! column on every endpoint. See [[feedback-self-healing-is-mandatory]].

pub use ::utils::route::{Route, Routes};

#[cfg(any(feature = "http", feature = "delegated-http"))]
use std::collections::HashMap;
#[cfg(any(feature = "http", feature = "delegated-http"))]
use std::sync::{Mutex, OnceLock};
#[cfg(feature = "http")]
use std::time::Duration;

#[cfg(any(feature = "http", feature = "delegated-http"))]
use anyhow::{Result, bail};

/// clap `value_parser` for the repeatable `--route` flag. Accepts either a full
/// JSON object (`{"kind":"lan_v4","scheme":"http","value":"10.0.0.5","port":8989}`)
/// or the shorthand `kind=scheme://value[:port]` (enabled defaults to true).
pub fn parse_route(s: &str) -> std::result::Result<Route, String> {
    let s = s.trim();
    if s.starts_with('{') {
        return serde_json::from_str::<Route>(s).map_err(|e| format!("invalid route JSON: {e}"));
    }
    let (kind, url) = s
        .split_once('=')
        .filter(|(k, u)| !k.trim().is_empty() && !u.trim().is_empty())
        .ok_or_else(|| {
            format!("expected `kind=scheme://value[:port]` or a JSON object, got `{s}`")
        })?;
    let (scheme, value, port) = split_url(url.trim())
        .ok_or_else(|| format!("route `{}` is not `scheme://value[:port]`", url.trim()))?;
    Ok(Route::new(kind.trim(), scheme, value, port))
}

/// Split `scheme://value[:port]` into its parts. Bare-IPv6 authorities must be
/// bracketed (`http://[fd00::1]:80`). Returns `None` if there is no `scheme://`.
fn split_url(url: &str) -> Option<(String, String, Option<u16>)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    // Strip any path/query — a Route holds host+port only.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        // Bracketed IPv6: `[fd00::1]` or `[fd00::1]:80`.
        let (h, tail) = stripped.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
        (h.to_string(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        // Only treat trailing `:NNNN` as a port when it parses; otherwise the
        // colon belongs to a bare (unbracketed) IPv6 literal.
        match p.parse::<u16>() {
            Ok(port) => (h.to_string(), Some(port)),
            Err(_) => (authority.to_string(), None),
        }
    } else {
        (authority.to_string(), None)
    };
    Some((scheme.to_string(), host, port))
}

#[cfg(any(feature = "http", feature = "delegated-http"))]
fn last_good() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Probe a base URL: any HTTP response (even 401/404) proves the host is
/// reachable; only a transport error (connect refused, DNS failure, timeout)
/// counts as down — which lets resolution survive a broken DNS path by falling
/// through to a raw-IP route.
#[cfg(feature = "http")]
async fn reachable(client: &utils::http::Client, url: &str, insecure: bool) -> bool {
    match client
        .get(url)
        .insecure(insecure)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(_) => true,
        Err(utils::http::HttpError::Status { .. }) => true,
        Err(_) => false,
    }
}

#[cfg(feature = "http")]
async fn probe(url: &str, insecure: bool) -> bool {
    reachable(&utils::http::Client::new(), url, insecure).await
}

#[cfg(all(feature = "delegated-http", not(feature = "http")))]
async fn probe(url: &str, insecure: bool) -> bool {
    let client = match crate::reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(insecure)
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let request = match client.get(url).build() {
        Ok(r) => r,
        Err(_) => return false,
    };
    client.execute(request).await.is_ok()
}

/// Resolve the first reachable base URL for an endpoint, trying the
/// last-known-good path first, then every enabled [`Route`] with a scheme in
/// registered order. Errors only when *no* path answers.
///
/// `key` scopes the last-good cache (use the endpoint name). `insecure` disables
/// TLS verification on the probe — pass the endpoint's `insecure` flag so a
/// self-signed host is probed the way the plugin will later call it.
#[cfg(any(feature = "http", feature = "delegated-http"))]
pub async fn resolve_reachable(key: &str, routes: &[Route], insecure: bool) -> Result<String> {
    // Only URL-addressable routes (those with a scheme) are probable.
    let candidates: Vec<(String, &Route)> = routes
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| r.base_url().map(|u| (u, r)))
        .collect();
    if candidates.is_empty() {
        bail!(
            "endpoint '{key}' has no enabled URL-addressable routes; register one with `--route kind=scheme://value[:port]`"
        );
    }

    // Order: last-good first (if still present + enabled), then the rest in
    // registered order, de-duplicated by URL.
    let cached = last_good().lock().ok().and_then(|m| m.get(key).cloned());
    let mut order: Vec<&(String, &Route)> = Vec::with_capacity(candidates.len());
    if let Some(url) = &cached
        && let Some(hit) = candidates.iter().find(|(u, _)| u == url)
    {
        order.push(hit);
    }
    for c in &candidates {
        if !order.iter().any(|(u, _)| *u == c.0) {
            order.push(c);
        }
    }

    let mut tried: Vec<String> = Vec::new();
    for (url, route) in order {
        if probe(url, insecure).await {
            if let Ok(mut m) = last_good().lock() {
                m.insert(key.to_string(), url.clone());
            }
            return Ok(url.clone());
        }
        tried.push(format!("{}={}", route.kind, url));
    }
    bail!(
        "endpoint '{key}' unreachable on all {} registered path(s): {}",
        tried.len(),
        tried.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shorthand() {
        let r = parse_route("lan_v4=http://10.0.0.15:8989").unwrap();
        assert_eq!(r.kind, "lan_v4");
        assert_eq!(r.scheme.as_deref(), Some("http"));
        assert_eq!(r.value, "10.0.0.15");
        assert_eq!(r.port, Some(8989));
        assert!(r.enabled);
    }

    #[test]
    fn parse_shorthand_no_port() {
        let r = parse_route("fqdn=https://sonarr.example.com").unwrap();
        assert_eq!(r.scheme.as_deref(), Some("https"));
        assert_eq!(r.value, "sonarr.example.com");
        assert_eq!(r.port, None);
    }

    #[test]
    fn parse_shorthand_ipv6() {
        let r = parse_route("lan_v6=http://[fd00::1]:80").unwrap();
        assert_eq!(r.value, "fd00::1");
        assert_eq!(r.port, Some(80));
    }

    #[test]
    fn parse_json() {
        let r = parse_route(
            r#"{"kind":"fqdn","scheme":"https","value":"x.example.com","enabled":false}"#,
        )
        .unwrap();
        assert_eq!(r.kind, "fqdn");
        assert!(!r.enabled);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_route("nope").is_err());
        assert!(parse_route("lan=notaurl").is_err());
        assert!(parse_route("=http://x").is_err());
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn no_enabled_routes_errors() {
        let routes = vec![Route {
            enabled: false,
            ..Route::new("lan_v4", "http", "127.0.0.1", Some(1))
        }];
        assert!(resolve_reachable("k", &routes, false).await.is_err());
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn first_reachable_wins() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let (scheme, value, port) = split_url(&server.uri()).unwrap();
        let routes = vec![
            Route::new("lan_v4", "http", "127.0.0.1", Some(1)),
            Route::new("fqdn", scheme, value, port),
        ];
        let url = resolve_reachable("fallthrough", &routes, false)
            .await
            .unwrap();
        assert_eq!(url, server.uri());
    }
}
