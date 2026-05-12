// CLI install command passing through spec/config blobs; HashMap/Value are protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use anyhow::{Context, Result};
use colored::Colorize;
use orca_sdk::pki;
use orca_utils::config::{APP_MCP_SERVER, APP_NAME, APP_PKI_DIR, APP_STATE_DIR};
use std::path::{Path, PathBuf};

const CLAUDE_MD: &str = include_str!("../../../../CLAUDE.md");

// Known project slugs to wire memory symlinks for.
// Format: (macos_slug, linux_slug, vault_name)
const MEMORY_PROJECTS: &[(&str, &str, &str)] = &[
    ("-Users-scottkey", "-home-skey", "global"),
    ("-Users-scottkey-code-orca", "-home-skey-code-orca", "orca"),
    (
        "-Users-scottkey-code-meerkat",
        "-home-skey-code-meerkat",
        "meerkat",
    ),
    (
        "-Users-scottkey-code-bardbase",
        "-home-skey-code-bardbase",
        "bardbase",
    ),
    (
        "-Users-scottkey-dotfiles",
        "-home-skey-dotfiles",
        "dotfiles",
    ),
    (
        "-Users-scottkey-code-rebuy-bod",
        "-home-skey-code-rebuy-bod",
        "rebuy-bod-root",
    ),
    (
        "-Users-scottkey-code-rebuy-bod-bod",
        "-home-skey-code-rebuy-bod-bod",
        "rebuy-bod",
    ),
    (
        "-Users-scottkey-code-rebuy-bod-bod-api",
        "-home-skey-code-rebuy-bod-bod-api",
        "rebuy-bod-api",
    ),
    (
        "-Users-scottkey-code-rebuy-bod-bod-dev",
        "-home-skey-code-rebuy-bod-bod-dev",
        "rebuy-bod-dev",
    ),
    (
        "-Users-scottkey-code-rebuy-bod-tributary",
        "-home-skey-code-rebuy-bod-tributary",
        "rebuy-tributary",
    ),
    (
        "-Users-scottkey-code-rebuy",
        "-home-skey-code-rebuy",
        "rebuy",
    ),
    (
        "-Users-scottkey-code-rebuy-rebuy-cli",
        "-home-skey-code-rebuy-rebuy-cli",
        "rebuy-cli",
    ),
    (
        "-Users-scottkey-code-rebuy-admin-api",
        "-home-skey-code-rebuy-admin-api",
        "admin-api",
    ),
    (
        "-Users-scottkey-code-rebuy-admin-nextjs",
        "-home-skey-code-rebuy-admin-nextjs",
        "admin-nextjs",
    ),
    (
        "-Users-scottkey-code-rebuy-apiv2",
        "-home-skey-code-rebuy-apiv2",
        "apiv2",
    ),
    (
        "-Users-scottkey-code-rebuy-rebuy-db",
        "-home-skey-code-rebuy-rebuy-db",
        "rebuy-db",
    ),
    (
        "-Users-scottkey-code-rebuy-onsite-js",
        "-home-skey-code-rebuy-onsite-js",
        "onsite-js",
    ),
    (
        "-Users-scottkey-code-rebuy-installer",
        "-home-skey-code-rebuy-installer",
        "installer",
    ),
    (
        "-Users-scottkey-code-rebuy-rebuyengine.com",
        "-home-skey-code-rebuy-rebuyengine.com",
        "rebuyengine",
    ),
];

pub struct InstallReport {
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

impl InstallReport {
    fn new() -> Self {
        Self {
            done: vec![],
            skipped: vec![],
            errors: vec![],
        }
    }

    fn ok(&mut self, msg: impl Into<String>) {
        self.done.push(msg.into());
    }

    fn skip(&mut self, msg: impl Into<String>) {
        self.skipped.push(msg.into());
    }

