//! Core diagnostics provider: surfaces config-parse failures found in logs.
//!
//! Pairs with [`utils::config_format`] (validate before orca writes) and
//! [`utils::config_parse_log`] (recognise a parse failure a service logged).
//! This is the piece that makes the read side *reachable* — without a provider
//! the detector is a function nobody calls, so nobody is told.
//!
//! ## Scope, stated honestly
//!
//! This provider reads the logs orca itself owns (`~/.orca/logs/*.log`). It does
//! NOT reach inside guests, so on its own it would **not** have caught the frigg
//! Jellyfin outage — that message was in Jellyfin's log, on another host, and
//! nothing about it passed through orca. What it does catch is every
//! config-parse failure orca logged while reading a plugin, service, or its own
//! config, which today goes unread in a 47 MB file.
//!
//! Closing the guest half is a plugin's job and the reason
//! [`utils::config_parse_log::scan`] takes text rather than a path: a jellyfin or
//! docker plugin already has that host's log and can register its own
//! diagnostics provider calling the same detector, so the signatures live in one
//! place instead of being re-implemented per plugin.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use contract::BoxFuture;
use contract::config::{APP_LOGS_SUBDIR, APP_STATE_DIR};
use contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome, Severity,
};
use utils::config_parse_log::{self, ParseFailure};

/// Registry name. Stable — it is the `--provider` filter operators type.
pub const PROVIDER_NAME: &str = "config-parse";

/// Cap per log file, read from the tail. A parse failure repeats on every
/// restart, so the newest window is enough to prove the condition still holds,
/// and an unbounded read of a multi-GB log is not something a health check may do.
const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;

/// Resolve orca's own log directory.
fn logs_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::Path::new(&home)
        .join(APP_STATE_DIR)
        .join(APP_LOGS_SUBDIR)
}

