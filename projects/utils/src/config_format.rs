//! Syntax validation for managed config files — the one place that knows how to
//! tell "this text parses as its declared format" from "this text does not".
//!
//! Exists because a service will happily keep running on a config file it could
//! not parse: `/etc/jellyfin/network.xml` on frigg had every attribute quote
//! stripped by an over-eager `sed` on 2026-03-15, Jellyfin logged
//! `Error loading configuration file`, fell back to defaults, and silently
//! discarded six months of real config (`KnownProxies`, `LocalNetworkSubnets`).
//! The service was "healthy" the whole time. orca must never be the thing that
//! leaves a service in that state, so every managed config write validates first.
//!
//! Deliberately domain-free: formats only, no schemas, no knowledge of which
//! service owns which file. Callers map a path to a format and get a parse
//! verdict; an unrecognised extension is never an error (see [`validate_for_path`]).

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

/// A config text format orca can check for syntactic validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Yaml,
    Toml,
    Xml,
}

impl ConfigFormat {
    /// Lower-case name used in error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigFormat::Json => "json",
            ConfigFormat::Yaml => "yaml",
            ConfigFormat::Toml => "toml",
            ConfigFormat::Xml => "xml",
        }
    }
}

/// Infer the format from a path's extension, case-insensitively. `None` means
/// "orca does not know this format" — never "invalid".
pub fn from_path(path: &Path) -> Option<ConfigFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "json" => Some(ConfigFormat::Json),
        "yaml" | "yml" => Some(ConfigFormat::Yaml),
        "toml" => Some(ConfigFormat::Toml),
        "xml" => Some(ConfigFormat::Xml),
        _ => None,
    }
}

/// Check that `bytes` parse as `fmt`. The parser's own message is preserved in
/// the error chain — an operator needs the line/column, not "invalid config".
pub fn validate(fmt: ConfigFormat, bytes: &[u8]) -> Result<()> {
    match fmt {
        // `IgnoredAny` over a `Value`: syntax is all we check, so there is no
        // reason to allocate a document tree (and no opaque JSON in the tree).
        ConfigFormat::Json => {
            serde_json::from_slice::<serde::de::IgnoredAny>(bytes).map(|_| ())?;
        }
        ConfigFormat::Yaml => {
            serde_yaml::from_slice::<serde::de::IgnoredAny>(bytes).map(|_| ())?;
        }
        ConfigFormat::Toml => {
            let text = as_text(fmt, bytes)?;
            toml::from_str::<serde::de::IgnoredAny>(text).map(|_| ())?;
        }
        ConfigFormat::Xml => {
            let text = as_text(fmt, bytes)?;
            // Well-formedness only — no schema/DTD. roxmltree is strict enough to
            // reject the unquoted-attribute corruption that started all this.
            roxmltree::Document::parse(text).map(|_| ())?;
        }
    }
    Ok(())
}

/// Validate `bytes` against the format implied by `path`'s extension.
///
/// `Ok(())` when the extension is unrecognised: not knowing a format may never
/// be a reason to block a write. Only a *recognised* format that *fails to
/// parse* is an error, and the error names the format, the path, and the parse
/// failure so an operator can see what to fix.
pub fn validate_for_path(path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(fmt) = from_path(path) else {
        return Ok(());
    };
    validate(fmt, bytes).with_context(|| {
        format!(
            "{} is not valid {}: refusing to write a config the service cannot parse",
            path.display(),
            fmt.as_str()
        )
    })
}

/// Decode bytes the text-only parsers need. Non-UTF-8 XML (a declared legacy
/// encoding) is a genuine "cannot check", not a corruption — say so rather than
/// failing the write on our own limitation.
fn as_text(fmt: ConfigFormat, bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes)
        .map_err(|e| anyhow!("{} content is not valid UTF-8: {e}", fmt.as_str()))
}

/// Atomic write gated on the contents parsing as the format `path` declares.
/// Prefer this over [`crate::atomic::write`] for anything a service reads as
/// config; plain `write` stays the primitive for opaque bytes (PEM, keys, blobs).
pub fn write_validated(path: &Path, contents: &[u8]) -> Result<()> {
    validate_for_path(path, contents)?;
    crate::atomic::write(path, contents)
}

