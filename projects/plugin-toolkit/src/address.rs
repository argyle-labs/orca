//! Endpoint addressing with per-instance fallback.
//!
//! An endpoint may be reachable by several independent paths: a public
//! FQDN (typically via a reverse proxy), a LAN `IP:port`, or a Tailscale
//! address — which may be served by a *different* mesh node that exposes
//! the service to the tailnet. **No machine is assumed to have any
//! particular path.** Every [`Address`] is optional; the resolver simply
//! tries each enabled entry in registered order and uses whichever answers
//! first, so a missing FQDN, missing LAN route, or missing Tailscale node
//! just falls through to the next entry.
//!
//! This is the single connection-fallback primitive every
//! `endpoint_resource!` plugin inherits — `addresses` is a built-in column
//! on every endpoint, configurable per-instance over CLI / MCP / REST, and
//! resolution + last-good caching live here so a fix lands once for all
//! plugins. See [[feedback-self-healing-is-mandatory]].

#[cfg(feature = "http")]
use std::collections::HashMap;
#[cfg(feature = "http")]
use std::sync::{Mutex, OnceLock};
#[cfg(feature = "http")]
use std::time::Duration;

#[cfg(feature = "http")]
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One reachable path to an endpoint. `kind` is a free-form label
/// (`"fqdn"`, `"lan"`, `"tailscale"`, …) used only for display + operator
/// intent; resolution is driven purely by registered order and liveness.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Address {
    /// Free-form path label: `"fqdn"`, `"lan"`, `"tailscale"`, …
    pub kind: String,
    /// Base URL including scheme and optional port, e.g.
    /// `https://sonarr.example.com` or `http://10.0.0.15:8989`.
    pub url: String,
    /// When `false`, the resolver skips this entry without probing.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Address {
    /// Construct an enabled address.
    pub fn new(kind: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            url: url.into(),
            enabled: true,
        }
    }
}

/// clap `value_parser` for the repeatable `--address` flag. Accepts either
/// a full JSON object (`{"kind":"lan","url":"http://…","enabled":true}`) or
/// the shorthand `kind=url` (enabled defaults to true).
pub fn parse_address(s: &str) -> std::result::Result<Address, String> {
    let s = s.trim();
    if s.starts_with('{') {
        return serde_json::from_str::<Address>(s)
            .map_err(|e| format!("invalid address JSON: {e}"));
    }
    match s.split_once('=') {
        Some((kind, url)) if !kind.trim().is_empty() && !url.trim().is_empty() => {
            Ok(Address::new(kind.trim(), url.trim()))
        }
        _ => Err(format!("expected `kind=url` or a JSON object, got `{s}`")),
    }
}

#[cfg(feature = "http")]
fn last_good() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Probe a base URL: any HTTP response (even 401/404) proves the host is
/// reachable; only a transport error (connect refused, DNS failure,
/// timeout) counts as down. This is what lets resolution survive a broken
/// DNS path (e.g. AdGuard down) by falling through to a raw-IP entry.
///
/// HTTP-backed; gated with the `http` feature (default `full`). A storage-only
/// plugin parses/registers [`Address`]es but does not resolve reachability, so
/// it drops the utils http stack.
#[cfg(feature = "http")]
async fn reachable(client: &utils::http::Client, url: &str, insecure: bool) -> bool {
    match client
        .get(url)
        .insecure(insecure)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        // Any 2xx response: reachable.
        Ok(_) => true,
        // A non-2xx HTTP status still proves the host answered (e.g. 401/404
        // on the bare base URL) — that's reachable for fallback purposes.
        Err(utils::http::HttpError::Status { .. }) => true,
        // Transport failure (connect refused, DNS failure, timeout): down.
        Err(_) => false,
    }
}

/// Resolve the first reachable base URL for an endpoint, trying the
/// last-known-good path first, then every enabled [`Address`] in registered
/// order. Errors only when *no* path answers — the caller then knows the
/// service is genuinely unreachable rather than merely missing one path.
///
/// `key` scopes the last-good cache (use the endpoint name).
///
/// `insecure` disables TLS certificate verification on the reachability probe —
/// pass the endpoint's `insecure` flag so a self-signed host (e.g. a default
/// Proxmox VE cert) is probed the same way the plugin will later call it, rather
/// than reading as unreachable on a cert rejection.
#[cfg(feature = "http")]
pub async fn resolve_reachable(key: &str, addresses: &[Address], insecure: bool) -> Result<String> {
    let enabled: Vec<&Address> = addresses.iter().filter(|a| a.enabled).collect();
    if enabled.is_empty() {
        bail!("endpoint '{key}' has no enabled addresses; register one with `--address kind=url`");
    }

    // Order: last-good first (if still registered + enabled), then the rest
    // in registered order, de-duplicated by URL.
    let cached = last_good().lock().ok().and_then(|m| m.get(key).cloned());
    let mut order: Vec<&Address> = Vec::with_capacity(enabled.len());
    if let Some(url) = &cached
        && let Some(a) = enabled.iter().find(|a| &a.url == url)
    {
        order.push(a);
    }
    for a in &enabled {
        if !order.iter().any(|x| x.url == a.url) {
            order.push(a);
        }
    }

    let client = utils::http::Client::new();
    let mut tried: Vec<String> = Vec::new();
    for a in order {
        if reachable(&client, &a.url, insecure).await {
            if let Ok(mut m) = last_good().lock() {
                m.insert(key.to_string(), a.url.clone());
            }
            return Ok(a.url.clone());
        }
        tried.push(format!("{}={}", a.kind, a.url));
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
        let a = parse_address("lan=http://10.0.0.15:8989").unwrap();
        assert_eq!(a.kind, "lan");
        assert_eq!(a.url, "http://10.0.0.15:8989");
        assert!(a.enabled);
    }

    #[test]
    fn parse_json() {
        let a = parse_address(r#"{"kind":"fqdn","url":"https://x.example.com","enabled":false}"#)
            .unwrap();
        assert_eq!(a.kind, "fqdn");
        assert!(!a.enabled);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_address("nope").is_err());
        assert!(parse_address("=http://x").is_err());
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn no_enabled_addresses_errors() {
        let addrs = vec![Address {
            kind: "lan".into(),
            url: "http://127.0.0.1:1".into(),
            enabled: false,
        }];
        assert!(resolve_reachable("k", &addrs, false).await.is_err());
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn first_reachable_wins() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let addrs = vec![
            // Dead path first → must fall through.
            Address::new("lan", "http://127.0.0.1:1"),
            // Live path (404 still proves reachability).
            Address::new("fqdn", server.uri()),
        ];
        let url = resolve_reachable("fallthrough", &addrs, false)
            .await
            .unwrap();
        assert_eq!(url, server.uri());
    }
}