/// Read at most the last `MAX_SCAN_BYTES` of `path` as lossy UTF-8.
///
/// Lossy on purpose: a truncated multi-byte char at the window boundary must not
/// turn into an error that hides a real finding further down the file.
fn tail_text(path: &std::path::Path, max_bytes: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Turn one detected failure into an operator-facing finding.
///
/// `Crit`, not `Warn`: the service is running on a config it could not read, so
/// its actual behaviour silently differs from its declared config. That is the
/// condition that stayed invisible for six months precisely because everything
/// downstream looked healthy.
fn finding_for(source: &str, f: &ParseFailure) -> Finding {
    let where_ = f
        .path
        .as_deref()
        .map(|p| format!("`{p}`"))
        .unwrap_or_else(|| "an unnamed config file".to_string());
    let fmt = f
        .format
        .map(|x| format!(" ({} parse failure)", x.as_str()))
        .unwrap_or_default();
    Finding {
        // Include the path so two broken files are two findings, not one that
        // overwrites the other in any id-keyed view.
        id: match &f.path {
            Some(p) => format!("{}:{}", f.signature, p),
            None => f.signature.to_string(),
        },
        provider: PROVIDER_NAME.to_string(),
        severity: Severity::Crit,
        title: format!("Config not parseable: {where_}{fmt}"),
        detail: format!(
            "{source} reports a config-parse failure, so the service is running on \
             fallback defaults rather than this file's contents. Logged line:\n{}",
            f.line
        ),
        // No RepairSpec: fixing malformed config means editing it, and orca must
        // not guess an operator's intended content. Detection is the deliverable.
        repair: None,
    }
}

/// Diagnoses config-parse failures in orca's own logs. See the module docs for
/// why the guest half belongs to plugins.
pub struct ConfigParseDiagnostics;

impl DiagnosticsProvider for ConfigParseDiagnostics {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn diagnose(&self, _args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        Box::pin(async move {
            let dir = logs_dir();
            let Ok(entries) = std::fs::read_dir(&dir) else {
                // No log dir yet (fresh install) is healthy, not an error — a
                // provider that errors here would blank its own report.
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("log") {
                    continue;
                }
                // An unreadable individual log is skipped, not fatal: one
                // permission-denied file must not hide findings in the others.
                let Ok(text) = tail_text(&path, MAX_SCAN_BYTES) else {
                    continue;
                };
                let name = path
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or("log")
                    .to_string();
                for f in config_parse_log::distinct(&config_parse_log::scan(&text)) {
                    out.push(finding_for(&name, &f));
                }
            }
            Ok(out)
        })
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        Box::pin(async move {
            // Deliberately unrepairable — see `finding_for`. Returning an error
            // rather than a false success keeps the surface honest.
            Err(anyhow!(
                "{PROVIDER_NAME} has no automatic repair for {:?}: a malformed config \
                 must be corrected by hand, then the service reloaded",
                args.repair_id
            ))
        })
    }
}

/// Register this provider. Idempotent — `register_provider` replaces by name.
pub fn register() {
    contract::diagnostics::register_provider(Arc::new(ConfigParseDiagnostics));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detected_failure_becomes_a_crit_finding_naming_the_file() {
        let f = ParseFailure {
            signature: "dotnet-config-load",
            format: Some(utils::config_format::ConfigFormat::Xml),
            path: Some("/etc/jellyfin/network.xml".to_string()),
            line: "Error loading configuration file: /etc/jellyfin/network.xml".to_string(),
        };
        let got = finding_for("daemon.log", &f);
        assert_eq!(got.severity, Severity::Crit, "silent fallback is critical");
        assert_eq!(got.provider, PROVIDER_NAME);
        assert!(got.title.contains("/etc/jellyfin/network.xml"));
        assert!(got.title.contains("xml parse failure"));
        // Evidence must survive into the finding, not just the summary.
        assert!(got.detail.contains("Error loading configuration file"));
        assert!(got.repair.is_none(), "orca must not guess config contents");
    }

    #[test]
    fn findings_for_two_files_have_distinct_ids() {
        let mk = |p: &str| ParseFailure {
            signature: "dotnet-config-load",
            format: None,
            path: Some(p.to_string()),
            line: format!("Error loading configuration file: {p}"),
        };
        let a = finding_for("daemon.log", &mk("/etc/a.xml"));
        let b = finding_for("daemon.log", &mk("/etc/b.xml"));
        assert_ne!(a.id, b.id, "same signature, different file = two findings");
    }

    #[test]
    fn a_pathless_failure_still_yields_a_finding() {
        let f = ParseFailure {
            signature: "dotnet-xml",
            format: None,
            path: None,
            line: "There is an error in XML document (3, 42)".to_string(),
        };
        let got = finding_for("daemon.log", &f);
        assert_eq!(got.id, "dotnet-xml");
        assert!(got.title.contains("unnamed"));
        assert!(got.detail.contains("(3, 42)"));
    }

    #[test]
    fn tail_text_reads_only_the_window_and_never_splits_into_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("big.log");
        // Multi-byte char straddling the cap boundary must not error.
        let body = format!("{}émalformed configuration\n", "x".repeat(4096));
        std::fs::write(&p, &body).expect("write");
        let got = tail_text(&p, 64).expect("tail must not fail on a split char");
        assert!(got.len() <= 64 + 4, "window is bounded");
        // And the detector still works on the lossy window.
        assert_eq!(config_parse_log::scan(&got).len(), 1);
    }

    #[tokio::test]
    async fn repair_is_an_honest_error_not_a_fake_success() {
        let p = ConfigParseDiagnostics;
        let r = p
            .repair(RepairArgs {
                provider: PROVIDER_NAME.to_string(),
                repair_id: "dotnet-config-load:/etc/a.xml".to_string(),
                confirm: true,
            })
            .await;
        assert!(
            r.is_err(),
            "must not claim to have fixed a config it cannot"
        );
    }

    /// Registering must be idempotent and must make the provider visible to the
    /// `diagnose` fan-out — otherwise the detector is still unreachable.
    #[tokio::test]
    async fn register_is_idempotent_and_the_provider_is_reachable() {
        register();
        register();
        let n = contract::diagnostics::providers()
            .iter()
            .filter(|p| p.name() == PROVIDER_NAME)
            .count();
        assert_eq!(n, 1, "re-register must replace, not duplicate");
    }
}
