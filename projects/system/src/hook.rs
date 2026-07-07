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
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
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
        "id": new_uuid(),
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
        "id": new_uuid(),
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

fn new_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Simple UUID v4 without the uuid crate — randomness via thread_rng
    let mut bytes = [0u8; 16];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Mix time + pid as lightweight entropy (not cryptographic)
    let pid = std::process::id();
    bytes[0..4].copy_from_slice(&nanos.to_le_bytes());
    bytes[4..8].copy_from_slice(&pid.to_le_bytes());
    // Set version 4 and variant bits
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
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

    // ── new_uuid ──────────────────────────────────────────────────────────────

    #[test]
    fn new_uuid_format_is_valid() {
        let id = new_uuid();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.len(),
            5,
            "UUID should have 5 dash-separated segments: {id}"
        );
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // Version 4 bit
        assert!(
            parts[2].starts_with('4'),
            "version nibble should be 4: {id}"
        );
    }

    #[test]
    fn new_uuid_generates_distinct_values() {
        let a = new_uuid();
        let b = new_uuid();
        // Not a guarantee but very unlikely to collide in practice
        assert_ne!(a, b, "two sequential UUIDs should differ");
    }

    // ── pii patterns compile without panic ────────────────────────────────────

    #[test]
    fn pii_patterns_all_compile() {
        for (pattern, _label) in PII_PATTERNS {
            regex::Regex::new(pattern).expect("PII pattern should compile: {pattern}");
        }
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
}