    fn err(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    pub fn print(&self) {
        for s in &self.done {
            println!("  {} {s}", "✓".green());
        }
        for s in &self.skipped {
            println!("  {} {s}", "-".dimmed());
        }
        for s in &self.errors {
            println!("  {} {s}", "✗".red());
        }
    }

    pub fn success(&self) -> bool {
        self.errors.is_empty()
    }
}

// ── public entry points ───────────────────────────────────────────────────────

pub fn cmd_install_report() -> InstallReport {
    let home = match home_dir() {
        Ok(h) => h,
        Err(e) => {
            let mut r = InstallReport::new();
            r.err(format!("cannot determine home directory: {e}"));
            return r;
        }
    };
    let mut report = InstallReport::new();
    step_install_binary(&home, &mut report);
    step_vault_dirs(&home, &mut report);
    step_pki_init(&home, &mut report);
    step_claude_md(&home, &mut report);
    // Agents are served via the orca-local MCP server (list_agents / get_agent /
    // run_agent), not by a `~/.claude/agents` symlink. On hosts upgraded from
    // older orca versions, sweep the old symlink so Claude Code doesn't see
    // duplicate agent definitions from two sources.
    step_remove_legacy_agents_link(&home, &mut report);
    step_memory_symlinks(&home, &mut report);
    step_git_hooks(&mut report);
    step_mcp_registration(&mut report);
    report
}

pub fn cmd_uninstall_report() -> InstallReport {
    let home = match home_dir() {
        Ok(h) => h,
        Err(e) => {
            let mut r = InstallReport::new();
            r.err(format!("cannot determine home directory: {e}"));
            return r;
        }
    };
    let mut report = InstallReport::new();
    step_remove_mcp(&mut report);
    step_remove_claude_md(&home, &mut report);
    step_remove_legacy_agents_link(&home, &mut report);
    step_remove_binary(&home, &mut report);
    report
}

/// Machine-readable status for web UI polling.
pub fn install_status() -> serde_json::Value {
    let home = match home_dir() {
        Ok(h) => h,
        Err(e) => return serde_json::json!({ "error": e.to_string() }),
    };

    let binary_path = install_bin_path(&home);
    let claude_md_path = home.join(".claude/CLAUDE.md");
    let vault_dir = home.join(APP_STATE_DIR);
    let pki_dir = vault_dir.join(APP_PKI_DIR);
    let pki_ca = pki::ca_cert_path(&pki_dir);
    let pki_server = pki::server_cert_path(&pki_dir);
    let mcp_registered = check_mcp_registered();

    serde_json::json!({
        "binary": {
            "installed": binary_path.exists(),
            "path": binary_path.to_string_lossy(),
        },
        "claude_md": {
            "linked": is_symlink(&claude_md_path),
            "path": claude_md_path.to_string_lossy(),
        },
        "vault": {
            "exists": vault_dir.exists(),
            "path": vault_dir.to_string_lossy(),
        },
        "pki": {
            "initialized": pki_ca.exists() && pki_server.exists(),
            "path": pki_dir.to_string_lossy(),
        },
        "mcp": {
            "registered": mcp_registered,
        },
    })
}

// ── install steps ─────────────────────────────────────────────────────────────

fn step_install_binary(home: &Path, report: &mut InstallReport) {
    let dest = install_bin_path(home);
    let src = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            report.err(format!("binary: cannot resolve current exe: {e}"));
            return;
        }
    };

    if dest == src {
        report.skip(format!("binary: already at {}", dest.display()));
        return;
    }

    if let Err(e) = std::fs::create_dir_all(
        dest.parent()
            .expect("install_bin_path always has a parent dir"),
    ) {
        report.err(format!("binary: cannot create ~/.local/bin: {e}"));
        return;
    }

    // If a previous install left a symlink (e.g. pointing at target/release/orca),
    // remove it before copying. fs::copy would otherwise follow the link and
    // overwrite the build artifact in place, which is exactly the drift we're
    // trying to prevent — the installed binary must be a real file.
    if is_symlink(&dest) {
        if let Err(e) = std::fs::remove_file(&dest) {
            report.err(format!(
                "binary: cannot replace symlink at {}: {e}",
                dest.display()
            ));
            return;
        }
        report.ok(format!(
            "binary: removed stale symlink at {}",
            dest.display()
        ));
    }

    match std::fs::copy(&src, &dest) {
        Ok(_) => {
            set_executable(&dest);
            report.ok(format!("binary: installed to {}", dest.display()));
        }
        Err(e) => report.err(format!("binary: copy failed: {e}")),
    }
}

fn step_vault_dirs(home: &Path, report: &mut InstallReport) {
    let vault = home.join(APP_STATE_DIR);
    let dirs = [vault.join("memory"), vault.join("logs/sessions")];
    for dir in &dirs {
        match std::fs::create_dir_all(dir) {
            Ok(_) => report.ok(format!("vault dir: {}", dir.display())),
            Err(e) => report.err(format!("vault dir {}: {e}", dir.display())),
        }
    }
}

