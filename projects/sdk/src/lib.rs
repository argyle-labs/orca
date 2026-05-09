//! Orca SDK — headless plugin and federation client library.
//!
//! This crate is the dependency surface for plugins, Meerkat, workers, and
//! native app shells. It must never depend on orca-server or anything that
//! requires the `ui` feature.
//!
//! ## Surface
//!
//! - [`pki`] — CA and node cert generation + loading
//! - [`transport`] — TCP+mTLS plugin transport and `orca/hello` handshake
//! - [`jsonrpc`] — JSON-RPC 2.0 wire types (shared with server plugin host)
//! - [`framing`] — length-prefixed frame encode/decode
//! - [`manifest`] — `orca-plugin.toml` types + parser (part of the contract)

/// SDK version — announced during the `orca/hello` handshake.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Flavor controls which transports and capabilities are compiled in.
#[derive(Default, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Flavor {
    /// Full SDK: all transports, all capability classes.
    Full,
    /// Headless only: TCP+mTLS. No UDP, no dashboard push.
    #[default]
    Headless,
    /// Local: same-host plugin, no network surface.
    Local,
}

pub mod conformance;
pub mod framing;
pub mod jsonrpc;
pub mod manifest;
pub mod pki;
pub mod tools;
pub mod transport;

// Re-export the tools surface so plugin authors get one canonical import path.
pub use tools::{
    RegisteredTool, TOOLS_CALL_METHOD, TOOLS_DECLARE_METHOD, ToolCallParams, ToolCallResult,
    ToolDeclaration, ToolFuture, ToolHandler, ToolHandlerError, ToolsDeclareParams,
    ToolsDeclareResult, tool_error_codes,
};

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
