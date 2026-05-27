//! Install / uninstall reporter — relocated from
//! `server::commands::install` (slice B1). Pure functions; no service
//! indirection. Helpers (`home_dir`, `install_bin_path`, `is_symlink`,
//! `check_mcp_registered`, `local_hostname`) are duplicated privately
//! per the no-indirection rule — this crate must not call back into
//! server.

// CLI install command passing through spec/config blobs; HashMap/Value are protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use anyhow::{Context, Result};
use orca_sdk::pki;
use orca_utils::config::{APP_MCP_SERVER, APP_NAME, APP_PKI_DIR, APP_STATE_DIR};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Global directive written to `~/.claude/CLAUDE.md` by `orca install`.
/// Tells Claude Code to invoke the `orca` agent first and delegate from
/// there. Distinct from the orca *project* CLAUDE.md (rust style rules)
/// which lives at `~/code/orca/CLAUDE.md` and is auto-loaded by Claude Code
/// only when working inside that repo.
const GLOBAL_CLAUDE_MD: &str = include_str!("templates/global_claude_md.md");

/// One project discovered on disk: a git repo somewhere under `~/code/` (or
/// `$HOME` itself for the global vault). Used to wire per-project Claude
/// Code memory symlinks and to materialize per-project agents.
///
/// Replaces the previous hardcoded `MEMORY_PROJECTS` list — projects are now
/// discovered dynamically so orca contains no references to specific user
/// repos.
struct DiscoveredProject {
    /// Absolute path to the project root (`$HOME` for the special `global`
    /// entry, otherwise a directory under `~/code/`).
    root: PathBuf,
    /// Stable label used as the per-project subdir under `~/.orca/memory/`.
    /// Derived from the path so it's reproducible across machines that share
    /// the same `~/code/` layout.
    vault_name: String,
    /// Claude Code's encoding of `root` — absolute path with separators
    /// replaced by `-`. Used as the subdir name under `~/.claude/projects/`.
    slug: String,
}

/// Discover projects under `$HOME` whose memory we should wire up.
///
/// Always includes a `global` entry for `$HOME` itself. Then walks
/// `$HOME/code/` and any subdir of `$HOME/code/<x>/` that looks like a
/// git repo (has `.git/`). Two-level depth is enough for the common
/// monorepo-of-repos layout (e.g. `~/code/rebuy/<repo>`) without
/// recursing into `node_modules` style trees.
fn discover_projects(home: &Path) -> Vec<DiscoveredProject> {
    let mut out = vec![DiscoveredProject {
        root: home.to_path_buf(),
        vault_name: "global".to_string(),
        slug: path_to_slug(home),
    }];

    let code = home.join("code");
    let Ok(level1) = std::fs::read_dir(&code) else {
        return out;
    };

    for e1 in level1.flatten() {
        let p1 = e1.path();
        if !p1.is_dir() {
            continue;
        }
        let name1 = p1
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if name1.is_empty() || name1.starts_with('.') {
            continue;
        }
        if p1.join(".git").exists() {
            out.push(DiscoveredProject {
                root: p1.clone(),
                vault_name: name1.clone(),
                slug: path_to_slug(&p1),
            });
        }
        // One level deeper for monorepo-of-repos layouts.
        if let Ok(level2) = std::fs::read_dir(&p1) {
            for e2 in level2.flatten() {
                let p2 = e2.path();
                if !p2.is_dir() {
                    continue;
                }
                let name2 = p2
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                if name2.is_empty() || name2.starts_with('.') {
                    continue;
                }
                if p2.join(".git").exists() {
                    out.push(DiscoveredProject {
                        root: p2.clone(),
                        vault_name: format!("{name1}-{name2}"),
                        slug: path_to_slug(&p2),
                    });
                }
            }
        }
    }
    out
}

/// Claude Code encodes project paths by replacing `/` with `-` (and stripping
/// the leading slash from the result, leaving a leading `-` from the empty
/// first segment). This must match Claude Code's encoding exactly.
fn path_to_slug(p: &Path) -> String {
    p.to_string_lossy().replace('/', "-")
}

