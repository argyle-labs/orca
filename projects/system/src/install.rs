//! Install / uninstall reporter — relocated from
//! `server::commands::install` (slice B1). Pure functions; no service
//! indirection. Helpers (`home_dir`, `install_bin_path`, `is_symlink`,
//! `check_mcp_registered`, `local_hostname`) are duplicated privately
//! per the no-indirection rule — this crate must not call back into
//! server.

// CLI install command passing through spec/config blobs; HashMap/Value are protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use anyhow::{Context, Result};
use contract::config::{APP_MCP_SERVER, APP_NAME, APP_PKI_DIR, APP_STATE_DIR};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Global directive written to `~/.claude/CLAUDE.md` by `orca install`.
/// Tells Claude Code to invoke the `orca` agent first and delegate from
/// there. Distinct from the orca *project* CLAUDE.md (rust style rules)
/// which lives at `~/code/argyle-labs/orca/CLAUDE.md` and is auto-loaded by Claude Code
/// only when working inside that repo.
const GLOBAL_CLAUDE_MD: &str = include_str!("templates/global_claude_md.md");

/// Global git `commit-msg` guard materialized to `~/.config/git/hooks/commit-msg`
/// and activated via `git config --global core.hooksPath`. Rejects an AI
/// attribution trailer in any commit on this machine, in every repo. Chains to
/// repo-local hooks so it shadows nothing.
const COMMIT_MSG_GUARD: &str = include_str!("templates/commit_msg_block_coauthor.sh");

/// Global git `pre-push` gate materialized into the same `core.hooksPath` dir as
/// the commit-msg guard. A global `core.hooksPath` shadows every repo's own
/// `.git/hooks/pre-push`, silently disabling dev/CI parity; this restores it by
/// running `cargo fmt --check` + clippy + test for argyle-labs cargo repos
/// before a push. No-op elsewhere; chains to a repo-local pre-push.
const PRE_PUSH_GATE: &str = include_str!("templates/pre_push_ci_gate.sh");

/// Claude PreToolUse attribution guard, materialized to
/// `~/.claude/hooks/block-coauthor.sh` (the path the settings.json PreToolUse
/// entry references). Blocks assistant self-attribution in ANY form — trailers,
/// "Generated with"-style credit lines, the robot signature glyph, and
/// attribution links — in tool payloads for commits, PR bodies (`gh pr
/// create/edit`), and files alike. Git hooks cannot see `gh pr create`, so this
/// Claude-layer guard is the primary line of defense; the commit-msg guard is
/// the git-layer backstop. Owned by orca so it survives install/update and
/// reaches every machine, rather than living as a clobberable hand-edit.
const CLAUDE_ATTRIBUTION_GUARD: &str = include_str!("templates/claude_attribution_guard.sh");

