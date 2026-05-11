//! Plugin manifest — `orca-plugin.toml`.
//!
//! The manifest is part of the plugin contract, on equal footing with the
//! JSON-RPC method types. Every plugin ships an `orca-plugin.toml`; the host
//! reads it to decide how to spawn and route, and per-language SDK ports
//! (Go, Kotlin, TypeScript) reproduce the same shape.
//!
//! ## v0 shape
//!
//! ```toml
//! [plugin]
//! id               = "alpha"
//! version          = "0.1.0"
//! min_orca_version = "0.1.0"
//!
//! [runtime]
//! binary = "./bin/alpha"   # mutually exclusive with `image`
//! # image = "ghcr.io/org/alpha:0.1"
//! mode   = "process"        # only "process" is supported in v0
//! eager  = false             # true → start at host boot; false → lazy-spawn
//!
//! [surfaces]
//! mcp        = true
//! cli        = false
//! ui         = false
//! docs       = false
//! jobs       = false
//! storage    = false
//! federation = false
//!
//! [[capabilities]]
//! name        = "context.publish"
//! sensitivity = "general"
//!
//! [[capabilities]]
//! name        = "atlassian.read"
//! sensitivity = "sensitive"
//! ```
//!
//! The shape is deliberately minimal. UI contributions, nav entries,
//! storage table declarations, and job schedules are intentionally NOT in
//! v0 — they'll arrive as additive sections once the surfaces that consume
//! them exist on the host side. Adding fields is non-breaking; renaming or
//! removing fields is breaking and bumps the manifest version.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::pki::Capability as PkiCapability;

/// Parsed `orca-plugin.toml`. Returned by [`parse_str`] and [`parse_path`].
///
/// Field defaults match the documented v0 shape. Unknown TOML keys are
/// rejected (`#[serde(deny_unknown_fields)]`) so plugin authors get a clear
/// error when they target a newer manifest schema than the SDK supports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plugin: PluginSection,
    pub runtime: RuntimeSection,
    #[serde(default)]
    pub surfaces: SurfacesSection,
    #[serde(default)]
    pub capabilities: Vec<CapabilityDecl>,
    /// Peer plugins this plugin needs at runtime. The host enforces presence
    /// of required deps before the plugin's tools are dispatchable; optional
    /// deps degrade rather than reject. Plugins consume peers via
    /// `transport.invoke_tool` (or, idiomatically, the dep's published
    /// `client/` package) and do not need to know the peer's transport
    /// address — the host owns the dispatch.
    #[serde(default, rename = "depends_on")]
    pub depends_on: Vec<PluginDependency>,
}

/// `[plugin]` — identity and version compatibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSection {
    /// Stable plugin id. Must match the Subject CN of the plugin's mTLS cert
    /// (the host enforces this in `orca/hello`).
    pub id: String,
    /// Plugin version. Dotted-numeric semver; pre-release/build tags rejected.
    pub version: String,
    /// Minimum orca core version this plugin requires. Sent in `orca/hello`
    /// as `core_min_required`.
    pub min_orca_version: String,
}

/// `[runtime]` — how the host spawns and supervises the plugin process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSection {
    /// Path (relative to the manifest) to the plugin binary. Mutually
    /// exclusive with [`image`](Self::image).
    #[serde(default)]
    pub binary: Option<String>,
    /// Container image reference. Mutually exclusive with
    /// [`binary`](Self::binary).
    #[serde(default)]
    pub image: Option<String>,
    /// Spawn mode. v0 supports only `"process"`.
    #[serde(default = "default_mode")]
    pub mode: RuntimeMode,
    /// `true` → host launches the plugin at boot. `false` (default) →
    /// host lazy-spawns on first surface invocation.
    #[serde(default)]
    pub eager: bool,
}

fn default_mode() -> RuntimeMode {
    RuntimeMode::Process
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    Process,
}