fn step_pki_init(home: &Path, report: &mut InstallReport) {
    let pki_dir = home.join(APP_STATE_DIR).join(APP_PKI_DIR);
    let already = pki::ca_cert_path(&pki_dir).exists() && pki::server_cert_path(&pki_dir).exists();
    match pki::init(&pki_dir) {
        Ok(_) if already => {
            report.skip(format!("pki: already initialized at {}", pki_dir.display()))
        }
        Ok(_) => report.ok(format!("pki: initialized at {}", pki_dir.display())),
        Err(e) => report.err(format!("pki: init failed: {e}")),
    }
}

fn step_claude_md(home: &Path, report: &mut InstallReport) {
    let claude_dir = home.join(".claude");
    let _ = std::fs::create_dir_all(&claude_dir);

    let vault_claude_md = home.join(APP_STATE_DIR).join("CLAUDE.md");
    let dot_claude_md = claude_dir.join("CLAUDE.md");

    // If orca repo is present, symlink through the vault. Otherwise write embedded content.
    let orca_repo_md = home.join("code/orca/CLAUDE.md");
    if orca_repo_md.exists() {
        // vault → repo
        force_symlink(
            &orca_repo_md,
            &vault_claude_md,
            report,
            "vault CLAUDE.md → repo",
        );
        // ~/.claude/CLAUDE.md → vault
        force_symlink(
            &vault_claude_md,
            &dot_claude_md,
            report,
            "~/.claude/CLAUDE.md → vault",
        );
    } else {
        // Write embedded content to vault (release install, no repo)
        match std::fs::write(&vault_claude_md, CLAUDE_MD) {
            Ok(_) => report.ok("vault CLAUDE.md written (embedded, no repo)".to_string()),
            Err(e) => {
                report.err(format!("vault CLAUDE.md write failed: {e}"));
                return;
            }
        }
        force_symlink(
            &vault_claude_md,
            &dot_claude_md,
            report,
            "~/.claude/CLAUDE.md → vault",
        );
    }
}

/// Sweep the legacy `~/.claude/agents` symlink that older orca installs
/// created. Agents are now served exclusively via the orca-local MCP server
/// (list_agents / get_agent / run_agent), so the on-disk link only causes
/// Claude Code to see duplicate definitions. Idempotent: silent no-op when
/// nothing's there. Real directories are left alone — if a user has hand-
/// curated content under `~/.claude/agents/`, we don't want to nuke it.
fn step_remove_legacy_agents_link(home: &Path, report: &mut InstallReport) {
    let agents_link = home.join(".claude/agents");
    if is_symlink(&agents_link) {
        match std::fs::remove_file(&agents_link) {
            Ok(_) => report.ok("~/.claude/agents: removed legacy symlink (agents now via MCP)"),
            Err(e) => report.err(format!("~/.claude/agents: remove failed: {e}")),
        }
    } else if agents_link.exists() {
        report.skip("~/.claude/agents: real directory present — not touching (move or delete manually if obsolete)");
    }
}

fn step_memory_symlinks(home: &Path, report: &mut InstallReport) {
    let claude_projects = home.join(".claude/projects");
    let orca_memory = home.join(APP_STATE_DIR).join("memory");
    let on_macos = cfg!(target_os = "macos");

    for (macos_slug, linux_slug, vault_name) in MEMORY_PROJECTS {
        let slug = if on_macos { macos_slug } else { linux_slug };
        let project_dir = claude_projects.join(slug);
        let memory_link = project_dir.join("memory");
        let vault_dir = orca_memory.join(vault_name);

        let _ = std::fs::create_dir_all(&project_dir);
        let _ = std::fs::create_dir_all(&vault_dir);

        if memory_link.exists() && !is_symlink(&memory_link) {
            // Real dir exists — back it up then remove
            let backup = project_dir.join("memory.bak");
            if let Err(e) = std::fs::rename(&memory_link, &backup) {
                report.err(format!(
                    "memory {vault_name}: cannot back up existing dir: {e}"
                ));
                continue;
            }
            report.ok(format!(
                "memory {vault_name}: backed up existing dir to memory.bak"
            ));
        }

        force_symlink(
            &vault_dir,
            &memory_link,
            report,
            &format!("memory/{vault_name}"),
        );
    }
}