/// One project discovered on disk: a git repo somewhere under `~/code/` (or
/// `$HOME` itself for the global vault). Used to wire per-project Claude
/// Code memory symlinks and to materialize per-project agents.
///
/// Replaces the previous hardcoded `MEMORY_PROJECTS` list — projects are now
/// discovered dynamically so orca contains no references to specific user
/// repos.
struct DiscoveredProject {
    /// Absolute path to the project root (`$HOME` for the special `global`
    /// entry, otherwise a directory under `~/code/`). Only read by tests now
    /// that agent materialization (its former consumer) lives in the agents
    /// plugin; retained for the discovery contract and test assertions.
    #[allow(dead_code)]
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
/// monorepo-of-repos layout (e.g. `~/code/<org>/<repo>`) without
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
    // After the new binary is in place, terminate any `mcp-serve` stdio servers
    // still running a pre-deploy image so their clients reconnect onto the new
    // binary. Same-binary instances and the daemon are left untouched.
    step_reap_stale_mcp_serve(&home, &mut report);
    step_vault_dirs(&home, &mut report);
    step_pki_init(&home, &mut report);
    step_cli_client_cert(&home, &mut report);
    step_claude_md(&home, &mut report);
    // NOTE: Claude-artifact materialization (agents, skills, slash-commands,
    // provider hooks) is intentionally NOT done here. That responsibility lives
    // entirely in the external `argyle-labs/agents` plugin, invoked explicitly
    // via `orca agents install` — core never writes the roster to
    // `~/.claude/{agents,skills,commands}` and never touches the hooks subtree.
    // Materialize the Claude PreToolUse attribution guard to
    // ~/.claude/hooks/block-coauthor.sh so it is orca-owned (survives
    // install/update, reaches every machine) rather than a hand-edit.
    step_claude_attribution_guard(&home, &mut report);
    step_memory_symlinks(&home, &mut report);
    step_git_hooks(&mut report);
    step_global_commit_guard(&home, &mut report);
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
    // Agent artifact removal is owned by the `argyle-labs/agents` plugin
    // (`orca agents ...`), not core — see the note in `cmd_install_report`.
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

/// Reap `orca mcp-serve` instances left over from a previous binary image.
///
/// The deploy boundary is the installed binary's mtime: any `mcp-serve` that
/// started before the binary on disk was last written is, by definition,
/// running the old code. Those are signalled; instances started at/after the
/// boundary (already on the new binary, or a client that reconnected during
/// the deploy) are spared. A no-op when the binary is absent or nothing is
/// stale.
fn step_reap_stale_mcp_serve(home: &Path, report: &mut InstallReport) {
    let dest = install_bin_path(home);
    let boundary = match std::fs::metadata(&dest).and_then(|m| m.modified()) {
        Ok(mtime) => match mtime.duration_since(std::time::UNIX_EPOCH) {
            Ok(since) => since.as_secs(),
            Err(_) => {
                report.skip("reap: installed binary mtime precedes the epoch; skipped");
                return;
            }
        },
        Err(_) => {
            report.skip(format!(
                "reap: no installed binary at {} yet",
                dest.display()
            ));
            return;
        }
    };

    let outcome = crate::sysadmin::reap_stale_mcp_serve(boundary);
    if outcome.killed.is_empty() {
        report.skip(format!(
            "reap: no stale mcp-serve ({} current spared)",
            outcome.spared
        ));
    } else {
        let pids = outcome
            .killed
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        report.ok(format!(
            "reap: signalled {} stale mcp-serve [{pids}] ({} current spared)",
            outcome.killed.len(),
            outcome.spared
        ));
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
    let already = utils::pki::ca_cert_path(&pki_dir).exists()
        && utils::pki::server_cert_path(&pki_dir).exists();
    match utils::pki::init(&pki_dir) {
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
    if utils::pki::cli_client_cert_path(&pki_dir).exists()
        && utils::pki::cli_client_key_path(&pki_dir).exists()
    {
        report.skip(format!(
            "pki/cli: client cert already present at {}",
            utils::pki::cli_client_cert_path(&pki_dir).display()
        ));
        return;
    }
    // Hostname for CN — install runs in standalone CLI flows where the
    // server-side host_identity OnceLock may not be populated. CN is
    // cosmetic for routing; the trust gate is the signature, not the name.
    let host_cn = local_hostname();
    match utils::pki::issue_cli_client_cert(&pki_dir, &host_cn) {
        Ok(_) => report.ok(format!(
            "pki/cli: issued client cert cli.{host_cn} at {}",
            utils::pki::cli_client_cert_path(&pki_dir).display()
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

    // Compose the base directive with any CLAUDE.md fragments contributed by
    // registered providers — each under its own heading. Empty today; the seam
    // lets a plugin extend the global directive without editing this template.
    let mut contents = GLOBAL_CLAUDE_MD.to_string();
    let fragments = agents::compose_prompt_fragments();
    for fragment in &fragments {
        contents.push_str(&format!(
            "\n\n## {}\n\n{}\n",
            fragment.heading.trim(),
            fragment.body.trim()
        ));
    }

    let fragment_note = if fragments.is_empty() {
        String::new()
    } else {
        format!(" + {} composed fragment(s)", fragments.len())
    };
    match std::fs::write(&dot_claude_md, &contents) {
        Ok(_) => report.ok(format!(
            "~/.claude/CLAUDE.md written (orca-first directive{fragment_note})"
        )),
        Err(e) => report.err(format!("~/.claude/CLAUDE.md write failed: {e}")),
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

/// Materialize the Claude PreToolUse attribution guard to
/// `~/.claude/hooks/block-coauthor.sh` — the path the settings.json PreToolUse
/// entry already references. Owned by orca so the script itself is
/// version-controlled and fleet-wide, not a clobberable hand-edit. Idempotent;
/// won't overwrite a foreign script the operator maintains (detected by the
/// orca marker line). The binding in settings.json is left to the operator /
/// existing config — orca only owns the guard body here.
fn step_claude_attribution_guard(home: &Path, report: &mut InstallReport) {
    let hooks_dir = home.join(".claude/hooks");
    if let Err(e) = std::fs::create_dir_all(&hooks_dir) {
        report.err(format!(
            "attribution guard: mkdir {} failed: {e}",
            hooks_dir.display()
        ));
        return;
    }
    let hook_path = hooks_dir.join("block-coauthor.sh");
    // Don't clobber a foreign script the operator maintains under this name.
    if hook_path.exists()
        && let Ok(current) = std::fs::read_to_string(&hook_path)
        && !current.contains("orca-managed: Claude attribution guard")
    {
        report.skip(format!(
            "attribution guard: {} already exists (not orca's) — left untouched",
            hook_path.display()
        ));
        return;
    }
    if let Err(e) = std::fs::write(&hook_path, CLAUDE_ATTRIBUTION_GUARD) {
        report.err(format!(
            "attribution guard: write {} failed: {e}",
            hook_path.display()
        ));
        return;
    }
    set_executable(&hook_path);
    report.ok(format!(
        "attribution guard: installed at {}",
        hook_path.display()
    ));
}

/// Materialize the global `commit-msg` guard and point git's global
/// `core.hooksPath` at it, so an AI attribution trailer is rejected in every
/// repo on this machine — not just the orca repo. Idempotent; honors an
/// existing global `core.hooksPath` by writing into it instead of overriding.
fn step_global_commit_guard(home: &Path, report: &mut InstallReport) {
    let default_dir = home.join(".config/git/hooks");

    // Honor an operator-set global core.hooksPath; otherwise use the default.
    let existing = std::process::Command::new("git")
        .args(["config", "--global", "core.hooksPath"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let (hooks_dir, set_path) = resolve_hooks_dir(home, existing, default_dir);

    if let Err(e) = std::fs::create_dir_all(&hooks_dir) {
        report.err(format!(
            "commit guard: mkdir {} failed: {e}",
            hooks_dir.display()
        ));
        return;
    }

    let hook_path = hooks_dir.join("commit-msg");
    // Don't clobber a foreign commit-msg the operator already maintains.
    if hook_path.exists()
        && let Ok(current) = std::fs::read_to_string(&hook_path)
        && !current.contains("Global git commit-msg guard")
    {
        report.skip(format!(
            "commit guard: {} already exists (not orca's) — left untouched",
            hook_path.display()
        ));
        return;
    }

    if let Err(e) = std::fs::write(&hook_path, COMMIT_MSG_GUARD) {
        report.err(format!(
            "commit guard: write {} failed: {e}",
            hook_path.display()
        ));
        return;
    }
    set_executable(&hook_path);

    // Materialize the global pre-push gate into the same hooks dir. A global
    // core.hooksPath shadows repo-local pre-push hooks, so without this nothing
    // runs fmt/clippy/test before a push and CI is the first gate.
    materialize_pre_push_gate(&hooks_dir, report);

    if !set_path {
        report.ok(format!(
            "commit guard: installed at {} (existing core.hooksPath)",
            hook_path.display()
        ));
        return;
    }

    let out = std::process::Command::new("git")
        .args([
            "config",
            "--global",
            "core.hooksPath",
            &hooks_dir.to_string_lossy(),
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => report.ok(format!(
            "commit guard: installed + global core.hooksPath = {}",
            hooks_dir.display()
        )),
        Ok(o) => report.err(format!(
            "commit guard: set core.hooksPath failed: {}",
            String::from_utf8_lossy(&o.stderr)
        )),
        Err(e) => report.err(format!("commit guard: git not found: {e}")),
    }
}

/// Write the global pre-push CI gate into `hooks_dir` (the active
/// `core.hooksPath`). Idempotent; won't clobber a foreign pre-push the operator
/// already maintains.
fn materialize_pre_push_gate(hooks_dir: &Path, report: &mut InstallReport) {
    let hook_path = hooks_dir.join("pre-push");
    if hook_path.exists()
        && let Ok(current) = std::fs::read_to_string(&hook_path)
        && !current.contains("Global git pre-push gate")
    {
        report.skip(format!(
            "pre-push gate: {} already exists (not orca's) — left untouched",
            hook_path.display()
        ));
        return;
    }
    if let Err(e) = std::fs::write(&hook_path, PRE_PUSH_GATE) {
        report.err(format!(
            "pre-push gate: write {} failed: {e}",
            hook_path.display()
        ));
        return;
    }
    set_executable(&hook_path);
    report.ok(format!(
        "pre-push gate: installed at {}",
        hook_path.display()
    ));
}

/// Resolve which hooks dir to write into and whether we must set the global
/// `core.hooksPath`. When git already reports a global `core.hooksPath`
/// (`existing`), honor it — expanding a leading `~/` against `home` — and don't
/// re-set the config. Otherwise fall back to `default_dir` and set the config.
fn resolve_hooks_dir(
    home: &Path,
    existing: Option<String>,
    default_dir: PathBuf,
) -> (PathBuf, bool) {
    match existing {
        Some(p) => {
            let expanded = if let Some(rest) = p.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(p)
            };
            (expanded, false)
        }
        None => (default_dir, true),
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
// Shared with `crate::install_status` (which reads the same real OS `$HOME`
// install layout). These deliberately use `$HOME`, not the `$ORCA_HOME`-aware
// `contract::config` resolver.

pub(crate) fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("cannot determine home directory")
}

pub(crate) fn install_bin_path(home: &Path) -> PathBuf {
    home.join(format!(".local/bin/{APP_NAME}"))
}

pub(crate) fn is_symlink(path: &Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // ── path_to_slug ──────────────────────────────────────────────────────

    #[test]
    fn slug_replaces_all_slashes_with_dashes() {
        assert_eq!(path_to_slug(Path::new("/a/b/c")), "-a-b-c");
    }

    #[test]
    fn slug_of_root_is_single_dash() {
        assert_eq!(path_to_slug(Path::new("/")), "-");
    }

    #[test]
    fn slug_relative_path_has_no_leading_dash() {
        assert_eq!(path_to_slug(Path::new("a/b")), "a-b");
    }

    #[test]
    fn slug_single_segment_unchanged() {
        assert_eq!(path_to_slug(Path::new("home")), "home");
    }

    // ── install_bin_path ──────────────────────────────────────────────────

    #[test]
    fn install_bin_path_is_local_bin_appname() {
        let home = Path::new("/home/u");
        assert_eq!(
            install_bin_path(home),
            PathBuf::from(format!("/home/u/.local/bin/{APP_NAME}"))
        );
    }

    #[test]
    fn install_bin_path_always_has_parent() {
        assert!(install_bin_path(Path::new("/home/u")).parent().is_some());
    }

    // ── home_dir ──────────────────────────────────────────────────────────

    #[test]
    fn home_dir_reads_home_env() {
        // SAFETY: single-threaded test; restore afterward.
        let prev = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", "/tmp/some-home") };
        assert_eq!(home_dir().unwrap(), PathBuf::from("/tmp/some-home"));
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    // ── is_symlink ────────────────────────────────────────────────────────

    #[test]
    fn is_symlink_false_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_symlink(&tmp.path().join("nope")));
    }

    #[test]
    fn is_symlink_false_for_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        assert!(!is_symlink(&f));
    }

    #[cfg(unix)]
    #[test]
    fn is_symlink_true_for_link() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(is_symlink(&link));
    }

    #[cfg(unix)]
    #[test]
    fn is_symlink_true_for_broken_link() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("broken");
        std::os::unix::fs::symlink(tmp.path().join("gone"), &link).unwrap();
        assert!(is_symlink(&link));
    }

    // ── find_git_root ─────────────────────────────────────────────────────

    #[test]
    fn find_git_root_finds_dot_git_at_start() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(find_git_root(&root), Some(root.clone()));
    }

    #[test]
    fn find_git_root_walks_up_from_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_git_root(&nested), Some(root));
    }

    #[test]
    fn find_git_root_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(find_git_root(&dir), None);
    }

    // ── resolve_hooks_dir ─────────────────────────────────────────────────

    #[test]
    fn resolve_hooks_dir_none_uses_default_and_sets() {
        let home = Path::new("/home/u");
        let default = PathBuf::from("/home/u/.config/git/hooks");
        let (dir, set) = resolve_hooks_dir(home, None, default.clone());
        assert_eq!(dir, default);
        assert!(set);
    }

    #[test]
    fn resolve_hooks_dir_expands_tilde() {
        let home = Path::new("/home/u");
        let (dir, set) =
            resolve_hooks_dir(home, Some("~/myhooks".to_string()), PathBuf::from("/def"));
        assert_eq!(dir, PathBuf::from("/home/u/myhooks"));
        assert!(!set);
    }

    #[test]
    fn resolve_hooks_dir_absolute_kept_verbatim() {
        let home = Path::new("/home/u");
        let (dir, set) = resolve_hooks_dir(
            home,
            Some("/etc/githooks".to_string()),
            PathBuf::from("/def"),
        );
        assert_eq!(dir, PathBuf::from("/etc/githooks"));
        assert!(!set);
    }

    // ── InstallReport ─────────────────────────────────────────────────────

    #[test]
    fn report_new_is_empty_and_successful() {
        let r = InstallReport::new();
        assert!(r.done.is_empty() && r.skipped.is_empty() && r.errors.is_empty());
        assert!(r.success());
    }

    #[test]
    fn report_collects_by_category() {
        let mut r = InstallReport::new();
        r.ok("a");
        r.ok("b");
        r.skip("s");
        r.err("e");
        assert_eq!(r.done, vec!["a", "b"]);
        assert_eq!(r.skipped, vec!["s"]);
        assert_eq!(r.errors, vec!["e"]);
    }

    #[test]
    fn report_success_false_when_errors_present() {
        let mut r = InstallReport::new();
        r.skip("only skipped");
        assert!(r.success());
        r.err("boom");
        assert!(!r.success());
    }

    // ── force_symlink ─────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn force_symlink_creates_link() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let dest = tmp.path().join("dest");
        let mut report = InstallReport::new();
        force_symlink(&src, &dest, &mut report, "lbl");
        assert!(is_symlink(&dest));
        assert_eq!(std::fs::read_link(&dest).unwrap(), src);
        assert_eq!(report.done.len(), 1);
        assert!(report.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn force_symlink_replaces_existing_link() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        let dest = tmp.path().join("dest");
        std::os::unix::fs::symlink(&old, &dest).unwrap();
        let mut report = InstallReport::new();
        force_symlink(&new, &dest, &mut report, "lbl");
        assert_eq!(std::fs::read_link(&dest).unwrap(), new);
        assert!(report.errors.is_empty());
    }

    // ── discover_projects ─────────────────────────────────────────────────

    fn mkrepo(base: &Path, rel: &str) {
        let p = base.join(rel);
        std::fs::create_dir_all(p.join(".git")).unwrap();
    }

    #[test]
    fn discover_always_includes_global() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = discover_projects(tmp.path());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].vault_name, "global");
        assert_eq!(projects[0].root, tmp.path());
    }

