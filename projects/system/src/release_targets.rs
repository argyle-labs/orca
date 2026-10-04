//! **Single source of truth** for the first-party plugin release target matrix
//! and the rules for resolving a release asset to *this* daemon's host.
//!
//! Historically the set of target triples lived in two places that could drift:
//! the shared reusable release workflow
//! (`argyle-labs/.github/.github/workflows/plugin-release.yml`, which decides
//! *what gets built*) and the fetch path in [`crate::plugin_fetch`] (which
//! decides *what gets downloaded*). This module makes orca core the canonical
//! owner of that matrix so the workflow mirrors orca — never the reverse.
//!
//! ## What is centralized here (clearly-safe, implemented)
//!
//! * [`TARGET_TRIPLES`] — the release triples every first-party plugin RC
//!   must publish (musl-static x86_64 linux + aarch64 darwin), plus
//!   [`OPTIONAL_TARGET_TRIPLES`] it may. The reusable workflow's default
//!   `targets` array MUST cover the required list; they are kept in
//!   sync by hand today and by a CI mirror check tomorrow (see the structural
//!   TODO below).
//! * [`linux_asset_candidates`] — the **linux asset-resolution fallback order**.
//!   A linux daemon prefers the *musl-static* asset for its arch (a static musl
//!   binary carries its own libc and runs on both musl and glibc hosts) and
//!   falls back to the matching *gnu* asset. Non-linux daemons (darwin) resolve
//!   to their own single triple with no fallback.
//! * [`is_release_triple`] — membership test used by catalog / asset-completeness
//!   validation.
//!
//! ## What is scaffolded here (structural — flagged, NOT forced)
//!
//! * [`validate_catalog_schema`] — a *pure* schema/consistency validator over
//!   catalog entries. Wiring it into the embedded-catalog load path and into a
//!   build-time test is the structural step; see its doc-comment TODO.
//! * [`RcAssetCompleteness`] — the shape of an "does this release publish all
//!   N×2 assets?" check (one binary + one sha256 per triple). The
//!   network-touching half must live next to
//!   `plugin_fetch`'s HTTP client and is deliberately left as a TODO so this
//!   module stays pure and unit-testable.

/// The triples every first-party plugin release MUST publish. **Canonical.**
/// The shared reusable release workflow's default `targets` array must cover
/// this set.
///
/// x86_64 linux is served by the musl-static asset alone: a static musl binary
/// carries its own libc and runs on glibc hosts (proxmox/LXC/unraid/gaming) as
/// well as Alpine ones, and [`linux_asset_candidates`] already prefers it. The
/// gnu asset is therefore optional for plugins (see [`OPTIONAL_TARGET_TRIPLES`]).
/// The orca daemon's own release still ships gnu; this matrix is plugins only.
/// aarch64 darwin is the Mac controller. Other triples (aarch64-linux,
/// x86_64-darwin) are not built until a host that needs them exists.
///
/// Callers that need a stable set should not depend on order.
pub const TARGET_TRIPLES: &[&str] = &["x86_64-unknown-linux-musl", "aarch64-apple-darwin"];

/// Triples a plugin release MAY publish but is not required to. A release that
/// still ships them stays valid, and fetch falls back to gnu when a release has
/// no musl asset (releases cut before musl existed).
pub const OPTIONAL_TARGET_TRIPLES: &[&str] = &["x86_64-unknown-linux-gnu"];

/// `true` if `triple` is a first-party plugin release target, required or
/// optional.
pub fn is_release_triple(triple: &str) -> bool {
    TARGET_TRIPLES.contains(&triple) || OPTIONAL_TARGET_TRIPLES.contains(&triple)
}

/// The CPU-arch prefix of a Rust target triple (`"x86_64"`, `"aarch64"`), or
/// `None` if the triple is malformed (no `-`).
fn arch_of(triple: &str) -> Option<&str> {
    triple.split('-').next().filter(|a| !a.is_empty())
}

