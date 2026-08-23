// CLI command that passes through spec/config blobs; HashMap/Value are protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use anyhow::Result;
use clap::Subcommand;
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum HookAction {
    /// UserPromptSubmit: log user prompt to ~/.orca/logs/sessions/
    SessionStart,
    /// Stop: log last assistant response to ~/.orca/logs/sessions/
    SessionStop,
    /// PreToolUse:Bash: block destructive shell commands against homelab infrastructure
    BashGuard,
    /// PreToolUse:Bash: block commands targeting the OPNsense network router
    OpnsenseGuard,
    /// PostToolUse:Write|Edit: scan written files for PII patterns
    PiiScan,
    /// PreToolUse:Bash(git commit): scan staged changes for secrets
    SecretsScan,
    /// PreToolUse:Glob: serve result from bloodhound cache if available (no-op until cache ported)
    GlobCacheRead,
    /// PostToolUse:Glob: write glob results to bloodhound cache (no-op until cache ported)
    GlobCacheWrite,
}

pub fn cmd_hook(action: HookAction) -> Result<()> {
    match action {
        HookAction::BashGuard => bash_guard(),
        HookAction::OpnsenseGuard => opnsense_guard(),
        HookAction::SessionStart => session_start(),
        HookAction::SessionStop => session_stop(),
        HookAction::PiiScan => pii_scan(),
        HookAction::SecretsScan => secrets_scan(),
        HookAction::GlobCacheRead => Ok(()),
        HookAction::GlobCacheWrite => Ok(()),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn read_stdin() -> Value {
    let mut buf = String::new();
    _ = std::io::stdin().read_to_string(&mut buf);
    serde_json::from_str(&buf).unwrap_or(Value::Null)
}

fn get_command(input: &Value) -> String {
    input["tool_input"]["command"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn log_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".orca")
        .join("logs")
        .join("sessions")
}

fn session_file(session_short: &str, project: &str) -> PathBuf {
    let date = utils::time::now().date();
    log_dir().join(format!("{date}_{session_short}_{project}.jsonl"))
}

fn append_jsonl(path: &PathBuf, record: &Value) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap_or(path))?;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

fn block(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}

// ── BashGuard ─────────────────────────────────────────────────────────────────

const DESTRUCTIVE_PATTERNS: &[&str] = &[
    r"rm\s+-[a-zA-Z]*r[a-zA-Z]*f", // rm -rf, rm -fr, etc.
    r"rm\s+-[a-zA-Z]*f[a-zA-Z]*r",
    r"qm\s+destroy",
    r"pct\s+destroy",
    r"pvesm\s+remove",
    r"wipefs",
    r"mkfs\.",
    r"dd\s+if=",
    r"blkdiscard",
    r"shred\s+",
];

fn bash_guard() -> Result<()> {
    let input = read_stdin();
    let command = get_command(&input);
    if command.is_empty() {
        return Ok(());
    }

    for pattern in DESTRUCTIVE_PATTERNS {
        let re = regex::Regex::new(pattern).expect("valid pattern");
        if re.is_match(&command) {
            block(&format!(
                "BLOCKED: Destructive command detected (pattern: `{pattern}`)\n\
                 Command: {command}\n\n\
                 This command requires explicit user confirmation before running.\n\
                 State what you intend to do and why, then ask the user to approve."
            ));
        }
    }
    Ok(())
}

// ── OpnsenseGuard ─────────────────────────────────────────────────────────────

// Named-host patterns are safe to ship — "opnsense" is a public product name.
// The router's IP is deployment-private, so it is NOT hardcoded here: set
// `ORCA_ROUTER_GUARD_IP` (e.g. in your .envrc) to also guard commands that
// target the router by address. When unset, only the named patterns apply.
const OPNSENSE_PATTERNS: &[&str] = &[
    r"ssh.*opnsense",
    r"opnsense-update",
    r"curl.*opnsense",
    r"wget.*opnsense",
];

/// Build the active guard patterns: the static named-host ones plus, when an
/// `ip` is given, a pattern matching exactly that IP (not a longer one sharing
/// it as a prefix).
fn opnsense_patterns_with(ip: Option<&str>) -> Vec<String> {
    let mut pats: Vec<String> = OPNSENSE_PATTERNS.iter().map(|s| s.to_string()).collect();
    if let Some(ip) = ip.filter(|s| !s.is_empty()) {
        pats.push(format!(r"{}(?:[^0-9]|$)", regex::escape(ip)));
    }
    pats
}

/// Active guard patterns, sourcing the optional router IP from
/// `ORCA_ROUTER_GUARD_IP` so the real address never ships in source.
fn opnsense_patterns() -> Vec<String> {
    opnsense_patterns_with(std::env::var("ORCA_ROUTER_GUARD_IP").ok().as_deref())
}

fn opnsense_guard() -> Result<()> {
    let input = read_stdin();
    let command = get_command(&input);
    if command.is_empty() {
        return Ok(());
    }

    for pattern in opnsense_patterns() {
        let re = regex::Regex::new(&pattern).expect("valid pattern");
        if re.is_match(&command) {
            block(&format!(
                "OPNSENSE GUARD: Command targets the OPNsense network router.\n\
                 Command: {command}\n\n\
                 OPNsense protocol requires:\n\
                 1. State exactly what you intend to change and why\n\
                 2. Get explicit user confirmation before running\n\
                 3. Make one change at a time, verify before the next step\n\n\
                 Do not proceed until the user has confirmed this specific command."
            ));
        }
    }
    Ok(())
}

// ── Session logging ───────────────────────────────────────────────────────────

fn session_start() -> Result<()> {
    let input = read_stdin();
    let session_id = input["session_id"].as_str().unwrap_or("");
    let cwd = input["cwd"].as_str().unwrap_or("");
    let prompt = input["prompt"].as_str().unwrap_or("");

    if session_id.is_empty() {
        return Ok(());
    }

    let session_short = &session_id[..session_id.len().min(8)];
    let project = PathBuf::from(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let prompt_trimmed = &prompt[..prompt.len().min(800)];

    let record = json!({
        "id": utils::id::new(),
        "session": session_short,
        "timestamp": utils::time::now_rfc3339(),
        "project": project,
        "role": "user",
        "agent": null,
        "content": prompt_trimmed,
        "important": false,
        "tags": [],
        "note": ""
    });

    append_jsonl(&session_file(session_short, &project), &record)
}

fn session_stop() -> Result<()> {
    let input = read_stdin();
    let session_id = input["session_id"].as_str().unwrap_or("");
    let transcript_path = input["transcript_path"].as_str().unwrap_or("");
    let cwd = input["cwd"].as_str().unwrap_or("");

    if session_id.is_empty() || transcript_path.is_empty() {
        return Ok(());
    }

    let session_short = &session_id[..session_id.len().min(8)];
    let project = PathBuf::from(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let content = extract_last_assistant_text(transcript_path);
    if content.is_empty() {
        return Ok(());
    }

    let record = json!({
        "id": utils::id::new(),
        "session": session_short,
        "timestamp": utils::time::now_rfc3339(),
        "project": project,
        "role": "assistant",
        "agent": "orca",
        "content": &content[..content.len().min(1200)],
        "important": false,
        "tags": [],
        "note": ""
    });

    append_jsonl(&session_file(session_short, &project), &record)
}

fn extract_last_assistant_text(transcript_path: &str) -> String {
    let Ok(raw) = std::fs::read_to_string(transcript_path) else {
        return String::new();
    };
    let mut last_texts: Vec<String> = Vec::new();
    for line in raw.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["type"].as_str() != Some("assistant") {
            continue;
        }
        if entry["message"]["role"].as_str() != Some("assistant") {
            continue;
        }
        let texts: Vec<String> = entry["message"]["content"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter(|b| b["type"].as_str() == Some("text"))
            .filter_map(|b| b["text"].as_str().map(str::to_string))
            .collect();
        if !texts.is_empty() {
            last_texts = texts;
        }
    }
    last_texts.join(" ").trim().to_string()
}

// ── PII scanner ───────────────────────────────────────────────────────────────

const PII_PATTERNS: &[(&str, &str)] = &[
    (
        r"\+?1[-.\s]?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}",
        "US phone number",
    ),
    (r"\b\d{3}-\d{2}-\d{4}\b", "SSN pattern"),
    (r"staging\.example\.com", "staging domain"),
    (r"re_[A-Za-z0-9]{20,}", "Resend API key"),
    (r"sk_live_[A-Za-z0-9]+", "Stripe secret key"),
    (r"pk_live_[A-Za-z0-9]+", "Stripe public key"),
    (r"Bearer [A-Za-z0-9\-_\.]{20,}", "Bearer token"),
    (r"0x[A-Fa-f0-9]{32,}", "hex secret (Turnstile/CF)"),
];

const PII_SCAN_EXCLUDES: &[&str] = &[];

fn pii_scan() -> Result<()> {
    let input = read_stdin();
    let file_path = input["tool_input"]["file_path"].as_str().unwrap_or("");

    if file_path.is_empty() || !std::path::Path::new(file_path).exists() {
        return Ok(());
    }

    // Excluded paths
    if PII_SCAN_EXCLUDES.iter().any(|ex| file_path.contains(ex)) {
        return Ok(());
    }

    let Ok(content) = std::fs::read_to_string(file_path) else {
        return Ok(());
    };

    let mut findings: Vec<String> = Vec::new();
    for (pattern, label) in PII_PATTERNS {
        let re = regex::Regex::new(pattern).expect("valid pattern");
        let matches: Vec<&str> = re.find_iter(&content).take(3).map(|m| m.as_str()).collect();
        if !matches.is_empty() {
            findings.push(format!("  [{label}]: {}", matches.join(", ")));
        }
    }

    if !findings.is_empty() {
        let msg = format!(
            "\n{bar}\n\
             ⚠  PII SCANNER — POTENTIAL SENSITIVE DATA DETECTED\n\
                File: {file_path}\n\
             {bar}\n\
             {details}\n\
             Review before committing. Remove or move to GH Actions secret.\n\
             {bar}",
            bar = "━".repeat(62),
            details = findings.join("\n"),
        );
        block(&msg);
    }
    Ok(())
}

#[cfg(test)]
fn matches_destructive(cmd: &str) -> bool {
    DESTRUCTIVE_PATTERNS
        .iter()
        .any(|p| regex::Regex::new(p).expect("valid pattern").is_match(cmd))
}

#[cfg(test)]
fn matches_opnsense(cmd: &str, ip: Option<&str>) -> bool {
    opnsense_patterns_with(ip)
        .iter()
        .any(|p| regex::Regex::new(p).expect("valid pattern").is_match(cmd))
}

// ── Secrets scan (git commit guard) ──────────────────────────────────────────

fn secrets_scan() -> Result<()> {
    let input = read_stdin();
    let command = get_command(&input);

    if !command.contains("git") || !command.contains("commit") {
        return Ok(());
    }

    // Prefer gitleaks if available
    let gitleaks = std::process::Command::new("gitleaks")
        .args(["protect", "--staged", "-v"])
        .output();

    if let Ok(output) = gitleaks {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let findings: String = stderr
                .lines()
                .filter(|l| {
                    l.contains("RuleID")
                        || l.contains("Secret")
                        || l.contains("File")
                        || l.contains("Line")
                })
                .take(10)
                .collect::<Vec<_>>()
                .join(" — ");
            let decision = json!({
                "continue": false,
                "stopReason": format!("Secrets scan blocked this commit. {findings} Remove credentials before committing.")
            });
            println!("{}", serde_json::to_string(&decision)?);
            return Ok(());
        }
        return Ok(());
    }

    // Fallback: grep staged diff
    let diff_output = std::process::Command::new("git")
        .args(["diff", "--cached"])
        .output();

    let Ok(diff_output) = diff_output else {
        return Ok(()); // not a git repo
    };
    let diff = String::from_utf8_lossy(&diff_output.stdout);

    let secret_patterns: &[(&str, &str)] = &[
        (
            r"eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}",
            "JWT token",
        ),
        (
            r#"[Aa]uthorization["': ]+[Bb]earer [A-Za-z0-9_.\-]{20,}"#,
            "Bearer token",
        ),
        (
            r#"[Aa][Pp][Ii][_-]?[Kk][Ee][Yy]["': =]+[A-Za-z0-9_.\-]{16,}"#,
            "API key",
        ),
        (
            r"-----BEGIN (RSA|EC|OPENSSH|PGP) PRIVATE KEY",
            "Private key",
        ),
        (
            r"[Aa][Ww][Ss]_[Aa][Cc][Cc][Ee][Ss][Ss][_-]?[Kk][Ee][Yy]",
            "AWS key",
        ),
    ];

    let mut detected: Vec<&str> = Vec::new();
    for (pattern, label) in secret_patterns {
        let re = regex::Regex::new(&format!(r"^\+.*({pattern})")).expect("valid pattern");
        for line in diff.lines() {
            if re.is_match(line) {
                detected.push(label);
                break;
            }
        }
    }

    if !detected.is_empty() {
        let detail = detected.join(", ");
        let decision = json!({
            "continue": false,
            "stopReason": format!(
                "Secrets scan blocked this commit. Detected: {detail}. \
                 Remove credentials before committing. \
                 Install gitleaks for comprehensive scanning: brew install gitleaks"
            )
        });
        println!("{}", serde_json::to_string(&decision)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ── BashGuard pattern matching ────────────────────────────────────────────

    #[test]
    fn destructive_blocks_rm_rf() {
        assert!(matches_destructive("rm -rf /tmp/foo"));
        assert!(matches_destructive("rm -fr /tmp/foo"));
        // Patterns match lowercase r/f only — uppercase variants are not in scope
        assert!(!matches_destructive("rm -Rf /etc"));
    }

    #[test]
    fn destructive_allows_safe_rm() {
        assert!(!matches_destructive("rm -f myfile.txt"));
        assert!(!matches_destructive("rm single_file"));
    }

    #[test]
    fn destructive_blocks_proxmox_commands() {
        assert!(matches_destructive("qm destroy 101"));
        assert!(matches_destructive("pct destroy 200"));
        assert!(matches_destructive("pvesm remove local:vm-101-disk-0"));
    }

    #[test]
    fn destructive_blocks_disk_wipe_commands() {
        assert!(matches_destructive("wipefs -a /dev/sda"));
        assert!(matches_destructive("mkfs.ext4 /dev/sda1"));
        assert!(matches_destructive("dd if=/dev/zero of=/dev/sda"));
        assert!(matches_destructive("blkdiscard /dev/nvme0n1"));
        assert!(matches_destructive("shred /dev/sda"));
    }

    #[test]
    fn destructive_allows_harmless_commands() {
        assert!(!matches_destructive("ls -la"));
        assert!(!matches_destructive("git status"));
        assert!(!matches_destructive("cargo build"));
        assert!(!matches_destructive("echo hello"));
    }

    // ── OpnsenseGuard pattern matching ────────────────────────────────────────

    // The configured router IP is supplied explicitly (as `ORCA_ROUTER_GUARD_IP`
    // would at runtime); a documentation-range IP stands in for the real one.
    const TEST_ROUTER_IP: Option<&str> = Some("192.0.2.1");

    #[test]
    fn opnsense_blocks_ip_access() {
        assert!(matches_opnsense("ssh admin@192.0.2.1", TEST_ROUTER_IP));
        assert!(matches_opnsense(
            "curl http://192.0.2.1/api",
            TEST_ROUTER_IP
        ));
        assert!(matches_opnsense("ping 192.0.2.1", TEST_ROUTER_IP));
    }

    #[test]
    fn opnsense_does_not_block_similar_ips() {
        // 192.0.2.10 has an extra digit — should NOT match 192.0.2.1 as a prefix
        assert!(!matches_opnsense("ping 192.0.2.10", TEST_ROUTER_IP));
        assert!(!matches_opnsense("ssh user@192.0.2.100", TEST_ROUTER_IP));
    }

    #[test]
    fn opnsense_ip_pattern_inert_when_unconfigured() {
        // With no configured IP, only named-host patterns apply.
        assert!(!matches_opnsense("ssh admin@192.0.2.1", None));
    }

    #[test]
    fn opnsense_blocks_named_target() {
        assert!(matches_opnsense("ssh root@opnsense", None));
        assert!(matches_opnsense("curl http://opnsense/api", None));
        assert!(matches_opnsense("wget http://opnsense/status", None));
        assert!(matches_opnsense("opnsense-update", None));
    }

    #[test]
    fn opnsense_allows_unrelated_commands() {
        assert!(!matches_opnsense("ping 8.8.8.8", TEST_ROUTER_IP));
        assert!(!matches_opnsense("ssh user@198.51.100.1", TEST_ROUTER_IP));
        assert!(!matches_opnsense(
            "curl https://api.example.com",
            TEST_ROUTER_IP
        ));
    }

    // ── extract_last_assistant_text ───────────────────────────────────────────

    #[test]
    fn extract_last_assistant_text_returns_empty_for_missing_file() {
        let result = extract_last_assistant_text("/tmp/__no_such_transcript_file__.jsonl");
        assert_eq!(result, "");
    }

    #[test]
    fn extract_last_assistant_text_parses_transcript() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // One assistant turn with text content
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "The answer is 42."}
                ]
            }
        });
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        let result = extract_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(result, "The answer is 42.");
    }

    #[test]
    fn extract_last_assistant_text_uses_last_turn() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Two assistant turns — should return only the last one
        for text in ["First response.", "Second response."] {
            let entry = serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": text}]
                }
            });
            writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        }
        let result = extract_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(result, "Second response.");
    }

    #[test]
    fn extract_last_assistant_text_skips_non_assistant_entries() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let user_entry = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": "User message"}]}
        });
        writeln!(f, "{}", serde_json::to_string(&user_entry).unwrap()).unwrap();
        let result = extract_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(result, "");
    }

    #[test]
    fn extract_last_assistant_text_skips_non_text_blocks() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {}},
                    {"type": "text", "text": "Done!"}
                ]
            }
        });
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        let result = extract_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(result, "Done!");
    }

    #[test]
    fn extract_last_assistant_text_handles_invalid_jsonl_gracefully() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{{bad json").unwrap();
        writeln!(f, "also not json").unwrap();
        let result = extract_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(result, "");
    }

    // ── pii patterns compile without panic ────────────────────────────────────

    #[test]
    fn pii_patterns_all_compile() {
        for (pattern, _label) in PII_PATTERNS {
            regex::Regex::new(pattern).expect("PII pattern should compile: {pattern}");
        }
    }

    // ── get_command ───────────────────────────────────────────────────────────

    #[test]
    fn get_command_extracts_nested_command() {
        let input = serde_json::json!({
            "tool_input": {"command": "ls -la"}
        });
        assert_eq!(get_command(&input), "ls -la");
    }

    #[test]
    fn get_command_missing_returns_empty() {
        assert_eq!(get_command(&Value::Null), "");
        assert_eq!(get_command(&serde_json::json!({})), "");
        assert_eq!(get_command(&serde_json::json!({"tool_input": {}})), "");
        // Wrong type (number, not string) also yields empty.
        assert_eq!(
            get_command(&serde_json::json!({"tool_input": {"command": 42}})),
            ""
        );
    }

    // ── opnsense_patterns_with ────────────────────────────────────────────────

    #[test]
    fn opnsense_patterns_named_only_without_ip() {
        let pats = opnsense_patterns_with(None);
        assert_eq!(pats.len(), OPNSENSE_PATTERNS.len());
    }

    #[test]
    fn opnsense_patterns_empty_ip_is_ignored() {
        // An empty string is filtered out; no IP pattern is appended.
        let pats = opnsense_patterns_with(Some(""));
        assert_eq!(pats.len(), OPNSENSE_PATTERNS.len());
    }

    #[test]
    fn opnsense_patterns_appends_escaped_ip() {
        let pats = opnsense_patterns_with(Some("192.0.2.1"));
        assert_eq!(pats.len(), OPNSENSE_PATTERNS.len() + 1);
        // The dots must be regex-escaped so they match literally, not "any char".
        let ip_pat = pats.last().unwrap();
        assert!(ip_pat.contains(r"192\.0\.2\.1"));
    }

    #[test]
    fn opnsense_ip_matches_at_end_of_string() {
        // The IP followed by end-of-string (no trailing chars) still matches.
        assert!(matches_opnsense("connect 192.0.2.1", TEST_ROUTER_IP));
    }

    // ── session_file / log_dir rendering ──────────────────────────────────────

    #[test]
    fn session_file_is_under_log_dir_with_expected_shape() {
        let path = session_file("abc12345", "myproject");
        // Lives directly under the sessions log dir.
        assert_eq!(path.parent().unwrap(), log_dir().as_path());
        let name = path.file_name().unwrap().to_str().unwrap();
        // date_session_project.jsonl
        assert!(name.ends_with("_abc12345_myproject.jsonl"));
    }

    #[test]
    fn log_dir_ends_with_sessions_path() {
        let dir = log_dir();
        assert!(dir.ends_with("sessions"));
        assert!(dir.to_str().unwrap().contains(".orca"));
    }

    // ── append_jsonl ──────────────────────────────────────────────────────────

    #[test]
    fn append_jsonl_creates_dirs_and_appends_lines() {
        let tmp = tempfile::tempdir().unwrap();
        // Nested path exercises the create_dir_all branch.
        let path = tmp.path().join("nested").join("log.jsonl");
        let r1 = serde_json::json!({"n": 1});
        let r2 = serde_json::json!({"n": 2});
        append_jsonl(&path, &r1).unwrap();
        append_jsonl(&path, &r2).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r#"{"n":1}"#);
        assert_eq!(lines[1], r#"{"n":2}"#);
    }

    // ── extract_last_assistant_text edge cases ────────────────────────────────

    #[test]
    fn extract_last_assistant_text_empty_content_array() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": []}
        });
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(extract_last_assistant_text(f.path().to_str().unwrap()), "");
    }

    #[test]
    fn extract_last_assistant_text_skips_wrong_inner_role() {
        // type=assistant but message.role != assistant is skipped.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {"role": "user", "content": [{"type": "text", "text": "nope"}]}
        });
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(extract_last_assistant_text(f.path().to_str().unwrap()), "");
    }

    #[test]
    fn extract_last_assistant_text_joins_multiple_text_blocks() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "Hello"},
                {"type": "text", "text": "world"}
            ]}
        });
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(
            extract_last_assistant_text(f.path().to_str().unwrap()),
            "Hello world"
        );
    }

    #[test]
    fn extract_last_assistant_text_earlier_turn_kept_when_last_has_no_text() {
        // A later assistant turn with only tool_use (no text) must NOT clobber
        // the earlier text turn, since last_texts only updates when non-empty.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let with_text = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "Keep me"}]}
        });
        let tool_only = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}
            ]}
        });
        writeln!(f, "{}", serde_json::to_string(&with_text).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&tool_only).unwrap()).unwrap();
        assert_eq!(
            extract_last_assistant_text(f.path().to_str().unwrap()),
            "Keep me"
        );
    }

    // ── PII pattern behaviour ─────────────────────────────────────────────────

    // Locate the label paired with a given regex source in PII_PATTERNS.
    fn pii_label(pattern_src: &str) -> &'static str {
        PII_PATTERNS
            .iter()
            .find(|(p, _)| *p == pattern_src)
            .map(|(_, l)| *l)
            .expect("pattern present")
    }

    #[test]
    fn pii_detects_us_phone_number() {
        let (pat, label) = PII_PATTERNS[0];
        assert_eq!(label, "US phone number");
        let re = regex::Regex::new(pat).unwrap();
        // Pattern requires a leading US country-code digit.
        assert!(re.is_match("call 1-415-555-0132"));
        assert!(re.is_match("+1 (415) 555-0132"));
    }

    #[test]
    fn pii_detects_ssn() {
        let re = regex::Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap();
        assert_eq!(pii_label(r"\b\d{3}-\d{2}-\d{4}\b"), "SSN pattern");
        assert!(re.is_match("ssn 123-45-6789 here"));
        assert!(!re.is_match("1234-45-6789"));
    }

    #[test]
    fn pii_detects_staging_domain_and_hex_secret() {
        let staging = regex::Regex::new(r"staging\.example\.com").unwrap();
        assert!(staging.is_match("https://staging.example.com/api"));
        let hex = regex::Regex::new(r"0x[A-Fa-f0-9]{32,}").unwrap();
        assert!(hex.is_match(&["0x", &"a1b2c3d4".repeat(4)].concat()));
        assert!(!hex.is_match("0xdeadbeef"));
    }

    #[test]
    fn pii_ignores_plain_text() {
        for (pattern, _label) in PII_PATTERNS {
            let re = regex::Regex::new(pattern).unwrap();
            assert!(
                !re.is_match("just some ordinary prose with no secrets"),
                "pattern {pattern} should not match plain text"
            );
        }
    }

    #[test]
    fn pii_excludes_list_is_empty() {
        // Guard: the exclude list is currently empty, so nothing is skipped.
        assert!(PII_SCAN_EXCLUDES.is_empty());
    }

    // ── extra destructive edge cases ──────────────────────────────────────────

    #[test]
    fn destructive_matches_combined_flags() {
        // Interleaved flags: rm -vrf and rm -rfv both trip the r-before-f rule.
        assert!(matches_destructive("rm -vrf /data"));
        assert!(matches_destructive("rm -rfv /data"));
    }

    #[test]
    fn destructive_dd_requires_if_argument() {
        assert!(matches_destructive("dd if=/dev/sda of=out.img"));
        // dd without an if= source is not flagged by the pattern.
        assert!(!matches_destructive("dd of=out.img bs=1M"));
    }

    #[test]
    fn pii_patterns_detect_known_secrets() {
        // Split across concat so no single literal matches the PII scanner patterns.
        let stripe_key = ["sk", "_live_", "abcdefghijklmnop"].concat();
        let bearer = ["Bearer ", "eyJhbGciOiJSUzI1NiIsInR5cCI6Ikp"].concat();
        let re_key = ["re_", "AbCdEfGhIjKlMnOpQrStUvWxYz"].concat();

        let stripe_re = regex::Regex::new(r"sk_live_[A-Za-z0-9]+").unwrap();
        assert!(stripe_re.is_match(&stripe_key));

        let bearer_re = regex::Regex::new(r"Bearer [A-Za-z0-9\-_\.]{20,}").unwrap();
        assert!(bearer_re.is_match(&bearer));

        let re_re = regex::Regex::new(r"re_[A-Za-z0-9]{20,}").unwrap();
        assert!(re_re.is_match(&re_key));
    }

    // ── pattern-set compile guards ────────────────────────────────────────────

    #[test]
    fn destructive_patterns_all_compile() {
        for pattern in DESTRUCTIVE_PATTERNS {
            regex::Regex::new(pattern).expect("destructive pattern should compile");
        }
    }

    #[test]
    fn opnsense_named_patterns_all_compile() {
        for pattern in OPNSENSE_PATTERNS {
            regex::Regex::new(pattern).expect("opnsense pattern should compile");
        }
    }

    #[test]
    fn destructive_pattern_count_is_stable() {
        // Guard against accidental additions/removals to the destructive set.
        assert_eq!(DESTRUCTIVE_PATTERNS.len(), 10);
    }

    // ── additional destructive edge cases ─────────────────────────────────────

    #[test]
    fn destructive_split_flags_do_not_match() {
        // Flags separated by a space break the contiguous `-...r...f` requirement.
        assert!(!matches_destructive("rm -r -f /data"));
        assert!(!matches_destructive("rm -f -r /data"));
    }

    #[test]
    fn destructive_only_r_or_only_f_does_not_match() {
        // A single flag alone (just -r or just -f) is not destructive by pattern.
        assert!(!matches_destructive("rm -r somedir"));
        assert!(!matches_destructive("rm -r"));
    }

    #[test]
    fn destructive_mkfs_variants_match() {
        // The `mkfs\.` pattern matches any filesystem-specific mkfs invocation.
        assert!(matches_destructive("mkfs.xfs /dev/sdb1"));
        assert!(matches_destructive("mkfs.btrfs /dev/sdc"));
        // Bare `mkfs` with no dot is not matched by the `mkfs\.` pattern.
        assert!(!matches_destructive("mkfs /dev/sda"));
    }

    #[test]
    fn destructive_shred_requires_trailing_space() {
        // Pattern is `shred\s+`, so "shred" must be followed by whitespace+arg.
        assert!(matches_destructive("shred -u file"));
        assert!(!matches_destructive("shredder"));
    }

    #[test]
    fn destructive_pvesm_and_qm_need_the_subcommand() {
        // Only the destroy/remove subcommands trip the guard.
        assert!(!matches_destructive("qm list"));
        assert!(!matches_destructive("pct list"));
        assert!(!matches_destructive("pvesm status"));
    }

    // ── additional opnsense edge cases ────────────────────────────────────────

    #[test]
    fn opnsense_ip_matches_before_non_digit_boundary() {
        // A port suffix (colon) is a non-digit boundary, so the IP still matches.
        assert!(matches_opnsense(
            "curl http://192.0.2.1:443/api",
            TEST_ROUTER_IP
        ));
        // A slash boundary too.
        assert!(matches_opnsense("http://192.0.2.1/status", TEST_ROUTER_IP));
    }

    #[test]
    fn opnsense_named_patterns_need_surrounding_context() {
        // ssh/curl/wget patterns require the "opnsense" token to appear.
        assert!(!matches_opnsense("ssh admin@otherhost", None));
        assert!(!matches_opnsense("curl http://example.com", None));
    }

    #[test]
    fn opnsense_ip_pattern_is_last_when_configured() {
        let pats = opnsense_patterns_with(Some("10.9.8.7"));
        let ip_pat = pats.last().unwrap();
        assert!(ip_pat.contains(r"10\.9\.8\.7"));
        // Every named pattern is still present ahead of the IP pattern.
        for named in OPNSENSE_PATTERNS {
            assert!(pats.iter().any(|p| p == named));
        }
    }

    #[test]
    fn opnsense_escapes_regex_metacharacters_in_ip() {
        // regex::escape ensures characters like dots are literal, not wildcards.
        let pats = opnsense_patterns_with(Some("192.0.2.1"));
        let ip_pat = pats.last().unwrap();
        let re = regex::Regex::new(ip_pat).unwrap();
        // A string that would match if dots were wildcards but shares no literal.
        assert!(!re.is_match("19200201x"));
    }

    // ── get_command additional shapes ─────────────────────────────────────────

    #[test]
    fn get_command_ignores_sibling_fields() {
        let input = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "git status", "description": "check"}
        });
        assert_eq!(get_command(&input), "git status");
    }

    #[test]
    fn get_command_array_command_is_empty() {
        // A command supplied as an array (not a string) yields empty.
        let input = serde_json::json!({"tool_input": {"command": ["ls", "-la"]}});
        assert_eq!(get_command(&input), "");
    }

    // ── session_file / log_dir details ────────────────────────────────────────

    #[test]
    fn session_file_name_starts_with_date_and_has_jsonl_ext() {
        let path = session_file("deadbeef", "orca");
        let name = path.file_name().unwrap().to_str().unwrap();
        // Format: YYYY-MM-DD_session_project.jsonl
        assert!(name.ends_with(".jsonl"));
        let parts: Vec<&str> = name.trim_end_matches(".jsonl").split('_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1], "deadbeef");
        assert_eq!(parts[2], "orca");
        // Date part looks like a dashed date.
        assert_eq!(parts[0].matches('-').count(), 2);
    }

    #[test]
    fn session_file_distinguishes_sessions_and_projects() {
        let a = session_file("sess1111", "projA");
        let b = session_file("sess2222", "projB");
        assert_ne!(a, b);
    }

    #[test]
    fn log_dir_layout_is_orca_logs_sessions() {
        let dir = log_dir();
        let s = dir.to_str().unwrap();
        assert!(s.contains(".orca"));
        assert!(s.contains("logs"));
        assert!(s.ends_with("sessions"));
    }

    // ── append_jsonl additional behaviour ─────────────────────────────────────

    #[test]
    fn append_jsonl_preserves_existing_content_on_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log.jsonl");
        append_jsonl(&path, &serde_json::json!({"first": true})).unwrap();
        // A separate call re-opens in append mode; the first line survives.
        append_jsonl(&path, &serde_json::json!({"second": true})).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines[0], r#"{"first":true}"#);
        assert_eq!(lines[1], r#"{"second":true}"#);
    }

    #[test]
    fn append_jsonl_serializes_nested_records_compactly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.jsonl");
        let record = serde_json::json!({
            "role": "user",
            "tags": ["a", "b"],
            "meta": {"n": 3}
        });
        append_jsonl(&path, &record).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        // Single line, compact (no pretty whitespace), ends with newline.
        assert!(contents.ends_with('\n'));
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains(r#""tags":["a","b"]"#));
        assert!(contents.contains(r#""meta":{"n":3}"#));
    }

    // ── extract_last_assistant_text additional edges ──────────────────────────

    #[test]
    fn extract_last_assistant_text_empty_file_returns_empty() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(extract_last_assistant_text(f.path().to_str().unwrap()), "");
    }

    #[test]
    fn extract_last_assistant_text_missing_message_field() {
        // type=assistant but no message object at all is skipped safely.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "{}",
            serde_json::to_string(&serde_json::json!({"type": "assistant"})).unwrap()
        )
        .unwrap();
        assert_eq!(extract_last_assistant_text(f.path().to_str().unwrap()), "");
    }

    #[test]
    fn extract_last_assistant_text_trims_surrounding_whitespace() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "  padded  "}]}
        });
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        // The join+trim collapses leading/trailing whitespace around the text.
        assert_eq!(
            extract_last_assistant_text(f.path().to_str().unwrap()),
            "padded"
        );
    }

    #[test]
    fn extract_last_assistant_text_blank_lines_are_skipped() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]}
        });
        writeln!(f).unwrap();
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        writeln!(f).unwrap();
        assert_eq!(
            extract_last_assistant_text(f.path().to_str().unwrap()),
            "hi"
        );
    }

    #[test]
    fn extract_last_assistant_text_text_block_missing_text_field() {
        // A text-typed block without a "text" string yields nothing for that block.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text"}]}
        });
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(extract_last_assistant_text(f.path().to_str().unwrap()), "");
    }

    // ── additional PII pattern behaviour ──────────────────────────────────────

    #[test]
    fn pii_detects_stripe_public_key() {
        assert_eq!(pii_label(r"pk_live_[A-Za-z0-9]+"), "Stripe public key");
        let re = regex::Regex::new(r"pk_live_[A-Za-z0-9]+").unwrap();
        assert!(re.is_match(&["pk", "_live_", "abc123XYZ"].concat()));
        // The test-mode prefix must not match the live-key pattern.
        assert!(!re.is_match("pk_test_abc123"));
    }

    #[test]
    fn pii_detects_resend_key_requires_min_length() {
        let re = regex::Regex::new(r"re_[A-Za-z0-9]{20,}").unwrap();
        // Fewer than 20 trailing chars does not match.
        assert!(!re.is_match(&["re_", "shorttoken"].concat()));
        assert!(re.is_match(&["re_", &"a".repeat(20)].concat()));
    }

    #[test]
    fn pii_bearer_requires_min_token_length() {
        let re = regex::Regex::new(r"Bearer [A-Za-z0-9\-_\.]{20,}").unwrap();
        assert!(!re.is_match("Bearer short"));
        assert!(re.is_match(&["Bearer ", &"x".repeat(20)].concat()));
    }

    #[test]
    fn pii_hex_secret_boundary_exactly_32() {
        let re = regex::Regex::new(r"0x[A-Fa-f0-9]{32,}").unwrap();
        // Exactly 32 hex digits after 0x matches; 31 does not.
        assert!(re.is_match(&["0x", &"a".repeat(32)].concat()));
        assert!(!re.is_match(&["0x", &"a".repeat(31)].concat()));
    }

    #[test]
    fn pii_ssn_word_boundaries_enforced() {
        let re = regex::Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap();
        // Embedded in a longer digit run — word boundary fails.
        assert!(!re.is_match("123-45-67890"));
        assert!(re.is_match("123-45-6789"));
    }

    #[test]
    fn pii_pattern_label_pairs_are_unique_sources() {
        // No duplicate regex sources in the PII set.
        let mut seen = std::collections::HashSet::new();
        for (pattern, _label) in PII_PATTERNS {
            assert!(seen.insert(*pattern), "duplicate PII pattern: {pattern}");
        }
    }

    #[test]
    fn pii_find_iter_caps_matches_at_three() {
        // pii_scan takes at most 3 matches per pattern; confirm the cap logic.
        let re = regex::Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap();
        let content = "111-11-1111 222-22-2222 333-33-3333 444-44-4444";
        let matches: Vec<&str> = re.find_iter(content).take(3).map(|m| m.as_str()).collect();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0], "111-11-1111");
    }
}