/// `[surfaces]` — which contract surfaces the plugin participates in. All
/// fields default to `false` so a manifest only opts in to what it uses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SurfacesSection {
    pub mcp: bool,
    pub cli: bool,
    pub ui: bool,
    pub docs: bool,
    pub jobs: bool,
    pub storage: bool,
    pub federation: bool,
}

/// `[[depends_on]]` — declares a peer plugin this plugin needs at runtime.
///
/// `optional = true` lets the host start this plugin even if the dep is
/// missing — used for graceful-degrade paths. The default is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginDependency {
    /// Plugin id of the peer (matches the peer's `plugin.id`).
    pub id: String,
    /// Minimum semver of the peer that satisfies this dependency.
    pub min_version: String,
    /// `true` → host may start this plugin without the peer; consumer is
    /// expected to handle the missing-peer case (e.g. degrade tools).
    /// `false` (default) → host rejects/degrades this plugin's hello until
    /// the peer is connected.
    #[serde(default)]
    pub optional: bool,
}

/// `[[capabilities]]` — what the plugin can do, and at what sensitivity.
/// Reuses [`PkiCapability`] for the sensitivity tier so manifest and cert
/// classification can never drift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityDecl {
    pub name: String,
    pub sensitivity: PkiCapability,
}

/// Parse a manifest from a TOML string.
pub fn parse_str(s: &str) -> Result<Manifest> {
    let manifest: Manifest = toml::from_str(s).context("parse orca-plugin.toml")?;
    manifest.validate()?;
    Ok(manifest)
}

/// Parse a manifest from a file on disk.
pub fn parse_path(path: &Path) -> Result<Manifest> {
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("read manifest at {}", path.display()))?;
    parse_str(&s)
}

impl Manifest {
    /// Conventional manifest filename.
    pub const FILENAME: &'static str = "orca-plugin.toml";

    /// Cross-field validation — runs after serde structural parsing.
    /// Catches things serde can't: empty ids, version format, mutually
    /// exclusive fields, duplicate capabilities.
    fn validate(&self) -> Result<()> {
        if self.plugin.id.trim().is_empty() {
            bail!("plugin.id must not be empty");
        }
        if self
            .plugin
            .id
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\')
        {
            bail!(
                "plugin.id '{}' contains invalid characters (whitespace or path separators)",
                self.plugin.id
            );
        }
        check_semver(&self.plugin.version, "plugin.version")?;
        check_semver(&self.plugin.min_orca_version, "plugin.min_orca_version")?;

        match (&self.runtime.binary, &self.runtime.image) {
            (Some(_), Some(_)) => bail!("runtime.binary and runtime.image are mutually exclusive"),
            (None, None) => bail!("runtime requires either `binary` or `image`"),
            _ => {}
        }

        let mut seen = std::collections::HashSet::new();
        for cap in &self.capabilities {
            if cap.name.trim().is_empty() {
                bail!("capability.name must not be empty");
            }
            if !seen.insert(cap.name.as_str()) {
                bail!("duplicate capability '{}'", cap.name);
            }
        }

        let mut dep_ids = std::collections::HashSet::new();
        for dep in &self.depends_on {
            if dep.id.trim().is_empty() {
                bail!("depends_on.id must not be empty");
            }
            if dep.id == self.plugin.id {
                bail!("plugin '{}' cannot depend on itself", self.plugin.id);
            }
            if !dep_ids.insert(dep.id.as_str()) {
                bail!("duplicate dependency on '{}'", dep.id);
            }
            check_semver(
                &dep.min_version,
                &format!("depends_on[{}].min_version", dep.id),
            )?;
        }

        Ok(())
    }
}

