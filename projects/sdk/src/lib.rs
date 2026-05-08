//! Orca SDK — headless plugin and federation client library.
//!
//! This crate is the dependency surface for Meerkat, workers, and native app
//! shells. It must never depend on orca-server or anything that requires the
//! `ui` feature.
//!
//! ## Planned surface (not yet implemented)
//!
//! - Unix socket transport (local plugin ↔ core)
//! - TCP + mTLS transport (remote plugin / federation node)
//! - UDP + DTLS transport (telemetry, presence beacons)
//! - `orca/*` JSON-RPC extension methods (hello, types.declare, context.subscribe)
//! - Typed Context / TypedValue system
//! - Version negotiation / drift enforcement

/// SDK version — announced during the connection handshake.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Flavors control which transports and capabilities are compiled in.
#[derive(Default, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Flavor {
    /// Full SDK: all transports, all capability classes.
    Full,
    /// Headless only: Unix + TCP+mTLS. No UDP, no dashboard push.
    #[default]
    Headless,
    /// Minimal: Unix socket only. For same-host plugins with no network surface.
    Local,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_version_is_non_empty() {
        assert!(!SDK_VERSION.is_empty());
    }

    #[test]
    fn flavor_default_is_headless() {
        assert_eq!(Flavor::default(), Flavor::Headless);
    }

    #[test]
    fn flavor_roundtrips_json() {
        let flavors = [Flavor::Full, Flavor::Headless, Flavor::Local];
        for f in &flavors {
            let json = serde_json::to_string(f).unwrap();
            let back: Flavor = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, f);
        }
    }
}
