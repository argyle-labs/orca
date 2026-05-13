//! `orca system <verb>` — host-level lifecycle helpers that scripts shell out to.
//!
//! Each verb is a Rust function so Makefile, install.sh, deploy-host.sh, and
//! the orca binary itself agree on patterns and behavior. Adding a new
//! pattern (e.g. another process name to clean up on binary swap) is a single
//! edit here, not a sweep across shell files.

use anyhow::Result;
use clap::Subcommand;
use std::process::Command;

#[derive(Subcommand, Debug)]
pub enum SystemAction {
    /// Kill stale orca runtime processes (mcp-serve, daemon start) so a
    /// binary swap is picked up by their clients on next call. Safe to run
    /// before any deploy; no-op when nothing matches.
    KillStale,
}

pub fn cmd_system(action: SystemAction) -> Result<()> {
    match action {
        SystemAction::KillStale => kill_stale_runtime(),
    }
}

/// Patterns kept here as the single source — scripts must NOT inline pkill.
const STALE_PATTERNS: &[&str] = &["orca mcp-serve", "orca daemon start"];

pub fn kill_stale_runtime() -> Result<()> {
    for pat in STALE_PATTERNS {
        // pkill -f matches against the full argv string. Exit 1 = no match,
        // which is fine — we ignore non-zero. Other exits (2 = syntax, 3 =
        // fatal, 64+ = signal failure) we surface as warnings, not fatals,
        // because deploy must proceed.
        let status = Command::new("pkill").arg("-f").arg(pat).status();
        match status {
            Ok(s) if s.success() => println!("→ killed processes matching '{pat}'"),
            Ok(_) => {} // no match — silent
            Err(e) => eprintln!("warn: pkill '{pat}' failed: {e}"),
        }
    }
    Ok(())
}