#[derive(Serialize, Deserialize, JsonSchema)]
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
            println!("  ✓ {s}");
        }
        for s in &self.skipped {
            println!("  - {s}");
        }
        for s in &self.errors {
            println!("  ✗ {s}");
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
    step_cli_client_cert(&home, &mut report);
    step_claude_md(&home, &mut report);
    // Materialize embedded agents to `~/.claude/agents/<name>.md` so Claude
    // Code's native Agent picker auto-discovers them — no MCP roundtrip
    // required. Also writes per-project copies under each known
    // `<project>/.claude/agents/` so project-scoped agents override globals.
    step_claude_agents(&home, &mut report);
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
    step_remove_claude_agents(&home, &mut report);
    step_remove_binary(&home, &mut report);
    report
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

/// Issue this host's CLI client cert (CN=`cli.<host>`) signed by the local
/// core CA. Used by the orca CLI to authenticate to the REST API over mTLS.
/// Idempotent — skips if `client.cert.pem` already exists.
fn step_cli_client_cert(home: &Path, report: &mut InstallReport) {
    let pki_dir = home.join(APP_STATE_DIR).join(APP_PKI_DIR);
    if pki::cli_client_cert_path(&pki_dir).exists() && pki::cli_client_key_path(&pki_dir).exists() {
        report.skip(format!(
            "pki/cli: client cert already present at {}",
            pki::cli_client_cert_path(&pki_dir).display()
        ));
        return;
    }
    // Hostname for CN — install runs in standalone CLI flows where the
    // server-side host_identity OnceLock may not be populated. CN is
    // cosmetic for routing; the trust gate is the signature, not the name.
    let host_cn = local_hostname();
    match pki::issue_cli_client_cert(&pki_dir, &host_cn) {
        Ok(_) => report.ok(format!(
            "pki/cli: issued client cert cli.{host_cn} at {}",
            pki::cli_client_cert_path(&pki_dir).display()
        )),
        Err(e) => report.err(format!("pki/cli: issue failed: {e}")),
    }
}

fn step_claude_md(home: &Path, report: &mut InstallReport) {
    let claude_dir = home.join(".claude");
    if let Err(e) = std::fs::create_dir_all(&claude_dir) {
        report.err(format!("~/.claude: mkdir failed: {e}"));
        return;
    }

    // Clear any legacy symlink at ~/.orca/CLAUDE.md left by older installs
    // — the vault no longer hosts CLAUDE.md; the global directive lives
    // directly at ~/.claude/CLAUDE.md and the per-project rules stay in
    // each repo's CLAUDE.md.
    let legacy_vault_md = home.join(APP_STATE_DIR).join("CLAUDE.md");
    if let Ok(meta) = std::fs::symlink_metadata(&legacy_vault_md)
        && meta.file_type().is_symlink()
    {
        _ = std::fs::remove_file(&legacy_vault_md);
    }

    let dot_claude_md = claude_dir.join("CLAUDE.md");
    // If a previous install symlinked ~/.claude/CLAUDE.md elsewhere, drop
    // the link so std::fs::write doesn't follow it back into the repo.
    if let Ok(meta) = std::fs::symlink_metadata(&dot_claude_md)
        && meta.file_type().is_symlink()
    {
        _ = std::fs::remove_file(&dot_claude_md);
    }

    match std::fs::write(&dot_claude_md, GLOBAL_CLAUDE_MD) {
        Ok(_) => report.ok("~/.claude/CLAUDE.md written (orca-first directive)".to_string()),
        Err(e) => report.err(format!("~/.claude/CLAUDE.md write failed: {e}")),
    }
}

/// External repos that own their own agent rosters. Each entry is a path
/// (relative to `$HOME/code/`) to a directory containing `<name>.md` files.
/// Discovered at install time and merged with orca's embedded agents.
///
/// To register a new external source, add the path here. Future: read this
/// list from `orca.db` so plugins can self-register without recompiling
/// orca.
const EXTERNAL_AGENT_SOURCES: &[&str] = &[
    "meerkat/agents",
    "rebuy/rebuy-orca-plugin/agents",
    "leetcode/agents",
];

/// One agent prompt resolved at install time: either embedded in the orca
/// binary or read from an external source repo. `body` is the full file
/// contents (frontmatter + prompt), ready to write verbatim.
struct AgentEntry {
    name: String,
    body: String,
    origin: String,
}

/// Walk embedded + external sources and return the full agent roster.
/// External sources win over embedded on name collision — that's how a
/// plugin can override an orca-shipped default for projects that have
/// the plugin installed (it just won't be a collision because we're
/// dropping the agents that belong to external repos).
fn collect_agent_entries(home: &Path) -> Vec<AgentEntry> {
    let mut by_name: std::collections::BTreeMap<String, AgentEntry> =
        std::collections::BTreeMap::new();

    for name in agents::embedded::embedded_agent_names() {
        if let Some(raw) = agents::embedded::embedded_agent(name) {
            by_name.insert(
                name.to_string(),
                AgentEntry {
                    name: name.to_string(),
                    body: raw.to_string(),
                    origin: "embedded".to_string(),
                },
            );
        }
    }

    for rel in EXTERNAL_AGENT_SOURCES {
        let dir = home.join("code").join(rel);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".md"))
            else {
                continue;
            };
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            by_name.insert(
                name.to_string(),
                AgentEntry {
                    name: name.to_string(),
                    body,
                    origin: format!("~/code/{rel}"),
                },
            );
        }
    }

    by_name.into_values().collect()
}

