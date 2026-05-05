use orca_core::backend::LMStudioBackend;
use config::Model;
use types::truncate_preview;
use anyhow::Result;
use colored::Colorize;

/// Resolve which model to use. Priority: explicit config > LM Studio auto-discover.
///
/// Hard-fail: if no explicit model is configured and LM Studio is unreachable or has
/// no chat models loaded, this returns an error. There is no Claude fallback —
/// configuration that can't be honored must surface, not be papered over.
pub async fn resolve_model(config: &config::Config) -> Result<Model> {
    match &config.default_model {
        Model::Claude(id) if !id.is_empty() => return Ok(Model::Claude(id.clone())),
        Model::LMStudio(id) if !id.is_empty() => return Ok(Model::LMStudio(id.clone())),
        _ => {}
    }

    let lms = LMStudioBackend::new(&config.lmstudio_url, "");
    match lms.list_models().await {
        Ok(models) => {
            let mut chat_models: Vec<&str> = models
                .iter()
                .map(|s| s.as_str())
                .filter(|m| !m.contains("embed"))
                .collect();

            if chat_models.is_empty() {
                anyhow::bail!(
                    "LM Studio is running at {} but no chat models are loaded. Load a model and retry.",
                    config.lmstudio_url
                );
            }

            // Auto-pick priority. Lower rank wins. Qwen ranks first per
            // explicit user preference. Other families follow alphabetically.
            // Reasoning models (deepseek-r1) are still viable — the LMStudio
            // backend folds `reasoning_content` into the response when
            // `content` is empty.
            chat_models.sort_by_key(|m| auto_pick_rank(m));

            if chat_models.len() == 1 {
                return Ok(Model::LMStudio(chat_models[0].to_string()));
            }

            // In non-interactive contexts (MCP server, piped stdin) we cannot prompt —
            // blocking on read_line would hang the caller indefinitely.
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                let first = chat_models[0].to_string();
                eprintln!(
                    "warning: multiple LM Studio models available, auto-selecting: {first}"
                );
                return Ok(Model::LMStudio(first));
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
            anyhow::bail!(
                "LM Studio not reachable at {} ({e}). Start it (with a chat model loaded) or set an explicit model in config.",
                config.lmstudio_url
            );
        }
    }
}

/// Auto-pick rank for LM Studio chat models when more than one is loaded and
/// orca is running non-interactively. Lower wins.
///
/// Qwen first (the user's default tier — strong tool-calling, low thinking
/// overhead). Reasoning-distill models (deepseek-r1-distill-qwen, etc.) sink
/// to the bottom because they share "qwen" in their id but behave like
/// reasoning models, not chat-tuned qwen.
fn auto_pick_rank(id: &str) -> u8 {
    let id_lower = id.to_ascii_lowercase();
    let is_reasoning = id_lower.contains("deepseek-r1")
        || id_lower.contains("/r1-")
        || id_lower.contains("o1-")
        || id_lower.contains("-thinking")
        || id_lower.contains("reasoning");
    let is_qwen = id_lower.starts_with("qwen/") || id_lower.contains("/qwen");

    match (is_qwen, is_reasoning) {
        (true,  false) => 0,    // chat-tuned qwen — preferred
        (false, false) => 10,   // other chat models
        (true,  true)  => 50,   // qwen-distilled reasoning model
        (false, true)  => 60,   // pure reasoning model
    }
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
    let dir = home.join(".orca");
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
        "orca" => "🐋",
        "wolf" => "🐺",
        "otter" => "🦦",
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

pub fn find_other_orca_pids() -> Vec<u32> {
    let my_pid = std::process::id();
    let output = std::process::Command::new("pgrep")
        .args(["-x", "orca"])
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