fn step_git_hooks(report: &mut InstallReport) {
    // Find the repo root by walking up from the current exe's directory.
    // Falls back to CWD. Silently skips if we're not inside a git repo.
    let repo_root = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .and_then(|p| find_git_root(&p))
        .or_else(|| std::env::current_dir().ok().and_then(|p| find_git_root(&p)));

    let Some(root) = repo_root else {
        report.skip("git hooks: not inside an orca git repo — skipped".to_string());
        return;
    };

    let hooks_dir = root.join(".githooks");
    if !hooks_dir.exists() {
        report.skip("git hooks: .githooks not present — skipped".to_string());
        return;
    }

    let output = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap_or("."),
            "config",
            "core.hooksPath",
            ".githooks",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            report.ok("git hooks: core.hooksPath = .githooks".to_string());
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            report.err(format!("git hooks: failed to set core.hooksPath: {err}"));
        }
        Err(e) => {
            report.err(format!("git hooks: git not found: {e}"));
        }
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn step_mcp_registration(report: &mut InstallReport) {
    if check_mcp_registered() {
        report.skip(format!("MCP: {APP_MCP_SERVER} already registered"));
        return;
    }

    let orca_bin = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            report.err(format!("MCP: cannot resolve binary path: {e}"));
            return;
        }
    };

    let status = std::process::Command::new("claude")
        .args([
            "mcp",
            "add",
            APP_MCP_SERVER,
            "--",
            orca_bin.to_str().unwrap_or(APP_NAME),
            "mcp-serve",
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            report.ok(format!("MCP: {APP_MCP_SERVER} registered with Claude Code"))
        }
        Ok(s) => report.err(format!("MCP: claude mcp add exited {s}")),
        Err(e) => report.err(format!("MCP: claude not found or failed: {e}")),
    }
}

// ── uninstall steps ───────────────────────────────────────────────────────────

fn step_remove_mcp(report: &mut InstallReport) {
    if !check_mcp_registered() {
        report.skip(format!("MCP: {APP_MCP_SERVER} not registered"));
        return;
    }

    let status = std::process::Command::new("claude")
        .args(["mcp", "remove", APP_MCP_SERVER])
        .status();

    match status {
        Ok(s) if s.success() => report.ok(format!("MCP: {APP_MCP_SERVER} removed")),
        Ok(s) => report.err(format!("MCP: claude mcp remove exited {s}")),
        Err(e) => report.err(format!("MCP: claude not found or failed: {e}")),
    }
}

fn step_remove_claude_md(home: &Path, report: &mut InstallReport) {
    let vault_link = home.join(APP_STATE_DIR).join("CLAUDE.md");
    let dot_link = home.join(".claude/CLAUDE.md");

    for (path, label) in [
        (&dot_link, "~/.claude/CLAUDE.md"),
        (&vault_link, "vault CLAUDE.md"),
    ] {
        if is_symlink(path) {
            match std::fs::remove_file(path) {
                Ok(_) => report.ok(format!("{label}: removed symlink")),
                Err(e) => report.err(format!("{label}: remove failed: {e}")),
            }
        } else if path.exists() {
            report.skip(format!("{label}: not a symlink — leaving in place"));
        } else {
            report.skip(format!("{label}: not present"));
        }
    }
}

fn step_remove_binary(home: &Path, report: &mut InstallReport) {
    let bin = install_bin_path(home);
    if !bin.exists() {
        report.skip(format!("binary: not found at {}", bin.display()));
        return;
    }
    match std::fs::remove_file(&bin) {
        Ok(_) => report.ok(format!("binary: removed {}", bin.display())),
        Err(e) => report.err(format!("binary: remove failed: {e}")),
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

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

fn force_symlink(src: &Path, dest: &Path, report: &mut InstallReport, label: &str) {
    // Remove existing symlink so we can replace it
    if is_symlink(dest) {
        let _ = std::fs::remove_file(dest);
    }

    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(src, dest);
    #[cfg(not(unix))]
    let result: std::io::Result<()> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks not supported on this platform",
    ));

    match result {
        Ok(_) => report.ok(format!("{label}: {} → {}", dest.display(), src.display())),
        Err(e) => report.err(format!("{label}: symlink failed: {e}")),
    }
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

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}