fn check_semver(v: &str, field: &str) -> Result<()> {
    if v.contains('-') || v.contains('+') {
        bail!("{field} '{v}': pre-release/build metadata not supported in v0");
    }
    let parts: Vec<&str> = v.split('.').collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        bail!("{field} '{v}': empty component");
    }
    for p in parts {
        p.parse::<u64>()
            .with_context(|| format!("{field} '{v}': bad numeric component '{p}'"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical fixture used both here and (eventually) by the conformance
    /// suite. Every language SDK port must accept this manifest verbatim.
    const CANONICAL: &str = r#"
[plugin]
id               = "alpha"
version          = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./bin/alpha"
mode   = "process"
eager  = false

[surfaces]
mcp = true

[[capabilities]]
name        = "context.publish"
sensitivity = "general"

[[capabilities]]
name        = "atlassian.read"
sensitivity = "sensitive"
"#;

    #[test]
    fn parses_canonical_fixture() {
        let m = parse_str(CANONICAL).unwrap();
        assert_eq!(m.plugin.id, "alpha");
        assert_eq!(m.plugin.version, "0.1.0");
        assert_eq!(m.plugin.min_orca_version, "0.1.0");
        assert_eq!(m.runtime.binary.as_deref(), Some("./bin/alpha"));
        assert!(m.runtime.image.is_none());
        assert_eq!(m.runtime.mode, RuntimeMode::Process);
        assert!(!m.runtime.eager);
        assert!(m.surfaces.mcp);
        assert!(!m.surfaces.cli);
        assert_eq!(m.capabilities.len(), 2);
        assert_eq!(m.capabilities[0].name, "context.publish");
        assert_eq!(m.capabilities[0].sensitivity, PkiCapability::General);
        assert_eq!(m.capabilities[1].sensitivity, PkiCapability::Sensitive);
    }

    #[test]
    fn round_trips_canonical_fixture_through_serde() {
        let m = parse_str(CANONICAL).unwrap();
        let s = toml::to_string(&m).unwrap();
        let m2 = parse_str(&s).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let s = format!("{CANONICAL}\n[bogus]\nx = 1\n");
        let err = parse_str(&s).unwrap_err();
        assert!(format!("{err:#}").contains("bogus"));
    }

    #[test]
    fn rejects_when_neither_binary_nor_image_given() {
        let s = r#"
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
"#;
        let err = parse_str(s).unwrap_err();
        assert!(format!("{err:#}").contains("binary"));
    }

    #[test]
    fn rejects_when_both_binary_and_image_given() {
        let s = r#"
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./bin/x"
image  = "ghcr.io/x:1"
"#;
        let err = parse_str(s).unwrap_err();
        assert!(format!("{err:#}").contains("mutually exclusive"));
    }

    #[test]
    fn rejects_bad_id() {
        // TOML literal strings (single-quoted) avoid escape-sequence
        // ambiguity for the backslash case.
        for bad in &["''", "'has space'", "'has/slash'", "'has\\slash'"] {
            let s = format!(
                r#"
[plugin]
id = {bad}
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
"#
            );
            let err = parse_str(&s).unwrap_err();
            assert!(
                format!("{err:#}").contains("plugin.id"),
                "expected plugin.id error for {bad}"
            );
        }
    }

    #[test]
    fn rejects_bad_semver() {
        let s = r#"
[plugin]
id = "x"
version = "0.1.0-rc1"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
"#;
        let err = parse_str(s).unwrap_err();
        assert!(format!("{err:#}").contains("pre-release"));
    }

    #[test]
    fn rejects_duplicate_capabilities() {
        let s = r#"
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"

[[capabilities]]
name        = "thing"
sensitivity = "general"

[[capabilities]]
name        = "thing"
sensitivity = "sensitive"
"#;
        let err = parse_str(s).unwrap_err();
        assert!(format!("{err:#}").contains("duplicate"));
    }

    #[test]
    fn surfaces_default_to_false() {
        let s = r#"
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
"#;
        let m = parse_str(s).unwrap();
        assert!(!m.surfaces.mcp);
        assert!(!m.surfaces.federation);
    }

    #[test]
    fn parse_path_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(Manifest::FILENAME);
        std::fs::write(&path, CANONICAL).unwrap();
        let m = parse_path(&path).unwrap();
        assert_eq!(m.plugin.id, "alpha");
    }
}
