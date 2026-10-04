//! `identity.privilege.audit` — who can escalate on this host, and how.
//!
//! Answers the question nothing in orca could answer before: *"who can become
//! root here, via what, and would removing a rule lock the operator out?"*
//!
//! Filed as orca#681 after a retired `halvor` footprint was removed from
//! freyr/baldur/thor/frigg (2026-10-01) and three classes of latent problem
//! surfaced, none of which were visible from any orca surface:
//!
//! - **dangling grants** — `svc` held `NOPASSWD` on three absolute paths, two of
//!   which existed on no host at all. A `NOPASSWD` rule naming a non-existent
//!   path is a standing escalation: whoever can create that path gets root.
//! - **sole grants** — `skey ALL=(ALL) NOPASSWD: ALL` lived *only* inside the
//!   retired file, and no group rule (`%sudo`/`%wheel`) granted it anywhere, so
//!   deleting that file would have locked the operator out of four hosts. That
//!   had to be found by reading the file by hand.
//! - **two privilege systems** — Debian hosts use `sudoers.d`, Alpine hosts also
//!   carry `doas.conf`. Both had to be audited separately.
//!
//! Read-only and side-effect free: pure filesystem parsing, no DB, no writes, so
//! it is safe to run anywhere at any time. Peer-dispatchable, so
//! `orca identity privilege audit --peer <host>` reports that host's own state
//! (the handler runs there).
//!
//! **Coverage is honest about privilege.** `/etc/sudoers*` is typically
//! `0440 root:root` and the daemon does not necessarily run as root, so files it
//! cannot read are reported in `unreadable` rather than silently skipped — a
//! partial audit must never read as a clean one.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One parsed privilege rule, from either sudoers or doas.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PrivilegeRule {
    /// File the rule came from.
    pub source: String,
    /// 1-indexed line within that file.
    pub line: usize,
    /// `sudoers` or `doas`.
    pub system: String,
    /// Who the rule grants to: a username, or `%group` / `:group` for a group.
    pub principal: String,
    /// True when `principal` names a group rather than a user.
    pub is_group: bool,
    /// Whether the rule skips password entry (`NOPASSWD:` / doas `nopass`).
    pub nopasswd: bool,
    /// Commands granted. `["ALL"]` means unrestricted.
    pub commands: Vec<String>,
    /// Verbatim line, for operator context.
    pub raw: String,
}

/// A finding worth acting on.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PrivilegeFinding {
    /// Machine-readable class: `dangling_target`, `sole_grant`, `nopasswd_all`,
    /// or `no_group_grant`.
    pub kind: String,
    /// `high` | `medium` | `info`.
    pub severity: String,
    pub principal: String,
    /// Where it was found, when tied to a specific rule.
    pub source: Option<String>,
    pub line: Option<usize>,
    /// What is wrong, in one sentence.
    pub detail: String,
}

/// Result of an audit on one host.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PrivilegeAudit {
    pub host: String,
    /// Every rule parsed, in discovery order.
    pub rules: Vec<PrivilegeRule>,
    /// Findings, most severe first.
    pub findings: Vec<PrivilegeFinding>,
    /// Files that exist but could not be read (usually `0440 root:root` while the
    /// daemon runs unprivileged). Coverage is PARTIAL whenever this is non-empty.
    pub unreadable: Vec<String>,
    /// True when every discovered file was readable.
    pub complete: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct IdentityPrivilegeAuditArgs {
    /// Override the filesystem root to audit. Primarily for tests; defaults to `/`.
    #[arg(long)]
    pub root: Option<String>,
}

/// Strip a trailing comment and surrounding whitespace.
fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or("").trim()
}

