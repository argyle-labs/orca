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
    // Materialize embedded agents to `~/.claude/agents/<name>.md` so Claude
    // Code's native Agent picker auto-discovers them — no MCP roundtrip
    // required. Also writes per-project copies under each known
    // `<project>/.claude/agents/` so project-scoped agents override globals.
    step_claude_agents(&home, &mut report);
    // Compose every registered provider's skills + slash commands into
    // `~/.claude/skills/<name>/` and `~/.claude/commands/<name>.md`. Empty until
    // a plugin registers them, but the sink is wired now so composition is the
    // single path — see `docs/CAPABILITY-REGISTRIES.md`.
    step_claude_skills(&home, &mut report);
    step_claude_commands(&home, &mut report);
    // Compose provider hooks into ~/.claude/settings.json's `hooks` subtree.
    // No-op unless a plugin registers a hook — so a hand-managed settings file
    // is left untouched today.
    step_claude_hooks(&home, &mut report);
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

/// External repos that own their own agent rosters. Each entry is a path
/// (relative to `$HOME/code/`) to a directory containing `<name>.md` files.
/// Discovered at install time and merged with orca's embedded agents.
///
/// To register a new external source, add the path here. Future: read this
/// list from `orca.db` so plugins can self-register without recompiling
/// orca.
const EXTERNAL_AGENT_SOURCES: &[&str] = &[];

/// One agent prompt resolved at install time: either embedded in the orca
/// binary or read from an external source repo. `body` is the full file
/// contents (frontmatter + prompt), ready to write verbatim.
struct AgentEntry {
    name: String,
    body: String,
    origin: String,
}

/// Register every agent source as an [`agents::AgentProvider`] and return the
/// composed roster. The compiled-in base roster and each external source repo
/// are both bridged into the process-global registry, so `compose_agents()` is
/// the single source of truth shared with the internal chat roster — the
/// capability-registry seam (see `docs/CAPABILITY-REGISTRIES.md`). Registration
/// order is precedence: base first, external after, so an external source wins
/// on name collision, exactly as before.
///
/// When the base roster moves to the external `argyle-labs/agents` plugin, the
/// `register_base_roster()` call goes away and the plugin registers itself —
/// nothing else here changes.
fn collect_agent_entries(home: &Path) -> Vec<AgentEntry> {
    agents::embedded::register_base_roster();

    for rel in EXTERNAL_AGENT_SOURCES {
        let dir = home.join("code").join(rel);
        agents::register_provider(std::sync::Arc::new(
            agents::embedded::FsRosterProvider::new(format!("~/code/{rel}"), dir),
        ));
    }

    agents::compose_agents()
        .into_iter()
        .map(|a| AgentEntry {
            name: a.name,
            body: a.body,
            origin: a.origin,
        })
        .collect()
}

