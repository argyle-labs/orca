use crate::sessions::log;
use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use contract::config::Config;

#[derive(Subcommand)]
pub enum LogAction {
    /// Search all session logs for a keyword
    Search { query: String },
    /// List recent sessions
    Sessions {
        #[arg(short, long, default_value = "15")]
        limit: usize,
    },
    /// Recall messages from a session
    Recall { session_id: String },
}

pub fn cmd_log(config: &Config, action: LogAction) -> Result<()> {
    let logs_dir = config.logs_dir();

    match action {
        LogAction::Search { query } => match log::search_logs(&logs_dir, &query, 20) {
            Ok(matches) if matches.is_empty() => {
                println!("{}", format!("no matches for '{query}'").dimmed());
            }
            Ok(matches) => {
                println!("{}", format!("found {} match(es):", matches.len()).green());
                for m in &matches {
                    let session = m["session"].as_str().unwrap_or("?");
                    let role = m["role"].as_str().unwrap_or("?");
                    let agent = m["agent"].as_str().unwrap_or("?");
                    let content = m["content"].as_str().unwrap_or("");
                    let preview: String = content.chars().take(120).collect();
                    let important = m["important"].as_bool() == Some(true);
                    let flag = if important { " ★" } else { "" };
                    println!(
                        "  {} {} @{} {}{}",
                        session.dimmed(),
                        role.cyan(),
                        agent,
                        preview,
                        flag.yellow()
                    );
                }
            }
            Err(e) => eprintln!("{}", format!("search error: {e}").red()),
        },
        LogAction::Sessions { limit } => match log::list_sessions(&logs_dir, limit) {
            Ok(sessions) if sessions.is_empty() => {
                println!("{}", "no sessions found".dimmed());
            }
            Ok(sessions) => {
                println!("{}", "Recent sessions:".green());
                for s in &sessions {
                    let flag = if s.flagged > 0 {
                        format!(" (★ {})", s.flagged)
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}  {} msgs{}",
                        s.session_id.dimmed(),
                        s.messages,
                        flag.yellow()
                    );
                }
            }
            Err(e) => eprintln!("{}", format!("error: {e}").red()),
        },
        LogAction::Recall { session_id } => match log::recall_session(&logs_dir, &session_id) {
            Ok(records) => {
                println!(
                    "{}",
                    format!("session: {} ({} records)", session_id, records.len()).green()
                );
                for r in &records {
                    let role = r["role"].as_str().unwrap_or("?");
                    let agent = r["agent"].as_str().unwrap_or("");
                    let content = r["content"].as_str().unwrap_or("");
                    let important = r["important"].as_bool() == Some(true);
                    let flag = if important { " ★" } else { "" };
                    let prefix = if agent.is_empty() {
                        format!("[{role}]")
                    } else {
                        format!("[{role}/@{agent}]")
                    };
                    let preview: String = content.chars().take(200).collect();
                    let ellipsis = if content.chars().count() > 200 {
                        "…"
                    } else {
                        ""
                    };
                    println!(
                        "  {} {}{}{}",
                        prefix.cyan(),
                        preview,
                        ellipsis,
                        flag.yellow()
                    );
                }
            }
            Err(e) => eprintln!("{}", format!("error: {e}").red()),
        },
    }

    Ok(())
}