/// Ordered list of asset triples this daemon may consume, most-preferred first.
///
/// This is the **linux asset-resolution fallback** — the single rule the fetch
/// path uses to decide which release asset matches the host:
///
/// * **linux** (`build_target` contains `-linux-`): prefer the *musl-static*
///   asset for the daemon's arch, then fall back to the *gnu* asset for the same
///   arch. A static musl binary runs on both musl (Alpine) and glibc hosts, so
///   preferring it means one asset satisfies the most hosts; the gnu fallback
///   covers releases cut before musl assets existed (older RCs) or any release
///   that only shipped gnu.
/// * **non-linux** (darwin, etc.): resolve to the daemon's own triple only. No
///   libc axis to fall back across.
///
/// Returns an empty vec for a malformed/unknown triple so callers surface a
/// clear "no candidate" error rather than downloading the wrong artifact.
pub fn linux_asset_candidates(build_target: &str) -> Vec<String> {
    if build_target == "unknown-target" || build_target.is_empty() {
        return Vec::new();
    }
    // Only linux has the gnu/musl split we fall back across.
    if !build_target.contains("-linux-") {
        return vec![build_target.to_string()];
    }
    let Some(arch) = arch_of(build_target) else {
        return Vec::new();
    };
    let musl = format!("{arch}-unknown-linux-musl");
    let gnu = format!("{arch}-unknown-linux-gnu");
    // Preferred first (musl-static), then the gnu fallback. If the daemon was
    // itself built for gnu, the musl-static asset still runs (static libc), so
    // preferring musl here is safe and maximizes asset reuse.
    let mut out = vec![musl];
    if gnu != out[0] {
        out.push(gnu);
    }
    out
}

// ── Scaffolded / structural centralizations (flagged, not forced) ───────────

/// A single catalog-entry validation problem, as text (kept dependency-free so
/// this can run at build time and in a unit test without pulling serde_json's
/// error types through the signature).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIssue {
    pub entry: String,
    pub problem: String,
}

/// Pure schema/consistency validation for one catalog entry's fields.
///
/// Checks the invariants documented in `plugin_catalog.json`'s header comment:
/// non-empty name/targetSoftware, `name == targetSoftware`, a `github.com`
/// `repoUrl`, a non-empty `docsUrl`, and a `status` in the known set.
///
/// TODO(structural): wire this into (a) `plugin_manager::catalog()` so a
/// malformed embedded catalog fails loudly at load, and (b) a `#[test]` that
/// runs it over every embedded entry so a bad PR can't merge. Both are
/// mechanical but touch the catalog load path's error type, so they are left as
/// a follow-up rather than forced in this consolidation PR.
pub fn validate_catalog_entry(
    name: &str,
    target_software: &str,
    repo_url: &str,
    docs_url: &str,
    status: &str,
) -> Vec<CatalogIssue> {
    const KNOWN_STATUS: &[&str] = &["available", "unreleased", "planned"];
    let mut issues = Vec::new();
    let mut push = |p: &str| {
        issues.push(CatalogIssue {
            entry: name.to_string(),
            problem: p.to_string(),
        })
    };
    if name.trim().is_empty() {
        push("name is empty");
    }
    if target_software.trim().is_empty() {
        push("targetSoftware is empty");
    }
    if !name.trim().is_empty() && name != target_software {
        push("name must equal targetSoftware (one catalog entry == one plugin artifact)");
    }
    if !repo_url.starts_with("https://github.com/") {
        push("repoUrl must be a https://github.com/ URL (no hostnames or IPs)");
    }
    if docs_url.trim().is_empty() {
        push("docsUrl is empty");
    }
    if !KNOWN_STATUS.contains(&status) {
        push("status must be one of: available, unreleased, planned");
    }
    issues
}

/// The result shape of a per-release asset-completeness check: for a given
/// plugin `name` + release `tag`, which of the required `TARGET_TRIPLES` are missing
/// either their binary or their `.sha256`.
///
/// TODO(structural): the actual check needs a GitHub release's asset list,
/// which only [`crate::plugin_fetch`] has an authenticated client for. Rather
/// than pull HTTP into this pure module, expose a helper *there* that lists a
/// release's asset names and calls [`missing_release_assets`] below. Left as a
/// follow-up: it is a genuine new network path with its own error/timeout
/// envelope and deserves its own review, not a rider on this PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcAssetCompleteness {
    pub name: String,
    pub tag: String,
    pub missing: Vec<String>,
}