/// Materialize every agent (embedded + external sources) to
/// `~/.claude/agents/<name>.md` so Claude Code's native Agent picker
/// discovers them automatically. Also writes per-project copies under
/// each known `~/code/<project>/.claude/agents/`.
///
/// Overwrite policy: unconditional. Re-run on every `orca install` /
/// `orca update` / daemon start. Users who want to edit an agent's prompt
/// should fork it to a different name (e.g. `wolf-custom.md`).
fn step_claude_agents(home: &Path, report: &mut InstallReport) {
    let entries = collect_agent_entries(home);

    materialize_agents_to(
        &entries,
        &home.join(".claude/agents"),
        "~/.claude/agents",
        report,
    );

    for project in discover_projects(home) {
        if project.vault_name == "global" {
            continue;
        }
        let target = project.root.join(".claude/agents");
        materialize_agents_to(
            &entries,
            &target,
            &format!("{}/.claude/agents", project.root.display()),
            report,
        );
    }

    let from_external = entries.iter().filter(|e| e.origin != "embedded").count();
    if from_external > 0 {
        report.ok(format!(
            "agents: {from_external} from external sources, {} embedded",
            entries.len() - from_external
        ));
    }
}

fn materialize_agents_to(
    entries: &[AgentEntry],
    target_dir: &Path,
    label: &str,
    report: &mut InstallReport,
) {
    if let Err(e) = std::fs::create_dir_all(target_dir) {
        report.err(format!("{label}: mkdir failed: {e}"));
        return;
    }
    let mut written = 0usize;
    let mut errored = 0usize;
    for entry in entries {
        let path = target_dir.join(format!("{}.md", entry.name));
        match std::fs::write(&path, &entry.body) {
            Ok(_) => written += 1,
            Err(e) => {
                errored += 1;
                report.err(format!("{label}/{}.md: write failed: {e}", entry.name));
            }
        }
    }
    if errored == 0 {
        report.ok(format!("{label}: materialized {written} agents"));
    }
}

/// Remove every agent file orca materialized at install time. Only deletes
/// canonical names that match an entry we wrote — user-authored agents in
/// the same directory are left alone.
fn step_remove_claude_agents(home: &Path, report: &mut InstallReport) {
    let entries = collect_agent_entries(home);
    let mut targets: Vec<std::path::PathBuf> = vec![home.join(".claude/agents")];
    for project in discover_projects(home) {
        if project.vault_name == "global" {
            continue;
        }
        let dir = project.root.join(".claude/agents");
        if dir.exists() {
            targets.push(dir);
        }
    }
    for dir in &targets {
        if !dir.exists() {
            continue;
        }
        let mut removed = 0usize;
        for entry in &entries {
            let path = dir.join(format!("{}.md", entry.name));
            if path.exists() && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        report.ok(format!("{}: removed {removed} agents", dir.display()));
    }
}

fn step_memory_symlinks(home: &Path, report: &mut InstallReport) {
    let claude_projects = home.join(".claude/projects");
    let orca_memory = home.join(APP_STATE_DIR).join("memory");

    for project in discover_projects(home) {
        let DiscoveredProject {
            slug, vault_name, ..
        } = &project;
        let project_dir = claude_projects.join(slug);
        let memory_link = project_dir.join("memory");
        let vault_dir = orca_memory.join(vault_name);

        _ = std::fs::create_dir_all(&project_dir);
        _ = std::fs::create_dir_all(&vault_dir);

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
    let dot_path = home.join(".claude/CLAUDE.md");

    // Vault path: only remove if it's a legacy symlink (we no longer write
    // a regular file here, so any plain file present is user-owned).
    if is_symlink(&vault_link) {
        match std::fs::remove_file(&vault_link) {
            Ok(_) => report.ok("vault CLAUDE.md: removed legacy symlink".to_string()),
            Err(e) => report.err(format!("vault CLAUDE.md: remove failed: {e}")),
        }
    } else if vault_link.exists() {
        report.skip("vault CLAUDE.md: not a symlink — leaving in place".to_string());
    } else {
        report.skip("vault CLAUDE.md: not present".to_string());
    }

    // ~/.claude/CLAUDE.md: orca-managed file (or legacy symlink). Remove
    // only if it matches our directive content or is a symlink we placed.
    match std::fs::symlink_metadata(&dot_path) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::remove_file(&dot_path) {
            Ok(_) => report.ok("~/.claude/CLAUDE.md: removed legacy symlink".to_string()),
            Err(e) => report.err(format!("~/.claude/CLAUDE.md: remove failed: {e}")),
        },
        Ok(_) => match std::fs::read_to_string(&dot_path) {
            Ok(s) if s == GLOBAL_CLAUDE_MD => match std::fs::remove_file(&dot_path) {
                Ok(_) => report.ok("~/.claude/CLAUDE.md: removed".to_string()),
                Err(e) => report.err(format!("~/.claude/CLAUDE.md: remove failed: {e}")),
            },
            _ => report.skip("~/.claude/CLAUDE.md: user-modified — leaving in place".to_string()),
        },
        Err(_) => report.skip("~/.claude/CLAUDE.md: not present".to_string()),
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

fn local_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn force_symlink(src: &Path, dest: &Path, report: &mut InstallReport, label: &str) {
    // Remove existing symlink so we can replace it
    if is_symlink(dest) {
        _ = std::fs::remove_file(dest);
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
        _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}
