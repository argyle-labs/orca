//! Typed install-status reporter.
//!
//! Relocated from `server::commands::install::install_status` (slice A2).
//! The legacy `serde_json::Value`-returning fn in server stays for now and
//! will be deleted in A4 when callers are rewired to the typed report.
//!
//! The `$HOME` install-layout helpers (`home_dir`, `install_bin_path`,
//! `is_symlink`) are shared from `crate::install`; `check_mcp_registered`
//! stays local.

use anyhow::Result;
use contract::config::{APP_MCP_SERVER, APP_PKI_DIR, APP_STATE_DIR};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::install::{home_dir, install_bin_path, is_symlink};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct BinaryStatus {
    pub installed: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ClaudeMdStatus {
    pub linked: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct VaultStatus {
    pub exists: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PkiStatus {
    pub initialized: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct McpStatus {
    pub registered: bool,
}

/// Machine-readable install status — fully typed install state.
///
/// Reused directly by `system.detail` (consolidation pass dedups the
/// parallel `PathInstalled`/`PathLinked`/... structs that previously
/// lived in `system::system`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct InstallStatusReport {
    pub binary: BinaryStatus,
    pub claude_md: ClaudeMdStatus,
    pub vault: VaultStatus,
    pub agents: ClaudeMdStatus,
    pub pki: PkiStatus,
    pub mcp: McpStatus,
}

/// Build the typed install-status report.
pub fn install_status_report() -> Result<InstallStatusReport> {
    let home = home_dir()?;

    let binary_path = install_bin_path(&home);
    let claude_md_path = home.join(".claude/CLAUDE.md");
    let agents_path = home.join(".claude/agents");
    let vault_dir = home.join(APP_STATE_DIR);
    let pki_dir = vault_dir.join(APP_PKI_DIR);
    let pki_ca = utils::pki::ca_cert_path(&pki_dir);
    let pki_server = utils::pki::server_cert_path(&pki_dir);
    let mcp_registered = check_mcp_registered();

    Ok(InstallStatusReport {
        binary: BinaryStatus {
            installed: binary_path.exists(),
            path: binary_path,
        },
        claude_md: ClaudeMdStatus {
            linked: is_symlink(&claude_md_path),
            path: claude_md_path,
        },
        vault: VaultStatus {
            exists: vault_dir.exists(),
            path: vault_dir,
        },
        agents: ClaudeMdStatus {
            linked: is_symlink(&agents_path),
            path: agents_path,
        },
        pki: PkiStatus {
            initialized: pki_ca.exists() && pki_server.exists(),
            path: pki_dir,
        },
        mcp: McpStatus {
            registered: mcp_registered,
        },
    })
}

// ── helpers ──────────────────────────────────────────────────────────────────
// `home_dir` / `install_bin_path` / `is_symlink` are shared from
// `crate::install` (imported above) — same real-`$HOME` install layout.

/// Strip ANSI escape sequences so a colorized `claude mcp list` still parses.
/// The entry name sits at the start of the line, exactly where a color code
/// would be emitted, so a substring/prefix match on raw output can silently miss.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI … final-byte: skip until a letter terminates the sequence.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// True when `stdout` from `claude mcp list` contains an entry named EXACTLY
/// `name`.
///
/// Output is one entry per line, `"<name>: <endpoint> - <status>"`, and a name
/// may contain spaces (`"claude.ai Google Drive"`). The name is therefore
/// everything before the FIRST colon — endpoints contain colons too
/// (`http://…:12000`), so splitting on the first one is what isolates it.
///
/// This MUST be an exact match on the name, not a substring search of the whole
/// output: `APP_MCP_SERVER` ("orca") is a substring of `APP_MCP_SERVER_LEGACY`
/// ("orca-local"), so a `contains` check reports the client as correctly
/// registered on any host that still carries only the stale legacy entry — the
/// last place the retired name could lie about install state.
fn mcp_list_has_entry(stdout: &str, name: &str) -> bool {
    stdout.lines().any(|line| {
        let line = strip_ansi(line);
        // Lines without a colon are headers/blank ("Checking MCP server health…").
        matches!(line.split_once(':'), Some((entry, _)) if entry.trim() == name)
    })
}

fn check_mcp_registered() -> bool {
    let out = std::process::Command::new("claude")
        .args(["mcp", "list"])
        .output();
    match out {
        // A non-zero exit means the listing is unreliable; treat it as
        // not-registered rather than trusting partial stdout.
        Ok(o) if o.status.success() => {
            mcp_list_has_entry(&String::from_utf8_lossy(&o.stdout), APP_MCP_SERVER)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test-only: the retired name is what the exact-match fix exists to exclude.
    use contract::config::APP_MCP_SERVER_LEGACY;

    // Real `claude mcp list` output shape, including an entry name with spaces.
    const SAMPLE: &str = "Checking MCP server health…\n\n\
claude.ai Google Drive: https://drivemcp.googleapis.com/mcp/v1 - ✔ Connected\n\
orca: http://127.0.0.1:12000/api/mcp (HTTP) - ✔ Connected\n";

    #[test]
    fn mcp_entry_match_is_exact_not_substring() {
        // THE REGRESSION: "orca" is a substring of "orca-local", so a
        // whole-output `contains` check reported the client as registered on a
        // host carrying only the retired legacy entry.
        let legacy_only = "orca-local: http://127.0.0.1:12000/api/mcp (HTTP) - ✔ Connected\n";
        assert!(!mcp_list_has_entry(legacy_only, APP_MCP_SERVER));
        assert!(mcp_list_has_entry(legacy_only, APP_MCP_SERVER_LEGACY));

        // And the current name is still detected.
        assert!(mcp_list_has_entry(SAMPLE, APP_MCP_SERVER));
        assert!(!mcp_list_has_entry(SAMPLE, APP_MCP_SERVER_LEGACY));
    }

    #[test]
    fn mcp_entry_match_handles_real_output_shapes() {
        // Names may contain spaces; endpoints contain colons.
        assert!(mcp_list_has_entry(SAMPLE, "claude.ai Google Drive"));
        // Header and blank lines carry no colon and must not match.
        assert!(!mcp_list_has_entry(SAMPLE, "Checking MCP server health…"));
        assert!(!mcp_list_has_entry("", APP_MCP_SERVER));
        // A both-present transitional host counts as registered.
        let both = format!("orca-local: x - ✔\n{SAMPLE}");
        assert!(mcp_list_has_entry(&both, APP_MCP_SERVER));
    }

    #[test]
    fn mcp_entry_match_survives_ansi_color() {
        // A colorized name would defeat a naive prefix/substring match.
        let colored = "\u{1b}[1morca\u{1b}[0m: http://127.0.0.1:12000/api/mcp - ✔\n";
        assert!(mcp_list_has_entry(colored, APP_MCP_SERVER));
        let colored_legacy = "\u{1b}[1morca-local\u{1b}[0m: http://x - ✔\n";
        assert!(!mcp_list_has_entry(colored_legacy, APP_MCP_SERVER));
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = InstallStatusReport {
            binary: BinaryStatus {
                installed: true,
                path: PathBuf::from("/home/x/.local/bin/orca"),
            },
            claude_md: ClaudeMdStatus {
                linked: false,
                path: PathBuf::from("/home/x/.claude/CLAUDE.md"),
            },
            vault: VaultStatus {
                exists: true,
                path: PathBuf::from("/home/x/.orca"),
            },
            agents: ClaudeMdStatus {
                linked: false,
                path: PathBuf::from("/home/x/.claude/agents"),
            },
            pki: PkiStatus {
                initialized: false,
                path: PathBuf::from("/home/x/.orca/pki"),
            },
            mcp: McpStatus { registered: true },
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let back: InstallStatusReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report, back);
    }

    #[test]
    fn install_status_report_runs() {
        // HOME is set in CI/dev shells; if not, the fn surfaces an anyhow error.
        if let Ok(r) = install_status_report() {
            assert!(!r.binary.path.as_os_str().is_empty());
        }
    }
}