/// Re-check a file already on disk. Used for the "after" half of a managed
/// write, and by health checks asking "is the service running the config we
/// think it is". A missing file is an error here — the caller expected one.
pub fn validate_file(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.is_empty() && from_path(path).is_some() {
        bail!("{} is empty: a config file with no content", path.display());
    }
    validate_for_path(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The shape of the file that broke: a real-ish Jellyfin `network.xml`.
    /// IPs are 10.0.0.x — never real fleet addresses in fixtures.
    const NETWORK_XML_GOOD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<NetworkConfiguration xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <EnableHttps>false</EnableHttps>
  <PublicHttpPort>8096</PublicHttpPort>
  <LocalNetworkSubnets>
    <string>10.0.0.0/24</string>
  </LocalNetworkSubnets>
  <KnownProxies>
    <string>10.0.0.5</string>
  </KnownProxies>
  <PublishedServerUriBySubnet>
    <string>10.0.0.0/24=http://10.0.0.13:8096</string>
  </PublishedServerUriBySubnet>
</NetworkConfiguration>
"#;

    /// The exact corruption found on frigg CT113: every attribute quote stripped.
    const NETWORK_XML_QUOTES_STRIPPED: &str = r#"<?xml version=1.0 encoding=utf-8?>
<NetworkConfiguration xmlns:xsd=http://www.w3.org/2001/XMLSchema>
  <EnableHttps>false</EnableHttps>
  <KnownProxies>
    <string>10.0.0.5</string>
  </KnownProxies>
</NetworkConfiguration>
"#;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn extension_maps_to_format_case_insensitively() {
        assert_eq!(from_path(&p("a.json")), Some(ConfigFormat::Json));
        assert_eq!(from_path(&p("a.YAML")), Some(ConfigFormat::Yaml));
        assert_eq!(from_path(&p("a.yml")), Some(ConfigFormat::Yaml));
        assert_eq!(from_path(&p("a.Toml")), Some(ConfigFormat::Toml));
        assert_eq!(
            from_path(&p("/etc/jellyfin/network.XML")),
            Some(ConfigFormat::Xml)
        );
        assert_eq!(from_path(&p("a.pem")), None);
        assert_eq!(from_path(&p("noext")), None);
    }

    #[test]
    fn json_valid_and_invalid() {
        validate(ConfigFormat::Json, br#"{"a": [1, 2], "b": null}"#).unwrap();
        assert!(validate(ConfigFormat::Json, br#"{"a": [1, 2,}"#).is_err());
    }

    #[test]
    fn yaml_valid_and_invalid() {
        validate(ConfigFormat::Yaml, b"a: 1\nb:\n  - x\n  - y\n").unwrap();
        // Unclosed flow sequence — not recoverable as a scalar.
        assert!(validate(ConfigFormat::Yaml, b"a: [1, 2\nb: {").is_err());
    }

    #[test]
    fn toml_valid_and_invalid() {
        validate(ConfigFormat::Toml, b"[server]\nport = 8096\n").unwrap();
        assert!(validate(ConfigFormat::Toml, b"[server\nport = 8096\n").is_err());
    }

    #[test]
    fn xml_valid_and_invalid() {
        validate(ConfigFormat::Xml, NETWORK_XML_GOOD.as_bytes()).unwrap();
        // Mismatched close tag.
        assert!(validate(ConfigFormat::Xml, b"<a><b></a></b>").is_err());
    }

    #[test]
    fn the_jellyfin_corruption_is_rejected() {
        let err = validate_for_path(
            &p("/etc/jellyfin/network.xml"),
            NETWORK_XML_QUOTES_STRIPPED.as_bytes(),
        )
        .expect_err("stripped attribute quotes must not pass as well-formed XML");
        let msg = format!("{err:#}");
        assert!(msg.contains("network.xml"), "error names the path: {msg}");
        assert!(msg.contains("xml"), "error names the format: {msg}");
    }

    #[test]
    fn a_good_network_xml_passes_for_its_path() {
        validate_for_path(&p("/etc/jellyfin/network.xml"), NETWORK_XML_GOOD.as_bytes()).unwrap();
    }

    #[test]
    fn unknown_extension_passes_through_untouched() {
        // A PEM body is not valid JSON/XML/TOML; an unknown extension may never
        // block a write.
        validate_for_path(
            &p("/etc/ssl/mesh.pem"),
            b"-----BEGIN CERTIFICATE-----\nAA==\n",
        )
        .unwrap();
        validate_for_path(&p("/etc/hosts"), b"10.0.0.1 gateway\n").unwrap();
        validate_for_path(&p("/opt/app/blob.bin"), &[0xff, 0x00, 0xfe]).unwrap();
    }

    #[test]
    fn non_utf8_in_a_known_text_format_is_reported_not_silently_accepted() {
        assert!(validate(ConfigFormat::Toml, &[0xff, 0xfe, b'a']).is_err());
    }

    #[test]
    fn write_validated_refuses_bad_and_accepts_good() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.xml");
        assert!(write_validated(&path, NETWORK_XML_QUOTES_STRIPPED.as_bytes()).is_err());
        assert!(!path.exists(), "a refused write must not touch the target");
        write_validated(&path, NETWORK_XML_GOOD.as_bytes()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), NETWORK_XML_GOOD.as_bytes());
    }

    #[test]
    fn validate_file_catches_an_already_broken_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.xml");
        std::fs::write(&path, NETWORK_XML_QUOTES_STRIPPED).unwrap();
        assert!(validate_file(&path).is_err());
        std::fs::write(&path, NETWORK_XML_GOOD).unwrap();
        validate_file(&path).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert!(
            validate_file(&path).is_err(),
            "an empty config is a lost config"
        );
    }
}
