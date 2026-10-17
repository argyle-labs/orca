//! The registry of service INSTANCES — "this syncthing lives at these routes".
//!
//! Until this existed, orca knew service *providers* (what backends are loaded)
//! but nothing addressable to reach. `service.health` therefore iterated
//! providers and handed each an address-less `Endpoint`, so it could not work
//! for any provider no matter how well its backend was implemented (#615):
//!
//! ```text
//! syncthing instance '' has no URL-addressable route
//! ```
//!
//! Note the blank instance name — that is the empty endpoint talking.
//!
//! Instances are `(provider, instance)` addressed by an ordered `routes[]`, on
//! the shared, mesh-replicated `endpoints` table rather than a table of their
//! own: an instance registered on one host is reachable from any host, which is
//! the whole point of a fleet control plane. `provider` is the tag `service`,
//! and `name` is `<backend>/<instance>` so two providers may each have a `main`.
//!
//! Secrets are NOT stored here. A token belongs in the secret store; this row
//! carries only the non-secret descriptor.

use anyhow::Result;
use plugin_toolkit::endpoint_resource;

/// One registered service instance. `routes` is supplied by the macro as the
/// first-class ordered set — index 0 is primary — so nothing here is a scalar
/// URL.
#[endpoint_resource(plugin = "service.instance", lww = "updated_at")]
pub struct ServiceInstanceRow {
    /// `<backend>/<instance>` — e.g. `syncthing/willow`.
    pub name: String,
    /// Deploy-target host the instance runs on. Empty when already-running.
    pub host: String,
    /// Runtime (`docker`/`podman`/`lxc`/`vm`). Empty = the backend's default.
    pub runtime: String,
    /// Backup method override (`tar`/`pbs`). Empty = auto-select.
    pub method: String,
    pub enabled: bool,
}

/// Compose the registry key for a `(provider, instance)` pair.
///
/// Two providers may legitimately each call an instance `main`, so the backend
/// name is part of the key rather than assumed unique.
pub fn key(provider: &str, instance: &str) -> String {
    format!("{}/{}", provider.trim(), instance.trim())
}

/// Split a registry key back into `(provider, instance)`.
///
/// An instance name may itself contain `/`, so this splits on the FIRST
/// separator only — the provider never does.
pub fn split_key(key: &str) -> Option<(&str, &str)> {
    let (provider, instance) = key.split_once('/')?;
    (!provider.is_empty() && !instance.is_empty()).then_some((provider, instance))
}

/// Every registered instance of `provider`, in registration order.
pub fn instances_of(provider: &str) -> Result<Vec<EndpointRow>> {
    Ok(endpoint_db::list()?
        .into_iter()
        .filter(|r| r.enabled)
        .filter(|r| split_key(&r.name).is_some_and(|(p, _)| p == provider))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_keeps_two_providers_main_instances_apart() {
        assert_eq!(key("syncthing", "main"), "syncthing/main");
        assert_ne!(key("syncthing", "main"), key("audiobookshelf", "main"));
        // Whitespace is not part of an identity.
        assert_eq!(key(" syncthing ", " main "), "syncthing/main");
    }

    #[test]
    fn a_key_round_trips_and_an_instance_may_contain_a_separator() {
        assert_eq!(split_key("syncthing/main"), Some(("syncthing", "main")));
        // Split on the FIRST separator: the provider never contains one, an
        // instance name might.
        assert_eq!(split_key("syncthing/site/a"), Some(("syncthing", "site/a")));
        // Degenerate keys are not instances.
        assert_eq!(split_key("syncthing"), None);
        assert_eq!(split_key("/main"), None);
        assert_eq!(split_key("syncthing/"), None);
    }
}