/// Parse one sudoers rule line. Returns `None` for anything that is not a
/// user/group specification (`Defaults`, `#includedir`, aliases, blanks).
fn parse_sudoers_line(raw: &str) -> Option<(String, bool, bool, Vec<String>)> {
    let line = strip_comment(raw);
    if line.is_empty() || line.starts_with("Defaults") || line.starts_with('@') {
        return None;
    }
    // Alias definitions (`User_Alias FOO = ...`) are not grants.
    for alias in ["User_Alias", "Runas_Alias", "Host_Alias", "Cmnd_Alias"] {
        if line.starts_with(alias) {
            return None;
        }
    }
    let (left, right) = line.split_once('=')?;
    let principal = left.split_whitespace().next()?.to_string();
    if principal.is_empty() {
        return None;
    }
    let is_group = principal.starts_with('%') || principal.starts_with('+');

    // Drop a leading `(runas)` spec, then any tags, leaving the command list.
    let mut rest = right.trim();
    if rest.starts_with('(') {
        let close = rest.find(')')?;
        rest = rest[close + 1..].trim();
    }
    let mut nopasswd = false;
    loop {
        let upper = rest.to_ascii_uppercase();
        let tag = [
            "NOPASSWD:",
            "PASSWD:",
            "NOEXEC:",
            "EXEC:",
            "SETENV:",
            "NOSETENV:",
        ]
        .into_iter()
        .find(|t| upper.starts_with(t));
        match tag {
            Some(t) => {
                if t == "NOPASSWD:" {
                    nopasswd = true;
                }
                rest = rest[t.len()..].trim();
            }
            None => break,
        }
    }
    let commands: Vec<String> = rest
        .split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    if commands.is_empty() {
        return None;
    }
    Some((principal, is_group, nopasswd, commands))
}

/// Parse one `doas.conf` rule: `permit|deny [nopass|persist|keepenv] <ident>
/// [as <user>] [cmd <command> [args ...]]`.
fn parse_doas_line(raw: &str) -> Option<(String, bool, bool, Vec<String>)> {
    let line = strip_comment(raw);
    if line.is_empty() {
        return None;
    }
    let mut it = line.split_whitespace();
    let action = it.next()?;
    if action != "permit" {
        return None; // `deny` grants nothing.
    }
    let mut nopasswd = false;
    let mut principal: Option<String> = None;
    let mut command: Option<String> = None;
    while let Some(tok) = it.next() {
        match tok {
            "nopass" => nopasswd = true,
            "persist" | "keepenv" | "setenv" => {}
            "as" => {
                it.next();
            }
            "cmd" => {
                command = it.next().map(str::to_string);
                break;
            }
            other if principal.is_none() => principal = Some(other.to_string()),
            _ => {}
        }
    }
    let principal = principal?;
    let is_group = principal.starts_with(':');
    let commands = vec![command.unwrap_or_else(|| "ALL".to_string())];
    Some((principal, is_group, nopasswd, commands))
}

/// Collect the sudoers/doas files to inspect, in a stable order.
fn candidate_files(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.join("etc/sudoers")];
    let dir = root.join("etc/sudoers.d");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| {
                // sudo itself ignores backup/temp names.
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.ends_with('~') && !n.contains(".bak") && !n.contains('.'))
            })
            .collect();
        found.sort();
        out.extend(found);
    }
    out.push(root.join("etc/doas.conf"));
    out
}

/// Does a granted command refer to an absolute path that is absent?
fn is_dangling(root: &Path, cmd: &str) -> bool {
    let bare = cmd.split_whitespace().next().unwrap_or(cmd);
    if !bare.starts_with('/') {
        return false; // `ALL`, aliases and relative forms are not path claims.
    }
    if bare.contains('*') || bare.contains('?') {
        return false; // globs may legitimately match nothing right now.
    }
    let joined = root.join(bare.trim_start_matches('/'));
    !joined.exists()
}

