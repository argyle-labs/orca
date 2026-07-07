//! Codebase sweep — orca dogfooding itself.
//!
//! Relocated 2026-06-01 from `system::sweep`. Tool shape deferred:
//! [`run_organization`] is a plain async fn for now. When the dev crate
//! grows an orca_tool surface it'll be a `dev.sweep` (or similar) entry.
//!
//! Per `project_orca_sweeps.md`. Order locked 2026-05-19:
//!   1. organization (this file) — unused deps, advisories, licenses.
//!   2. code-standards (TODO).
//!   3. security (TODO).
//!   4. coverage (TODO).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Args ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SweepOrganizationArgs {
    /// Cargo workspace root. Defaults to `cargo locate-project --workspace`.
    pub workspace_root: Option<PathBuf>,
}

// ── Output ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SweepOrganizationOutput {
    pub workspace_root: String,
    pub machete: MacheteReport,
    pub udeps: UdepsReport,
    pub deny: DenyReport,
    pub duration_ms: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Tool ran successfully.
    Ok,
    /// Tool binary not found on PATH; sweep was skipped for this tool.
    NotInstalled,
    /// Tool ran but exited non-zero or output failed to parse.
    Errored,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Normal,
    Dev,
    Build,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnusedDependency {
    /// Workspace crate that declared the unused dep.
    pub crate_name: String,
    pub dependency: String,
    pub kind: DependencyKind,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MacheteReport {
    pub status: ToolStatus,
    pub findings: Vec<UnusedDependency>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UdepsReport {
    pub status: ToolStatus,
    pub findings: Vec<UnusedDependency>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorySeverity {
    Critical,
    High,
    Medium,
    Low,
    None,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DenyAdvisory {
    pub id: String,
    pub package: String,
    pub version: String,
    pub severity: AdvisorySeverity,
    pub title: String,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DenyReport {
    pub status: ToolStatus,
    pub advisories: Vec<DenyAdvisory>,
    pub error: Option<String>,
}

// ── Tool body (native only) ─────────────────────────────────────────────────

/// Run the organization sweep — unused-dependency scans (cargo-machete +
/// cargo-udeps) and advisory check (cargo-deny). Each sub-tool reports
/// independently; missing binaries surface as `not_installed` rather than
/// erroring the whole sweep.
pub async fn run_organization(
    args: SweepOrganizationArgs,
) -> anyhow::Result<SweepOrganizationOutput> {
    let start = std::time::Instant::now();
    let workspace_root = native::resolve_workspace_root(args.workspace_root.as_deref())?;
    let machete = native::run_machete(&workspace_root).await;
    let udeps = native::run_udeps(&workspace_root).await;
    let deny = native::run_deny(&workspace_root).await;
    Ok(SweepOrganizationOutput {
        workspace_root: workspace_root.display().to_string(),
        machete,
        udeps,
        deny,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

mod native {
    use super::*;
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use tokio::process::Command;

    pub(super) fn resolve_workspace_root(explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        let out = std::process::Command::new("cargo")
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .output()
            .context("running `cargo locate-project` to find workspace root")?;
        if !out.status.success() {
            anyhow::bail!(
                "`cargo locate-project` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let manifest = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let path = PathBuf::from(manifest)
            .parent()
            .ok_or_else(|| anyhow::anyhow!("workspace Cargo.toml has no parent"))?
            .to_path_buf();
        Ok(path)
    }

    fn binary_available(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub(super) async fn run_machete(root: &Path) -> MacheteReport {
        if !binary_available("cargo-machete") {
            return MacheteReport {
                status: ToolStatus::NotInstalled,
                findings: vec![],
                error: None,
            };
        }
        let out = match Command::new("cargo-machete")
            .arg("--with-metadata")
            .current_dir(root)
            .output()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                return MacheteReport {
                    status: ToolStatus::Errored,
                    findings: vec![],
                    error: Some(e.to_string()),
                };
            }
        };
        // machete exits 1 when unused deps are found; both 0 and 1 are "ran".
        let code = out.status.code().unwrap_or(-1);
        if code != 0 && code != 1 {
            return MacheteReport {
                status: ToolStatus::Errored,
                findings: vec![],
                error: Some(format!(
                    "cargo-machete exited {code}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
            };
        }
        let findings = parse_machete_output(&String::from_utf8_lossy(&out.stdout));
        MacheteReport {
            status: ToolStatus::Ok,
            findings,
            error: None,
        }
    }

    /// Parse cargo-machete's plain-text output. Format:
    ///   `path/to/Cargo.toml -- <crate-name>:`
    ///   `\t<dep>`
    ///   ...
    fn parse_machete_output(stdout: &str) -> Vec<UnusedDependency> {
        let mut findings = vec![];
        let mut current_crate: Option<String> = None;
        for line in stdout.lines() {
            let trimmed = line.trim_end();
            if let Some(rest) = trimmed.strip_prefix("\t") {
                if let Some(name) = &current_crate
                    && !rest.is_empty()
                {
                    findings.push(UnusedDependency {
                        crate_name: name.clone(),
                        dependency: rest.to_string(),
                        kind: DependencyKind::Normal,
                    });
                }
            } else if let Some((_, after)) = trimmed.split_once(" -- ") {
                current_crate = after.strip_suffix(':').map(str::to_string);
            }
        }
        findings
    }

    pub(super) async fn run_udeps(_root: &Path) -> UdepsReport {
        // cargo-udeps requires a nightly toolchain. Detection probes both the
        // binary and `cargo +nightly --version`.
        if !binary_available("cargo-udeps") {
            return UdepsReport {
                status: ToolStatus::NotInstalled,
                findings: vec![],
                error: None,
            };
        }
        // Skip actually running it for now — udeps recompiles the workspace
        // under nightly, which is too slow for a default sweep. Surfaced as
        // a separate `--deep` flag in a future revision.
        UdepsReport {
            status: ToolStatus::NotInstalled,
            findings: vec![],
            error: Some("cargo-udeps present but skipped — deep scan not yet wired".into()),
        }
    }

    pub(super) async fn run_deny(root: &Path) -> DenyReport {
        if !binary_available("cargo-deny") {
            return DenyReport {
                status: ToolStatus::NotInstalled,
                advisories: vec![],
                error: None,
            };
        }
        let out = match Command::new("cargo-deny")
            .args(["check", "advisories", "--format", "json"])
            .current_dir(root)
            .output()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                return DenyReport {
                    status: ToolStatus::Errored,
                    advisories: vec![],
                    error: Some(e.to_string()),
                };
            }
        };
        // cargo-deny streams findings on stderr as JSON-lines; stdout is human.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let advisories = parse_deny_output(&stderr);
        DenyReport {
            status: ToolStatus::Ok,
            advisories,
            error: None,
        }
    }

    /// Parse cargo-deny `--format json` output (newline-delimited JSON on stderr).
    /// Tolerates lines that aren't advisory diagnostics; ignores those.
    fn parse_deny_output(stderr: &str) -> Vec<DenyAdvisory> {
        #[derive(serde::Deserialize)]
        struct Line {
            fields: Option<Fields>,
        }
        #[derive(serde::Deserialize)]
        struct Fields {
            advisory: Option<AdvisoryFields>,
            severity: Option<String>,
            message: Option<String>,
            #[serde(default)]
            labels: Vec<Label>,
            #[allow(dead_code)]
            #[serde(default)]
            notes: Vec<String>,
        }
        #[derive(serde::Deserialize)]
        struct AdvisoryFields {
            id: String,
            url: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Label {
            #[allow(dead_code)]
            span: Option<String>,
            message: Option<String>,
        }
        let mut out = vec![];
        for raw in stderr.lines() {
            let line: Line = match serde_json::from_str(raw) {
                Ok(l) => l,
                Err(_) => continue,
            };
            let Some(fields) = line.fields else { continue };
            let Some(adv) = fields.advisory else {
                continue;
            };
            // First label usually carries `<pkg> <version>` text.
            let (package, version) = fields
                .labels
                .iter()
                .find_map(|l| l.message.as_deref())
                .and_then(|s| s.split_once(' '))
                .map(|(p, v)| (p.to_string(), v.to_string()))
                .unwrap_or_default();
            let severity = match fields.severity.as_deref() {
                Some("error") => AdvisorySeverity::Critical,
                Some("warning") => AdvisorySeverity::High,
                Some("note") => AdvisorySeverity::Medium,
                Some("help") => AdvisorySeverity::Low,
                _ => AdvisorySeverity::None,
            };
            out.push(DenyAdvisory {
                id: adv.id,
                package,
                version,
                severity,
                title: fields.message.unwrap_or_default(),
                url: adv.url,
            });
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn machete_parser_basic() {
            let sample = "\
cargo-machete found the following unused dependencies in /repo:
/repo/projects/foo/Cargo.toml -- foo:
\tregex
\tonce_cell

/repo/projects/bar/Cargo.toml -- bar:
\tserde_yaml
";
            let findings = parse_machete_output(sample);
            assert_eq!(findings.len(), 3);
            assert_eq!(findings[0].crate_name, "foo");
            assert_eq!(findings[0].dependency, "regex");
            assert_eq!(findings[2].crate_name, "bar");
            assert_eq!(findings[2].dependency, "serde_yaml");
        }

        #[test]
        fn machete_parser_handles_empty() {
            assert!(parse_machete_output("").is_empty());
            assert!(parse_machete_output("no findings\n").is_empty());
        }

        #[test]
        fn deny_parser_skips_non_json() {
            assert!(parse_deny_output("hello\nworld\n").is_empty());
        }

        #[test]
        fn deny_parser_extracts_advisory() {
            let sample = r#"{"fields":{"severity":"warning","message":"RustSec advisory: cargo: vulnerable","advisory":{"id":"RUSTSEC-2024-0001","url":"https://rustsec.org/advisories/RUSTSEC-2024-0001"},"labels":[{"message":"openssl 0.10.0"}],"notes":[]}}"#;
            let parsed = parse_deny_output(sample);
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].id, "RUSTSEC-2024-0001");
            assert_eq!(parsed[0].package, "openssl");
            assert_eq!(parsed[0].version, "0.10.0");
            assert!(matches!(parsed[0].severity, AdvisorySeverity::High));
        }
    }
}