    #[test]
    fn discover_finds_level1_repo() {
        let tmp = tempfile::tempdir().unwrap();
        mkrepo(tmp.path(), "code/proj");
        let names: BTreeSet<_> = discover_projects(tmp.path())
            .into_iter()
            .map(|p| p.vault_name)
            .collect();
        assert!(names.contains("global"));
        assert!(names.contains("proj"));
    }

    #[test]
    fn discover_finds_level2_repo_with_hyphenated_name() {
        let tmp = tempfile::tempdir().unwrap();
        mkrepo(tmp.path(), "code/org/repo");
        let names: BTreeSet<_> = discover_projects(tmp.path())
            .into_iter()
            .map(|p| p.vault_name)
            .collect();
        assert!(names.contains("org-repo"));
    }

    #[test]
    fn discover_org_and_nested_repo_both_when_both_git() {
        let tmp = tempfile::tempdir().unwrap();
        // org itself is a repo AND contains a nested repo
        mkrepo(tmp.path(), "code/org");
        mkrepo(tmp.path(), "code/org/repo");
        let names: BTreeSet<_> = discover_projects(tmp.path())
            .into_iter()
            .map(|p| p.vault_name)
            .collect();
        assert!(names.contains("org"));
        assert!(names.contains("org-repo"));
    }