/// Audit a filesystem root. Split out from the tool fn so tests can drive it
/// against a fixture tree.
pub fn audit_root(root: &Path, host: String) -> PrivilegeAudit {
    let mut rules: Vec<PrivilegeRule> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();

    for path in candidate_files(root) {
        if !path.exists() {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                unreadable.push(path.display().to_string());
                continue;
            }
        };
        let is_doas = path.file_name().and_then(|n| n.to_str()) == Some("doas.conf");
        for (idx, raw) in text.lines().enumerate() {
            let parsed = if is_doas {
                parse_doas_line(raw)
            } else {
                parse_sudoers_line(raw)
            };
            if let Some((principal, is_group, nopasswd, commands)) = parsed {
                rules.push(PrivilegeRule {
                    source: path.display().to_string(),
                    line: idx + 1,
                    system: if is_doas { "doas" } else { "sudoers" }.to_string(),
                    principal,
                    is_group,
                    nopasswd,
                    commands,
                    raw: raw.trim().to_string(),
                });
            }
        }
    }

    let mut findings: Vec<PrivilegeFinding> = Vec::new();

    // 1. Dangling targets — a NOPASSWD rule on a path that does not exist is a
    //    standing escalation for anyone who can create it.
    for r in &rules {
        for cmd in &r.commands {
            if is_dangling(root, cmd) {
                findings.push(PrivilegeFinding {
                    kind: "dangling_target".into(),
                    severity: if r.nopasswd { "high" } else { "medium" }.into(),
                    principal: r.principal.clone(),
                    source: Some(r.source.clone()),
                    line: Some(r.line),
                    detail: format!(
                        "grants {} on `{}`, which does not exist — anything able to create \
                         that path gains this privilege",
                        if r.nopasswd {
                            "passwordless root"
                        } else {
                            "root"
                        },
                        cmd
                    ),
                });
            }
        }
    }

    // 2. Sole grants — removing the only rule for a principal that has no group
    //    grant locks them out. This is the lockout tripwire.
    let mut per_principal: BTreeMap<&str, Vec<&PrivilegeRule>> = BTreeMap::new();
    for r in &rules {
        per_principal
            .entry(r.principal.as_str())
            .or_default()
            .push(r);
    }
    let has_group_rule = rules.iter().any(|r| r.is_group);
    for (principal, prules) in &per_principal {
        // `root` is privileged by definition — a "removing this locks them out"
        // finding for root is pure noise, so it is never a sole-grant subject.
        if *principal == "root" {
            continue;
        }
        if prules.len() == 1 && !prules[0].is_group {
            let r = prules[0];
            findings.push(PrivilegeFinding {
                kind: "sole_grant".into(),
                severity: "medium".into(),
                principal: (*principal).to_string(),
                source: Some(r.source.clone()),
                line: Some(r.line),
                detail: format!(
                    "this is the ONLY rule granting `{principal}` privilege on this host — \
                     removing {} would lock them out",
                    r.source
                ),
            });
        }
    }

    // 3. Unrestricted passwordless root, and 4. whether any group grant exists at
    //    all (its absence is why sole grants are so load-bearing here).
    for r in &rules {
        if r.nopasswd && r.commands.iter().any(|c| c == "ALL") {
            findings.push(PrivilegeFinding {
                kind: "nopasswd_all".into(),
                severity: "info".into(),
                principal: r.principal.clone(),
                source: Some(r.source.clone()),
                line: Some(r.line),
                detail: format!("`{}` has unrestricted passwordless root", r.principal),
            });
        }
    }
    if !has_group_rule && !rules.is_empty() {
        findings.push(PrivilegeFinding {
            kind: "no_group_grant".into(),
            severity: "info".into(),
            principal: "-".into(),
            source: None,
            line: None,
            detail: "no group-based rule (%sudo/%wheel/:wheel) grants privilege here, so every \
                     grant is per-user and individually load-bearing"
                .into(),
        });
    }

    let rank = |s: &str| match s {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    };
    findings.sort_by_key(|f| rank(&f.severity));

    PrivilegeAudit {
        host,
        rules,
        findings,
        complete: unreadable.is_empty(),
        unreadable,
    }
}

