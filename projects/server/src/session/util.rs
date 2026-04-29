use brain_core::backend::LMStudioBackend;
use brain_utils::config::Model;
use brain_utils::types::truncate_preview;
use anyhow::{Context, Result};
use colored::Colorize;

/// Resolve which model to use. Priority: explicit config > LM Studio auto-discover > Claude fallback.
/// LM Studio is always attempted first — Claude is escalation only.
pub async fn resolve_model(config: &brain_utils::config::Config) -> Result<Model> {
    match &config.default_model {
        Model::Claude(id) if !id.is_empty() => return Ok(Model::Claude(id.clone())),
        Model::LMStudio(id) if !id.is_empty() => return Ok(Model::LMStudio(id.clone())),
        _ => {}
    }

    let lms = LMStudioBackend::new(&config.lmstudio_url, "");
    match lms.list_models().await {
        Ok(models) => {
            let chat_models: Vec<&str> = models
                .iter()
                .map(|s| s.as_str())
                .filter(|m| !m.contains("embed"))
                .collect();

            if chat_models.is_empty() {
                eprintln!(
                    "warning: LM Studio is running but no chat models are loaded — falling back to Claude"
                );
                return claude_fallback(config);
            }
            if chat_models.len() == 1 {
                return Ok(Model::LMStudio(chat_models[0].to_string()));
            }

            println!("{}", "Select a model:".green());
            for (i, m) in chat_models.iter().enumerate() {
                println!("  {}  {m}", format!("[{}]", i + 1).dimmed());
            }
            print!("{} ", "[1]:".cyan());
            std::io::Write::flush(&mut std::io::stdout())?;
            let mut input = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
            let choice: usize = input.trim().parse().unwrap_or(1);
            let selected = chat_models
                .get(choice.saturating_sub(1))
                .unwrap_or(&chat_models[0]);
            Ok(Model::LMStudio(selected.to_string()))
        }
        Err(e) => {
            eprintln!(
                "warning: LM Studio not reachable at {} ({e}) — falling back to Claude",
                config.lmstudio_url
            );
            claude_fallback(config)
        }
    }
}

/// Cheapest Claude model as fallback when LM Studio is unavailable.
fn claude_fallback(config: &brain_utils::config::Config) -> Result<Model> {
    config
        .anthropic_api_key
        .as_ref()
        .context("LM Studio unreachable and no Anthropic API key — run `brain login` or start LM Studio")?;
    Ok(Model::Claude("claude-haiku-4-5-20251001".to_string()))
}

pub fn estimate_context_window(model: &Model) -> usize {
    match model {
        Model::Claude(id) if id.contains("opus") => 200_000,
        Model::Claude(_) => 200_000,
        Model::LMStudio(id) if id.contains("35b") => 32_768,
        Model::LMStudio(_) => 32_768,
    }
}

pub fn history_file() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".brain");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("history"))
}

pub fn check_git_changes(dir: &str) -> Option<usize> {
    let output = std::process::Command::new("git")
        .args(["-C", dir, "status", "--short"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let count = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    if count == 0 { None } else { Some(count) }
}

pub fn agent_emoji(name: &str) -> &'static str {
    match name {
        "brain" | "wolf" => "🧠",
        "pinky" => "🐭",
        "owl" => "🦉",
        "fox" => "🦊",
        "crow" => "🐦‍⬛",
        "bear" => "🐻",
        "spider" => "🕷️",
        "badger" => "🦡",
        "ferret" => "🐾",
        "hawk" => "🦅",
        "mole" => "🐀",
        "elephant" => "🐘",
        "raven" => "🪶",
        "lynx" => "🐱",
        "boar" => "🐗",
        "magpie" => "🐦",
        "oracle" => "🔮",
        _ => "🔧",
    }
}

pub fn find_other_brain_pids() -> Vec<u32> {
    let my_pid = std::process::id();
    let output = std::process::Command::new("pgrep")
        .args(["-x", "brain"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .filter(|&pid| pid != my_pid)
            .collect(),
        _ => vec![],
    }
}

/// Produce a terminal-friendly summary of a tool result.
pub fn summarize_result(tool: &str, content: &str, is_error: bool) -> String {
    if is_error {
        return truncate_preview(content, 300);
    }
    match tool {
        "glob" => {
            let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
            if lines.is_empty() {
                return "(no matches)".to_string();
            }
            format!("{} file(s) matched", lines.len())
        }
        "grep" => {
            let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
            if lines.is_empty() {
                return "(no matches)".to_string();
            }
            let files: std::collections::HashSet<&str> = lines
                .iter()
                .filter_map(|l| l.split(':').next())
                .collect();
            format!("{} match(es) in {} file(s)", lines.len(), files.len())
        }
        "read_file" => {
            let lines = content.lines().count();
            format!("{lines} lines")
        }
        "write_file" => content.to_string(),
        "edit_file" => content.to_string(),
        "bash" => {
            let mut non_empty = content.lines().filter(|l| !l.trim().is_empty());
            match non_empty.next() {
                None => "(no output)".to_string(),
                Some(first) => {
                    let rest = non_empty.count();
                    if rest == 0 {
                        truncate_preview(first, 120)
                    } else {
                        format!("{} (+{rest} more lines)", truncate_preview(first, 80))
                    }
                }
            }
        }
        _ => truncate_preview(content, 200),
    }
}