/// Materialize every agent (embedded + external sources) to
/// `~/.claude/agents/<name>.md` so Claude Code's native Agent picker
/// discovers them automatically.
///
/// Overwrite policy: unconditional. Re-run on every `orca install` /
/// `orca update` / daemon start. Users who want to edit an agent's prompt
/// should fork it to a different name (e.g. `wolf-custom.md`).
///
/// Also actively cleans up any per-project `<project>/.claude/agents/<name>.md`
/// files orca wrote in a previous version — those should only ever live in the
/// global dir.
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
        let dir = project.root.join(".claude/agents");
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
        if removed > 0 {
            report.ok(format!(
                "{}: removed {removed} stale per-project agents",
                dir.display()
            ));
        }
        if std::fs::read_dir(&dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            _ = std::fs::remove_dir(&dir);
        }
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
    // Resolve symlinks: if target_dir is a symlink (broken or live), create
    // the link's destination instead so create_dir_all doesn't trip EEXIST
    // on the link entry when the destination is missing.
    let resolved = std::fs::read_link(target_dir).unwrap_or_else(|_| target_dir.to_path_buf());
    let real_target = if resolved.is_absolute() {
        resolved
    } else {
        target_dir.parent().unwrap_or(target_dir).join(resolved)
    };
    if let Err(e) = std::fs::create_dir_all(&real_target) {
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

/// Materialize composed skills to `~/.claude/skills/<name>/` — a directory per
/// skill holding `SKILL.md` plus any supporting files. Sourced from every
/// registered [`agents::AgentProvider`] via `compose_skills()`, so a plugin can
/// ship skills the same way it ships agents.
fn step_claude_skills(home: &Path, report: &mut InstallReport) {
    let skills = agents::compose_skills();
    if skills.is_empty() {
        return;
    }
    let root = home.join(".claude/skills");
    let mut written = 0usize;
    for skill in &skills {
        let dir = root.join(&skill.name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            report.err(format!(
                "~/.claude/skills/{}: mkdir failed: {e}",
                skill.name
            ));
            continue;
        }
        let mut ok = true;
        for file in &skill.files {
            let path = dir.join(&file.path);
            if let Some(parent) = path.parent() {
                _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, &file.contents) {
                report.err(format!(
                    "~/.claude/skills/{}/{}: write failed: {e}",
                    skill.name, file.path
                ));
                ok = false;
            }
        }
        if ok {
            written += 1;
        }
    }
    report.ok(format!("~/.claude/skills: materialized {written} skills"));
}

/// Materialize composed slash commands to `~/.claude/commands/<name>.md`.
/// Sourced from every registered [`agents::AgentProvider`] via
/// `compose_commands()`.
fn step_claude_commands(home: &Path, report: &mut InstallReport) {
    let commands = agents::compose_commands();
    if commands.is_empty() {
        return;
    }
    let dir = home.join(".claude/commands");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        report.err(format!("~/.claude/commands: mkdir failed: {e}"));
        return;
    }
    let mut written = 0usize;
    for command in &commands {
        let path = dir.join(format!("{}.md", command.name));
        match std::fs::write(&path, &command.body) {
            Ok(_) => written += 1,
            Err(e) => report.err(format!(
                "~/.claude/commands/{}.md: write failed: {e}",
                command.name
            )),
        }
    }
    report.ok(format!(
        "~/.claude/commands: materialized {written} commands"
    ));
}

/// Compose provider hooks into `~/.claude/settings.json`'s `hooks` subtree.
///
/// Returns immediately when no provider contributes a hook — the common case —
/// so a hand-managed settings file is never rewritten. When hooks DO exist,
/// orca takes ownership of the file: it round-trips through the typed
/// [`agents::ClaudeSettings`] (no opaque JSON, per the hard rule), replacing the
/// `hooks` subtree and preserving every key orca models. Settings keys orca
/// does not model are intentionally dropped — this is the accepted tradeoff of
/// the fully-typed model (see `docs/CAPABILITY-REGISTRIES.md`).
fn step_claude_hooks(home: &Path, report: &mut InstallReport) {
    let tree = agents::hooks_to_settings_tree(&agents::compose_hooks());
    if tree.is_empty() {
        return;
    }

    let path = home.join(".claude/settings.json");
    // Start from the existing (typed) settings if present so modeled keys
    // survive; otherwise a fresh document with only the hooks subtree.
    let mut settings: agents::ClaudeSettings = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    let hook_count: usize = tree.values().map(|groups| groups.len()).sum();
    settings.hooks = tree;

    match serde_json::to_string_pretty(&settings) {
        Ok(json) => match std::fs::write(&path, json + "\n") {
            Ok(_) => report.ok(format!(
                "~/.claude/settings.json: composed {hook_count} hook matcher group(s)"
            )),
            Err(e) => report.err(format!("~/.claude/settings.json: write failed: {e}")),
        },
        Err(e) => report.err(format!("~/.claude/settings.json: serialize failed: {e}")),
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

    let (hooks_dir, set_path) = match existing {
        Some(p) => {
            let expanded = if let Some(rest) = p.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(p)
            };
            (expanded, false)
        }
        None => (default_dir, true),
    };

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