/// Pure core of the asset-completeness check: given the set of asset filenames
/// present on a release and the plugin `name`+`version`, return the triples
/// whose binary and/or `.sha256` is absent. `version` is the tag with any
/// leading `v` stripped (matching `plugin_fetch::asset_name`).
///
/// This is the safe, testable half; the network half is the TODO above.
pub fn missing_release_assets(
    name: &str,
    version: &str,
    present_asset_names: &[String],
) -> Vec<String> {
    let mut missing = Vec::new();
    for triple in TARGET_TRIPLES {
        let bin = format!("{name}-v{version}-{triple}");
        let sha = format!("{bin}.sha256");
        let has_bin = present_asset_names.iter().any(|a| a == &bin);
        let has_sha = present_asset_names.iter().any(|a| a == &sha);
        if !has_bin || !has_sha {
            missing.push((*triple).to_string());
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_and_optional_triples_are_unique_and_disjoint() {
        let mut all: Vec<&str> = TARGET_TRIPLES
            .iter()
            .chain(OPTIONAL_TARGET_TRIPLES)
            .copied()
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "a triple is listed twice");
    }

    #[test]
    fn plugins_require_musl_and_darwin_and_may_ship_gnu() {
        assert_eq!(
            TARGET_TRIPLES,
            ["x86_64-unknown-linux-musl", "aarch64-apple-darwin"]
        );
        assert!(is_release_triple("x86_64-unknown-linux-gnu"));
        assert!(!TARGET_TRIPLES.contains(&"x86_64-unknown-linux-gnu"));
        // Intentionally not built until a host needs them.
        assert!(!is_release_triple("aarch64-unknown-linux-gnu"));
        assert!(!is_release_triple("aarch64-unknown-linux-musl"));
        assert!(!is_release_triple("x86_64-apple-darwin"));
    }

    #[test]
    fn linux_prefers_musl_static_then_gnu() {
        assert_eq!(
            linux_asset_candidates("x86_64-unknown-linux-gnu"),
            vec![
                "x86_64-unknown-linux-musl".to_string(),
                "x86_64-unknown-linux-gnu".to_string(),
            ]
        );
        assert_eq!(
            linux_asset_candidates("aarch64-unknown-linux-musl"),
            vec![
                "aarch64-unknown-linux-musl".to_string(),
                "aarch64-unknown-linux-gnu".to_string(),
            ]
        );
    }

    #[test]
    fn darwin_resolves_to_itself_only() {
        assert_eq!(
            linux_asset_candidates("aarch64-apple-darwin"),
            vec!["aarch64-apple-darwin".to_string()]
        );
    }

    #[test]
    fn unknown_target_yields_no_candidates() {
        assert!(linux_asset_candidates("unknown-target").is_empty());
        assert!(linux_asset_candidates("").is_empty());
    }

    #[test]
    fn catalog_validation_flags_mismatches() {
        // Clean entry — no issues.
        assert!(
            validate_catalog_entry(
                "ntfy",
                "ntfy",
                "https://github.com/argyle-labs/ntfy",
                "https://github.com/argyle-labs/ntfy#readme",
                "available",
            )
            .is_empty()
        );
        // name != targetSoftware, bad url, bad status.
        let issues = validate_catalog_entry("a", "b", "http://x", "d", "bogus");
        assert!(issues.iter().any(|i| i.problem.contains("name must equal")));
        assert!(issues.iter().any(|i| i.problem.contains("repoUrl")));
        assert!(issues.iter().any(|i| i.problem.contains("status")));
    }

    #[test]
    fn missing_assets_detects_partial_release() {
        // Only the gnu binary (optional) and an aarch64-gnu binary (not a
        // release triple): every required triple is incomplete.
        let present = vec![
            "ntfy-v0.1.0-x86_64-unknown-linux-gnu".to_string(),
            "ntfy-v0.1.0-aarch64-unknown-linux-gnu".to_string(),
        ];
        let missing = missing_release_assets("ntfy", "0.1.0", &present);
        assert_eq!(missing, TARGET_TRIPLES);

        // A binary without its .sha256 is not complete.
        let no_sha = vec!["ntfy-v0.1.0-x86_64-unknown-linux-musl".to_string()];
        assert!(
            missing_release_assets("ntfy", "0.1.0", &no_sha)
                .contains(&"x86_64-unknown-linux-musl".to_string())
        );
    }

    #[test]
    fn a_release_without_gnu_is_complete() {
        let mut full = Vec::new();
        for t in TARGET_TRIPLES {
            full.push(format!("ntfy-v0.1.0-{t}"));
            full.push(format!("ntfy-v0.1.0-{t}.sha256"));
        }
        assert!(missing_release_assets("ntfy", "0.1.0", &full).is_empty());
    }
}