/// Report every sudo/doas grant on the host this runs on, flagging dangling
/// targets and sole grants. Pure filesystem read — mutates nothing, which is
/// why `audit` is in the derive's read-shaped verb list rather than this tool
/// carrying a `[MUTATES STATE]` marker it would be lying about.
#[orca_tool(domain = "identity.privilege", verb = "audit")]
async fn identity_privilege_audit(
    args: IdentityPrivilegeAuditArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PrivilegeAudit> {
    let root = PathBuf::from(args.root.unwrap_or_else(|| "/".to_string()));
    Ok(audit_root(
        &root,
        crate::host_identity::hostname().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_nopasswd_all_rule() {
        let (p, g, np, cmds) = parse_sudoers_line("skey ALL=(ALL) NOPASSWD: ALL").unwrap();
        assert_eq!(p, "skey");
        assert!(!g);
        assert!(np);
        assert_eq!(cmds, vec!["ALL"]);
    }

    #[test]
    fn parses_a_scoped_command_rule() {
        let (p, _, np, cmds) = parse_sudoers_line("svc ALL=(ALL) NOPASSWD: /usr/sbin/pct").unwrap();
        assert_eq!(p, "svc");
        assert!(np);
        assert_eq!(cmds, vec!["/usr/sbin/pct"]);
    }

    #[test]
    fn recognises_group_rules_and_skips_non_grants() {
        let (p, g, _, _) = parse_sudoers_line("%sudo\tALL=(ALL:ALL) ALL").unwrap();
        assert_eq!(p, "%sudo");
        assert!(g);
        assert!(parse_sudoers_line("Defaults:svc !requiretty").is_none());
        assert!(parse_sudoers_line("# a comment").is_none());
        assert!(parse_sudoers_line("").is_none());
        assert!(parse_sudoers_line("User_Alias ADMINS = skey").is_none());
    }

    #[test]
    fn parses_doas_permit_and_ignores_deny() {
        let (p, _, np, cmds) =
            parse_doas_line("permit nopass svc cmd /usr/local/bin/halvor-agent.sh").unwrap();
        assert_eq!(p, "svc");
        assert!(np);
        assert_eq!(cmds, vec!["/usr/local/bin/halvor-agent.sh"]);
        assert!(parse_doas_line("deny svc").is_none());
    }

    /// The exact shape found on freyr/baldur/thor/frigg on 2026-10-01: a retired
    /// file holding the operator's ONLY grant plus dangling `svc` rules.
    #[test]
    fn reproduces_the_halvor_finding() {
        let tmp = std::env::temp_dir().join(format!("orca-priv-audit-{}", std::process::id()));
        let sudoers_d = tmp.join("etc/sudoers.d");
        std::fs::create_dir_all(&sudoers_d).unwrap();
        std::fs::create_dir_all(tmp.join("usr/sbin")).unwrap();
        std::fs::write(tmp.join("usr/sbin/pct"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            sudoers_d.join("halvor"),
            "Defaults:svc !requiretty\n\
             skey ALL=(ALL) NOPASSWD: ALL\n\
             svc ALL=(ALL) NOPASSWD: /usr/sbin/pct\n\
             svc ALL=(ALL) NOPASSWD: /usr/local/bin/nfs-monitor.sh\n\
             svc ALL=(ALL) NOPASSWD: /usr/local/bin/willow-nfs-release.sh\n",
        )
        .unwrap();

        let audit = audit_root(&tmp, "fixture".into());

        // 4 grants parsed; the `Defaults:` line is not one.
        assert_eq!(audit.rules.len(), 4);

        // Both absent paths flagged high; the present `pct` is not flagged.
        let dangling: Vec<_> = audit
            .findings
            .iter()
            .filter(|f| f.kind == "dangling_target")
            .collect();
        assert_eq!(dangling.len(), 2, "expected both absent paths flagged");
        assert!(dangling.iter().all(|f| f.severity == "high"));
        assert!(dangling.iter().all(|f| f.principal == "svc"));
        assert!(
            !audit
                .findings
                .iter()
                .any(|f| { f.kind == "dangling_target" && f.detail.contains("/usr/sbin/pct") }),
            "an existing target must not be reported as dangling"
        );

        // skey's single rule is the lockout tripwire; svc has 3 rules, so it is not.
        let sole: Vec<_> = audit
            .findings
            .iter()
            .filter(|f| f.kind == "sole_grant")
            .collect();
        assert_eq!(sole.len(), 1);
        assert_eq!(sole[0].principal, "skey");

        // No %sudo/%wheel rule here, which is what made skey's rule load-bearing.
        assert!(audit.findings.iter().any(|f| f.kind == "no_group_grant"));
        assert!(audit.complete);

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// `root ALL=(ALL:ALL) ALL` in /etc/sudoers must not produce a sole-grant
    /// finding — root cannot be locked out of its own privilege. Caught when the
    /// first run against thor's real sudoers reported it as noise.
    #[test]
    fn root_is_never_a_sole_grant_finding() {
        let tmp = std::env::temp_dir().join(format!("orca-priv-root-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("etc")).unwrap();
        std::fs::write(
            tmp.join("etc/sudoers"),
            "root ALL=(ALL:ALL) ALL\nskey ALL=(ALL) NOPASSWD: ALL\n",
        )
        .unwrap();

        let audit = audit_root(&tmp, "fixture".into());
        let sole: Vec<_> = audit
            .findings
            .iter()
            .filter(|f| f.kind == "sole_grant")
            .map(|f| f.principal.as_str())
            .collect();
        assert_eq!(
            sole,
            vec!["skey"],
            "root must be excluded, skey must not be"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn globs_and_relative_commands_are_not_dangling() {
        let root = Path::new("/nonexistent-root-for-test");
        assert!(!is_dangling(root, "ALL"));
        assert!(!is_dangling(root, "/usr/bin/apt-*"));
        assert!(is_dangling(root, "/definitely/not/here"));
    }
}
