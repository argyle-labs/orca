//! Typed install-status reporter.
//!
//! Relocated from `server::commands::install::install_status` (slice A2).
//! The legacy `serde_json::Value`-returning fn in server stays for now and
//! will be deleted in A4 when callers are rewired to the typed report.
//!
//! Helpers (`home_dir`, `install_bin_path`, `is_symlink`,
//! `check_mcp_registered`) are duplicated privately here per the
//! no-indirection rule — this crate must not call back into server.

use anyhow::{Context, Result};
use contract::config::{APP_MCP_SERVER, APP_NAME, APP_PKI_DIR, APP_STATE_DIR};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

// ── helpers (duplicated from server::commands::install) ──────────────────────

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("cannot determine home directory")
}

fn install_bin_path(home: &Path) -> PathBuf {
    home.join(format!(".local/bin/{APP_NAME}"))
}

fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn check_mcp_registered() -> bool {
    let out = std::process::Command::new("claude")
        .args(["mcp", "list"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(APP_MCP_SERVER),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