    #[test]
    fn discover_skips_non_git_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("code/plain")).unwrap();
        let names: BTreeSet<_> = discover_projects(tmp.path())
            .into_iter()
            .map(|p| p.vault_name)
            .collect();
        assert!(!names.contains("plain"));
    }

    #[test]
    fn discover_skips_hidden_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        mkrepo(tmp.path(), "code/.hidden");
        let names: BTreeSet<_> = discover_projects(tmp.path())
            .into_iter()
            .map(|p| p.vault_name)
            .collect();
        assert!(!names.contains(".hidden"));
    }

    #[test]
    fn discover_no_code_dir_returns_only_global() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = discover_projects(tmp.path());
        assert_eq!(projects.len(), 1);
    }

    #[test]
    fn discover_sets_slug_from_path() {
        let tmp = tempfile::tempdir().unwrap();
        mkrepo(tmp.path(), "code/proj");
        let proj = discover_projects(tmp.path())
            .into_iter()
            .find(|p| p.vault_name == "proj")
            .unwrap();
        assert_eq!(proj.slug, path_to_slug(&tmp.path().join("code/proj")));
    }

    // ── local_hostname ────────────────────────────────────────────────────

    #[test]
    fn local_hostname_nonempty() {
        // Real subprocess to `hostname`; always yields a non-empty string
        // (falls back to "unknown" if the command is unavailable).
        assert!(!local_hostname().is_empty());
    }

    // ── path_to_slug (more shapes) ────────────────────────────────────────

    #[test]
    fn slug_preserves_dashes_already_in_path() {
        assert_eq!(
            path_to_slug(Path::new("/home/u/code/argyle-labs/orca")),
            "-home-u-code-argyle-labs-orca"
        );
    }

    #[test]
    fn slug_trailing_slash_yields_trailing_dash() {
        assert_eq!(path_to_slug(Path::new("/a/b/")), "-a-b-");
    }

    // ── resolve_hooks_dir: empty existing falls through as Some ────────────

    #[test]
    fn resolve_hooks_dir_bare_tilde_not_expanded() {
        // Only a leading "~/" is expanded; a bare "~" stays literal.
        let (dir, set) = resolve_hooks_dir(
            Path::new("/home/u"),
            Some("~".to_string()),
            PathBuf::from("/def"),
        );
        assert_eq!(dir, PathBuf::from("~"));
        assert!(!set);
    }

    // ── discover_projects depth bound ─────────────────────────────────────

    #[test]
    fn discover_does_not_recurse_below_level_two() {
        let tmp = tempfile::tempdir().unwrap();
        // A level-3 repo (code/org/repo/deep) must not be discovered.
        mkrepo(tmp.path(), "code/org/repo/deep");
        let names: BTreeSet<_> = discover_projects(tmp.path())
            .into_iter()
            .map(|p| p.vault_name)
            .collect();
        assert!(!names.contains("deep"));
        assert!(!names.contains("repo-deep"));
    }

    #[test]
    fn discover_level2_skips_hidden_and_nongit() {
        let tmp = tempfile::tempdir().unwrap();
        mkrepo(tmp.path(), "code/org/.hidden");
        std::fs::create_dir_all(tmp.path().join("code/org/plain")).unwrap();
        let names: BTreeSet<_> = discover_projects(tmp.path())
            .into_iter()
            .map(|p| p.vault_name)
            .collect();
        assert!(!names.contains("org-.hidden"));
        assert!(!names.contains("org-plain"));
    }

    #[test]
    fn discover_root_paths_and_slugs_are_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        mkrepo(tmp.path(), "code/org/repo");
        let proj = discover_projects(tmp.path())
            .into_iter()
            .find(|p| p.vault_name == "org-repo")
            .unwrap();
        assert_eq!(proj.root, tmp.path().join("code/org/repo"));
        assert!(proj.slug.starts_with('-'));
    }

    // ── set_executable ────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn set_executable_sets_0755() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("bin");
        std::fs::write(&f, b"#!/bin/sh\n").unwrap();
        set_executable(&f);
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn set_executable_missing_file_is_noop() {
        // No panic on a nonexistent path.
        set_executable(Path::new("/nonexistent/orca/path/xyz"));
    }

    // ── step_claude_attribution_guard: materialize + idempotency ──────────

    #[cfg(unix)]
    #[test]
    fn attribution_guard_materializes_executable_with_marker() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_claude_attribution_guard(home.path(), &mut report);

        let hook = home.path().join(".claude/hooks/block-coauthor.sh");
        assert!(hook.exists(), "guard script must be written");
        let body = std::fs::read_to_string(&hook).unwrap();
        assert!(
            body.contains("orca-managed: Claude attribution guard"),
            "must carry the orca ownership marker"
        );
        let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "must be executable");
        assert!(report.errors.is_empty());
    }

    #[test]
    fn attribution_guard_leaves_foreign_script_untouched() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let hook = dir.join("block-coauthor.sh");
        std::fs::write(&hook, "#!/bin/sh\n# operator's own hook\n").unwrap();

        let mut report = InstallReport::new();
        step_claude_attribution_guard(home.path(), &mut report);

        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            "#!/bin/sh\n# operator's own hook\n",
            "a non-orca script must be left untouched"
        );
        assert!(report.skipped.iter().any(|m| m.contains("not orca's")));
    }

    #[test]
    fn attribution_guard_overwrites_its_own_prior_version() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let hook = dir.join("block-coauthor.sh");
        // A stale orca-owned version (carries the marker) is refreshed.
        std::fs::write(
            &hook,
            "#!/usr/bin/env bash\n# orca-managed: Claude attribution guard\n# old\n",
        )
        .unwrap();

        let mut report = InstallReport::new();
        step_claude_attribution_guard(home.path(), &mut report);

        let body = std::fs::read_to_string(&hook).unwrap();
        assert!(
            body.contains("patterns=("),
            "stale orca version must be refreshed"
        );
        assert!(report.errors.is_empty());
    }

    // ── InstallReport::print does not panic ───────────────────────────────

    #[test]
    fn report_print_runs() {
        let mut r = InstallReport::new();
        r.ok("done");
        r.skip("skipped");
        r.err("errored");
        r.print();
    }

    // ── step_vault_dirs ───────────────────────────────────────────────────

    #[test]
    fn vault_dirs_creates_memory_and_logs_sessions() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_vault_dirs(home.path(), &mut report);

        let vault = home.path().join(APP_STATE_DIR);
        assert!(vault.join("memory").is_dir());
        assert!(vault.join("logs/sessions").is_dir());
        assert!(report.errors.is_empty());
        assert_eq!(report.done.len(), 2);
    }

    #[test]
    fn vault_dirs_idempotent_on_second_run() {
        let home = tempfile::tempdir().unwrap();
        let mut r1 = InstallReport::new();
        step_vault_dirs(home.path(), &mut r1);
        let mut r2 = InstallReport::new();
        step_vault_dirs(home.path(), &mut r2);
        // create_dir_all is a no-error no-op when the dir already exists.
        assert!(r2.errors.is_empty());
        assert_eq!(r2.done.len(), 2);
    }

    // ── step_claude_md ────────────────────────────────────────────────────

    #[test]
    fn claude_md_writes_directive_file() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_claude_md(home.path(), &mut report);

        let md = home.path().join(".claude/CLAUDE.md");
        assert!(md.is_file());
        let body = std::fs::read_to_string(&md).unwrap();
        assert!(body.starts_with(GLOBAL_CLAUDE_MD));
        assert!(report.errors.is_empty());
        assert!(
            report
                .done
                .iter()
                .any(|m| m.contains("orca-first directive"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_md_replaces_prior_symlink_without_following_it() {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        // A previous install left CLAUDE.md as a symlink into a repo file.
        let repo_file = home.path().join("repo_claude.md");
        std::fs::write(&repo_file, "ORIGINAL REPO CONTENT").unwrap();
        let link = claude_dir.join("CLAUDE.md");
        std::os::unix::fs::symlink(&repo_file, &link).unwrap();

        let mut report = InstallReport::new();
        step_claude_md(home.path(), &mut report);

        // The link was dropped and replaced by a regular file; the repo file
        // it used to point at must be untouched (write did not follow it).
        assert!(!is_symlink(&link));
        assert_eq!(
            std::fs::read_to_string(&repo_file).unwrap(),
            "ORIGINAL REPO CONTENT"
        );
        assert!(report.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn claude_md_clears_legacy_vault_symlink() {
        let home = tempfile::tempdir().unwrap();
        let vault = home.path().join(APP_STATE_DIR);
        std::fs::create_dir_all(&vault).unwrap();
        let legacy_target = home.path().join("legacy_target.md");
        std::fs::write(&legacy_target, "x").unwrap();
        let legacy = vault.join("CLAUDE.md");
        std::os::unix::fs::symlink(&legacy_target, &legacy).unwrap();

        let mut report = InstallReport::new();
        step_claude_md(home.path(), &mut report);

        assert!(!legacy.exists(), "legacy vault symlink must be removed");
        assert!(report.errors.is_empty());
    }

    // ── step_memory_symlinks ──────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn memory_symlinks_creates_global_link() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_memory_symlinks(home.path(), &mut report);

        let global_slug = path_to_slug(home.path());
        let link = home
            .path()
            .join(".claude/projects")
            .join(global_slug)
            .join("memory");
        assert!(is_symlink(&link), "global memory link must exist");
        let target = home.path().join(APP_STATE_DIR).join("memory/global");
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert!(report.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn memory_symlinks_backs_up_existing_real_dir() {
        let home = tempfile::tempdir().unwrap();
        // Pre-create a REAL memory dir where the global link would go.
        let global_slug = path_to_slug(home.path());
        let project_dir = home.path().join(".claude/projects").join(&global_slug);
        std::fs::create_dir_all(project_dir.join("memory")).unwrap();
        std::fs::write(project_dir.join("memory/note.md"), "keep me").unwrap();

        let mut report = InstallReport::new();
        step_memory_symlinks(home.path(), &mut report);

        // The real dir was moved to memory.bak and a symlink took its place.
        let backup = project_dir.join("memory.bak");
        assert!(backup.is_dir());
        assert_eq!(
            std::fs::read_to_string(backup.join("note.md")).unwrap(),
            "keep me"
        );
        assert!(is_symlink(&project_dir.join("memory")));
        assert!(
            report
                .done
                .iter()
                .any(|m| m.contains("backed up existing dir"))
        );
    }

    // ── step_reap_stale_mcp_serve (no-binary skip branch) ─────────────────

    #[test]
    fn reap_skips_when_no_installed_binary() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_reap_stale_mcp_serve(home.path(), &mut report);
        assert!(
            report
                .skipped
                .iter()
                .any(|m| m.contains("no installed binary"))
        );
        assert!(report.done.is_empty());
        assert!(report.errors.is_empty());
    }

    // ── step_install_binary ───────────────────────────────────────────────

    #[test]
    fn install_binary_copies_current_exe_to_local_bin() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_install_binary(home.path(), &mut report);

        let dest = install_bin_path(home.path());
        assert!(dest.is_file(), "binary must be copied into ~/.local/bin");
        assert!(!is_symlink(&dest), "installed binary must be a real file");
        assert!(report.errors.is_empty());
        assert!(report.done.iter().any(|m| m.contains("installed to")));
    }

    #[cfg(unix)]
    #[test]
    fn install_binary_replaces_stale_symlink() {
        let home = tempfile::tempdir().unwrap();
        let dest = install_bin_path(home.path());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        // A prior install left a symlink pointing at a build artifact.
        let artifact = home.path().join("artifact");
        std::fs::write(&artifact, "artifact bytes").unwrap();
        std::os::unix::fs::symlink(&artifact, &dest).unwrap();

        let mut report = InstallReport::new();
        step_install_binary(home.path(), &mut report);

        // The symlink was removed and replaced by a real copied file; the
        // build artifact it pointed at was NOT overwritten in place.
        assert!(!is_symlink(&dest));
        assert!(dest.is_file());
        assert_eq!(
            std::fs::read_to_string(&artifact).unwrap(),
            "artifact bytes",
            "fs::copy must not follow the stale symlink into the artifact"
        );
        assert!(
            report
                .done
                .iter()
                .any(|m| m.contains("removed stale symlink"))
        );
    }

    // ── step_pki_init + step_cli_client_cert ──────────────────────────────

    #[test]
    fn pki_init_then_reports_already_on_second_run() {
        let home = tempfile::tempdir().unwrap();
        let mut r1 = InstallReport::new();
        step_pki_init(home.path(), &mut r1);
        assert!(
            r1.errors.is_empty(),
            "pki init should succeed: {:?}",
            r1.errors
        );
        assert!(r1.done.iter().any(|m| m.contains("pki: initialized")));

        let pki_dir = home.path().join(APP_STATE_DIR).join(APP_PKI_DIR);
        assert!(utils::pki::ca_cert_path(&pki_dir).exists());
        assert!(utils::pki::server_cert_path(&pki_dir).exists());

        let mut r2 = InstallReport::new();
        step_pki_init(home.path(), &mut r2);
        assert!(r2.skipped.iter().any(|m| m.contains("already initialized")));
    }

    #[test]
    fn cli_client_cert_issued_then_skipped_when_present() {
        let home = tempfile::tempdir().unwrap();
        // CA must exist before a client cert can be signed.
        let mut init = InstallReport::new();
        step_pki_init(home.path(), &mut init);
        assert!(init.errors.is_empty());

        let mut r1 = InstallReport::new();
        step_cli_client_cert(home.path(), &mut r1);
        assert!(
            r1.errors.is_empty(),
            "issue should succeed: {:?}",
            r1.errors
        );
        assert!(r1.done.iter().any(|m| m.contains("issued client cert")));

        let pki_dir = home.path().join(APP_STATE_DIR).join(APP_PKI_DIR);
        assert!(utils::pki::cli_client_cert_path(&pki_dir).exists());
        assert!(utils::pki::cli_client_key_path(&pki_dir).exists());

        let mut r2 = InstallReport::new();
        step_cli_client_cert(home.path(), &mut r2);
        assert!(r2.skipped.iter().any(|m| m.contains("already present")));
    }

    // ── step_remove_binary ────────────────────────────────────────────────

    #[test]
    fn remove_binary_skips_when_absent() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_remove_binary(home.path(), &mut report);
        assert!(report.skipped.iter().any(|m| m.contains("not found")));
    }

    #[test]
    fn remove_binary_removes_present_file() {
        let home = tempfile::tempdir().unwrap();
        let bin = install_bin_path(home.path());
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"orca").unwrap();

        let mut report = InstallReport::new();
        step_remove_binary(home.path(), &mut report);
        assert!(!bin.exists());
        assert!(report.done.iter().any(|m| m.contains("removed")));
    }

    // ── step_remove_claude_md ─────────────────────────────────────────────

    #[test]
    fn remove_claude_md_all_absent_skips() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_remove_claude_md(home.path(), &mut report);
        assert!(report.errors.is_empty());
        assert!(
            report
                .skipped
                .iter()
                .any(|m| m.contains("vault CLAUDE.md: not present"))
        );
        assert!(
            report
                .skipped
                .iter()
                .any(|m| m.contains("~/.claude/CLAUDE.md: not present"))
        );
    }

    #[test]
    fn remove_claude_md_removes_orca_managed_file() {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("CLAUDE.md"), GLOBAL_CLAUDE_MD).unwrap();

        let mut report = InstallReport::new();
        step_remove_claude_md(home.path(), &mut report);
        assert!(!claude_dir.join("CLAUDE.md").exists());
        assert!(
            report
                .done
                .iter()
                .any(|m| m == "~/.claude/CLAUDE.md: removed")
        );
    }

    #[test]
    fn remove_claude_md_keeps_user_modified_file() {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let md = claude_dir.join("CLAUDE.md");
        std::fs::write(&md, "user's own directives").unwrap();

        let mut report = InstallReport::new();
        step_remove_claude_md(home.path(), &mut report);
        assert!(md.exists(), "user-modified file must be left in place");
        assert!(report.skipped.iter().any(|m| m.contains("user-modified")));
    }

    #[cfg(unix)]
    #[test]
    fn remove_claude_md_removes_legacy_symlinks() {
        let home = tempfile::tempdir().unwrap();
        let vault = home.path().join(APP_STATE_DIR);
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::create_dir_all(&claude_dir).unwrap();
        let target = home.path().join("target.md");
        std::fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, vault.join("CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink(&target, claude_dir.join("CLAUDE.md")).unwrap();

        let mut report = InstallReport::new();
        step_remove_claude_md(home.path(), &mut report);

        assert!(!vault.join("CLAUDE.md").exists());
        assert!(!claude_dir.join("CLAUDE.md").exists());
        assert!(
            report
                .done
                .iter()
                .any(|m| m.contains("vault CLAUDE.md: removed legacy symlink"))
        );
        assert!(
            report
                .done
                .iter()
                .any(|m| m.contains("~/.claude/CLAUDE.md: removed legacy symlink"))
        );
        // The shared target the links pointed at must survive.
        assert!(target.exists());
    }

    #[test]
    fn remove_claude_md_keeps_regular_vault_file() {
        let home = tempfile::tempdir().unwrap();
        let vault = home.path().join(APP_STATE_DIR);
        std::fs::create_dir_all(&vault).unwrap();
        let vault_md = vault.join("CLAUDE.md");
        std::fs::write(&vault_md, "plain user file").unwrap();

        let mut report = InstallReport::new();
        step_remove_claude_md(home.path(), &mut report);
        assert!(vault_md.exists(), "a plain vault file is user-owned");
        assert!(report.skipped.iter().any(|m| m.contains("not a symlink")));
    }

    // ── step_global_commit_guard: materialize_pre_push_gate ────────────────
    // The full step runs `git config --global`, which touches real machine
    // state, so it is not exercised here. The pre-push materializer is a pure
    // file-render helper and is covered directly against a tempdir.

    #[cfg(unix)]
    #[test]
    fn pre_push_gate_materializes_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        materialize_pre_push_gate(tmp.path(), &mut report);

        let hook = tmp.path().join("pre-push");
        assert!(hook.is_file());
        let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(
            report
                .done
                .iter()
                .any(|m| m.contains("pre-push gate: installed"))
        );
    }

    #[test]
    fn pre_push_gate_leaves_foreign_hook_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = tmp.path().join("pre-push");
        std::fs::write(&hook, "#!/bin/sh\n# operator's own pre-push\n").unwrap();

        let mut report = InstallReport::new();
        materialize_pre_push_gate(tmp.path(), &mut report);
        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            "#!/bin/sh\n# operator's own pre-push\n"
        );
        assert!(report.skipped.iter().any(|m| m.contains("not orca's")));
    }

    #[test]
    fn pre_push_gate_refreshes_its_own_prior_version() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = tmp.path().join("pre-push");
        std::fs::write(&hook, "#!/bin/sh\n# Global git pre-push gate (old)\n").unwrap();

        let mut report = InstallReport::new();
        materialize_pre_push_gate(tmp.path(), &mut report);
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), PRE_PUSH_GATE);
        assert!(report.done.iter().any(|m| m.contains("installed")));
    }

    // ── InstallReport serde round-trip ────────────────────────────────────

    #[test]
    fn install_report_serde_round_trips() {
        let mut r = InstallReport::new();
        r.ok("did a thing");
        r.skip("skipped a thing");
        r.err("failed a thing");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"done\":[\"did a thing\"]"));
        assert!(s.contains("\"skipped\":[\"skipped a thing\"]"));
        assert!(s.contains("\"errors\":[\"failed a thing\"]"));

        let back: InstallReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back.done, r.done);
        assert_eq!(back.skipped, r.skipped);
        assert_eq!(back.errors, r.errors);
        assert!(!back.success());
    }

    // ── step_reap_stale_mcp_serve: binary present, nothing stale ───────────

    #[test]
    fn reap_skips_when_binary_present_but_nothing_stale() {
        // A freshly-written binary has an mtime of "now", so any older
        // mcp-serve would be stale — but in the hermetic test process there
        // are none, so the step reports the no-stale skip.
        let home = tempfile::tempdir().unwrap();
        let bin = install_bin_path(home.path());
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"orca").unwrap();

        let mut report = InstallReport::new();
        step_reap_stale_mcp_serve(home.path(), &mut report);
        assert!(report.errors.is_empty());
        assert!(
            report
                .skipped
                .iter()
                .any(|m| m.contains("no stale mcp-serve"))
                || report.done.iter().any(|m| m.contains("stale mcp-serve"))
        );
    }

    // ── error branches: create_dir_all fails when a path component is a file ─
    // On Unix, `create_dir_all` errors if an ancestor exists as a regular
    // file. Each step below is driven into its error arm by planting such a
    // file where a directory is expected, exercising the `report.err(...)`
    // paths that the happy-path tests never reach.

    #[cfg(unix)]
    #[test]
    fn vault_dirs_errors_when_vault_path_is_a_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(APP_STATE_DIR), b"not a dir").unwrap();
        let mut report = InstallReport::new();
        step_vault_dirs(home.path(), &mut report);
        assert_eq!(report.errors.len(), 2, "both vault dirs must error");
        assert!(report.done.is_empty());
        assert!(report.errors.iter().all(|m| m.starts_with("vault dir")));
    }

    #[cfg(unix)]
    #[test]
    fn claude_md_errors_when_dot_claude_is_a_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".claude"), b"blocker").unwrap();
        let mut report = InstallReport::new();
        step_claude_md(home.path(), &mut report);
        assert!(report.done.is_empty());
        assert!(
            report
                .errors
                .iter()
                .any(|m| m.contains("~/.claude: mkdir failed"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn attribution_guard_errors_when_claude_dir_is_a_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".claude"), b"blocker").unwrap();
        let mut report = InstallReport::new();
        step_claude_attribution_guard(home.path(), &mut report);
        assert!(report.done.is_empty());
        assert!(
            report
                .errors
                .iter()
                .any(|m| m.contains("attribution guard: mkdir"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_binary_errors_when_local_is_a_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".local"), b"blocker").unwrap();
        let mut report = InstallReport::new();
        step_install_binary(home.path(), &mut report);
        assert!(!install_bin_path(home.path()).exists());
        assert!(report.done.is_empty());
        assert!(
            report
                .errors
                .iter()
                .any(|m| m.contains("cannot create ~/.local/bin"))
        );
    }

    #[test]
    fn cli_client_cert_errors_without_ca() {
        let home = tempfile::tempdir().unwrap();
        let mut report = InstallReport::new();
        step_cli_client_cert(home.path(), &mut report);
        assert!(report.skipped.is_empty(), "nothing exists yet to skip on");
        assert!(report.done.is_empty(), "issue cannot succeed without a CA");
        assert!(
            report
                .errors
                .iter()
                .any(|m| m.contains("pki/cli: issue failed"))
        );
    }

    // ── force_symlink error arm ───────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn force_symlink_reports_error_when_dest_parent_missing() {
        // symlink(2) fails with ENOENT when the destination's parent dir does
        // not exist, driving the Err arm that the happy-path tests never hit.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let dest = tmp.path().join("no_such_dir").join("dest");
        let mut report = InstallReport::new();
        force_symlink(&src, &dest, &mut report, "lbl");
        assert!(!is_symlink(&dest));
        assert!(report.done.is_empty());
        assert!(
            report
                .errors
                .iter()
                .any(|m| m.contains("lbl: symlink failed"))
        );
    }

    // ── step_memory_symlinks backup-rename failure arm ────────────────────

    #[cfg(unix)]
    #[test]
    fn memory_symlinks_reports_error_when_backup_rename_fails() {
        let home = tempfile::tempdir().unwrap();
        let global_slug = path_to_slug(home.path());
        let project_dir = home.path().join(".claude/projects").join(&global_slug);
        // A REAL memory dir occupies the link site (not a symlink) so the step
        // tries to back it up to memory.bak.
        std::fs::create_dir_all(project_dir.join("memory")).unwrap();
        // memory.bak already exists and is NON-empty, so rename onto it fails
        // with ENOTEMPTY — exercising the "cannot back up existing dir" arm.
        std::fs::create_dir_all(project_dir.join("memory.bak/occupied")).unwrap();
        std::fs::write(project_dir.join("memory.bak/occupied/f"), b"x").unwrap();

        let mut report = InstallReport::new();
        step_memory_symlinks(home.path(), &mut report);

        assert!(
            report
                .errors
                .iter()
                .any(|m| m.contains("cannot back up existing dir")),
            "expected backup-rename error, got {:?}",
            report.errors
        );
        // The original real dir is left in place (no symlink took over).
        assert!(!is_symlink(&project_dir.join("memory")));
        assert!(project_dir.join("memory").is_dir());
    }

    // ── cmd_{install,uninstall}_report home-resolution error arms ─────────
    // nextest runs each test in its own process, so clearing HOME here does
    // not leak into sibling tests; we restore it regardless.

    #[test]
    fn cmd_install_report_errors_without_home() {
        let prev = std::env::var("HOME").ok();
        unsafe { std::env::remove_var("HOME") };
        let r = cmd_install_report();
        if let Some(v) = prev {
            unsafe { std::env::set_var("HOME", v) }
        }
        assert!(!r.success());
        assert!(r.done.is_empty());
        assert!(
            r.errors
                .iter()
                .any(|m| m.contains("cannot determine home directory"))
        );
    }

    #[test]
    fn cmd_uninstall_report_errors_without_home() {
        let prev = std::env::var("HOME").ok();
        unsafe { std::env::remove_var("HOME") };
        let r = cmd_uninstall_report();
        if let Some(v) = prev {
            unsafe { std::env::set_var("HOME", v) }
        }
        assert!(!r.success());
        assert!(
            r.errors
                .iter()
                .any(|m| m.contains("cannot determine home directory"))
        );
    }

    // ── step_pki_init error arm ───────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn pki_init_errors_when_state_dir_is_a_file() {
        // Plant a regular file where the ~/.orca state dir must be, so the
        // pki dir under it cannot be created and init fails — driving the
        // `report.err("pki: init failed")` arm the happy path never reaches.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(APP_STATE_DIR), b"blocker").unwrap();
        let mut report = InstallReport::new();
        step_pki_init(home.path(), &mut report);
        assert!(report.done.is_empty());
        assert!(report.skipped.is_empty());
        assert!(
            report.errors.iter().any(|m| m.contains("pki: init failed")),
            "expected init-failed error, got {:?}",
            report.errors
        );
    }

    // ── materialize_pre_push_gate write-failure arm ───────────────────────

    #[test]
    fn pre_push_gate_errors_when_hooks_dir_missing() {
        // The materializer does not create its parent dir; pointing it at a
        // nonexistent directory makes std::fs::write fail with ENOENT,
        // exercising the "pre-push gate: write ... failed" error arm.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no_such_dir");
        let mut report = InstallReport::new();
        materialize_pre_push_gate(&missing, &mut report);
        assert!(report.done.is_empty());
        assert!(
            report
                .errors
                .iter()
                .any(|m| m.contains("pre-push gate: write")),
            "expected write-failed error, got {:?}",
            report.errors
        );
    }

    #[test]
    fn remove_claude_md_keeps_regular_dot_claude_file_that_is_not_ours() {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let md = claude_dir.join("CLAUDE.md");
        std::fs::write(&md, "hand-written directives, definitely not orca's").unwrap();
        let mut report = InstallReport::new();
        step_remove_claude_md(home.path(), &mut report);
        assert!(md.exists(), "a user-modified regular file must survive");
        assert!(report.errors.is_empty());
        assert!(
            report
                .skipped
                .iter()
                .any(|m| m.contains("~/.claude/CLAUDE.md: user-modified"))
        );
    }
}
