use super::Session;
use crate::sessions::ledger::fmt_tokens;
use ::model::{
    ClaudeBackend, LMStudioBackend, Message, ModelBackend, build_backend, estimate_context_window,
};
use anyhow::{Context, Result};
use colored::Colorize;
use contract::config::Model;
use tokio_util::sync::CancellationToken;

impl Session {
    pub(super) async fn handle_command(&mut self, input: &str) -> Result<()> {
        let parts: Vec<&str> = input.splitn(3, ' ').collect();
        match parts[0] {
            "/model" => {
                if parts.len() < 2 {
                    self.cmd_list_models().await?;
                } else {
                    self.cmd_switch_model(parts[1]).await?;
                }
            }
            "/models" => self.cmd_list_models().await?,
            "/clear" => {
                self.messages.clear();
                self.out(&"context cleared.".dimmed().to_string());
            }
            "/tokens" | "/t" => {
                self.out(&self.ledger.format().dimmed().to_string());
            }
            "/context" | "/ctx" => self.cmd_context(),
            "/system" => self.out(&self.system_prompt.dimmed().to_string()),
            "/agent" => self.out(&"you're talking to Orca.".cyan().to_string()),
            "/flag" => {
                let note = parts.get(1).copied().unwrap_or("flagged as important");
                if let Some(log) = &mut self.log {
                    log.flag_last(note).ok();
                    self.out(&format!("flagged: {note}").green().to_string());
                } else {
                    self.out(&"logging not active".yellow().to_string());
                }
            }
            "/log" => {
                if let Some(log) = &self.log {
                    self.out(
                        &format!("session: {}", log.session_id())
                            .dimmed()
                            .to_string(),
                    );
                    self.out(
                        &format!("file: {}", log.path().display())
                            .dimmed()
                            .to_string(),
                    );
                } else {
                    self.out(&"logging not active".yellow().to_string());
                }
            }
            "/search" => {
                if parts.len() < 2 {
                    self.out(&"usage: /search <query>".yellow().to_string());
                } else {
                    let query = parts[1..].join(" ");
                    self.cmd_search_logs(&query);
                }
            }
            "/sessions" => self.cmd_list_sessions(),
            "/recall" => {
                if parts.len() < 2 {
                    self.out(
                        &"usage: /recall <session_id or partial>"
                            .yellow()
                            .to_string(),
                    );
                } else {
                    self.cmd_recall_session(parts[1]);
                }
            }
            "/escalate" => {
                if parts.len() < 2 {
                    self.out(&"usage: /escalate <question>".yellow().to_string());
                } else {
                    let question = parts[1..].join(" ");
                    self.cmd_escalate(&question).await?;
                }
            }
            "/narration" => {
                self.narration = !self.narration;
                let state = if self.narration { "on" } else { "off" };
                self.out(&format!("narration: {state}").green().to_string());
            }
            "/bg" => {
                if parts.len() < 2 {
                    self.out(&"usage: /bg <prompt>".yellow().to_string());
                } else {
                    let bg_prompt = parts[1..].join(" ");
                    self.cmd_bg(&bg_prompt)?;
                }
            }
            "/jobs" => {
                for line in self.jobs.list() {
                    self.out(&line);
                }
            }
            "/output" => {
                if parts.len() < 2 {
                    self.out(&"usage: /output <job_id>".yellow().to_string());
                } else if let Ok(id) = parts[1].trim_start_matches('#').parse::<usize>() {
                    match self.jobs.get_output(id) {
                        Some(out) if out.is_empty() => {
                            self.out(&"(no output yet)".dimmed().to_string());
                        }
                        Some(out) => {
                            self.out(&format!("── job #{id} output ──").cyan().to_string());
                            self.out_raw(&out);
                            self.out(&"── end ──".to_string().cyan().to_string());
                        }
                        None => self.out(&format!("job #{id} not found").yellow().to_string()),
                    }
                } else {
                    self.out(
                        &"usage: /output <job_id>  (e.g. /output 1)"
                            .yellow()
                            .to_string(),
                    );
                }
            }
            "/cancel" => {
                if parts.len() < 2 {
                    self.out(&"usage: /cancel <job_id>".yellow().to_string());
                } else if let Ok(id) = parts[1].trim_start_matches('#').parse::<usize>() {
                    if self.jobs.cancel(id) {
                        self.out(&format!("cancelled job #{id}").green().to_string());
                    } else {
                        self.out(
                            &format!("job #{id} not found or already finished")
                                .yellow()
                                .to_string(),
                        );
                    }
                } else {
                    self.out(&"usage: /cancel <job_id>".yellow().to_string());
                }
            }
            "/cleanup" => self.cleanup_phantom_processes(),
            "/quit" | "/exit" | "/q" => {
                self.out(&"bye.".dimmed().to_string());
                anyhow::bail!("__exit__");
            }
            "/help" | "/h" => self.print_help(),
            _ => {
                self.out(
                    &format!("unknown command: {}", parts[0])
                        .yellow()
                        .to_string(),
                );
                self.out(&"  type /help or help for commands".dimmed().to_string());
            }
        }
        Ok(())
    }

    fn cmd_bg(&mut self, prompt: &str) -> Result<()> {
        let id = self.jobs.spawn(
            &self.config,
            &self.current_model,
            self.system_prompt.clone(),
            prompt.to_string(),
        )?;
        self.out(
            &format!("job #{id} started — /jobs to check status, /output {id} to view result")
                .green()
                .to_string(),
        );
        Ok(())
    }

    fn cmd_context(&self) {
        let msg_count = self.messages.len();
        let turns = self
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User { .. }))
            .count();
        let session_tokens = self.ledger.session_input + self.ledger.session_output;
        let pct = if self.context_window > 0 {
            (session_tokens as f64 / self.context_window as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        self.out(&"Context:".green().to_string());
        self.out(&format!("  active agent:  @{}", self.active_agent));
        self.out(&format!(
            "  model:         {}:{}",
            self.backend.name(),
            self.backend.model_id()
        ));
        self.out(&format!("  messages:      {} ({} turns)", msg_count, turns));
        self.out(&format!(
            "  tokens (est):  {} / {} ({:.0}%)",
            fmt_tokens(session_tokens),
            fmt_tokens(self.context_window as u32),
            pct,
        ));
        if let Some(log) = &self.log {
            self.out(&format!("  session log:   {}", log.session_id()));
        }
        if pct > 75.0 {
            self.out(
                &"  ⚠ context over 75% — consider /clear or starting a new session"
                    .yellow()
                    .to_string(),
            );
        }
    }

    async fn cmd_switch_model(&mut self, spec: &str) -> Result<()> {
        let model = Model::parse(spec);
        let new_backend = build_backend(&self.config, &model)?;
        self.context_window = estimate_context_window(&model);
        self.out(
            &format!(
                "switched to {}:{}",
                new_backend.name(),
                new_backend.model_id()
            )
            .green()
            .to_string(),
        );
        self.backend = new_backend;
        self.current_model = model;
        Ok(())
    }

    async fn cmd_list_models(&mut self) -> Result<()> {
        let current = format!("{}:{}", self.backend.name(), self.backend.model_id());
        let mut all: Vec<(String, String)> = vec![];

        let lms = LMStudioBackend::new(&self.config.lmstudio_url, "");
        match lms.list_models().await {
            Ok(models) => {
                for m in models.iter().filter(|m| !m.contains("embed")) {
                    all.push((format!("lmstudio:{m}"), format!("lmstudio:{m}")));
                }
            }
            Err(_) => self.out(&"  LM Studio: not reachable".dimmed().to_string()),
        }

        if self.config.anthropic_api_key.is_some() {
            for m in [
                "claude-sonnet-4-6",
                "claude-opus-4-6",
                "claude-haiku-4-5-20251001",
            ] {
                all.push((format!("claude:{m}"), m.to_string()));
            }
        } else {
            self.out(
                &"  Claude: no API key (run `orca login`)"
                    .dimmed()
                    .to_string(),
            );
        }

        if all.is_empty() {
            self.out(&"no models available".yellow().to_string());
            return Ok(());
        }

        self.out(&"Available models:".green().to_string());
        for (i, (display, _)) in all.iter().enumerate() {
            let marker = if current.ends_with(
                display
                    .trim_start_matches("lmstudio:")
                    .trim_start_matches("claude:"),
            ) || current == *display
            {
                "●"
            } else {
                " "
            };
            self.out(&format!(
                "  {} {}  {display}",
                marker,
                format!("[{}]", i + 1).dimmed()
            ));
        }
        self.out(
            &"  use /model <spec> to switch  (e.g. /model lmstudio:qwen3)"
                .dimmed()
                .to_string(),
        );
        Ok(())
    }

    async fn cmd_escalate(&mut self, question: &str) -> Result<()> {
        let api_key = self
            .config
            .anthropic_api_key
            .clone()
            .context("no API key — run `orca login` first")?;

        self.out(&"↑ escalating to Claude…".yellow().to_string());

        let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
        let msgs = vec![Message::user(question)];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let ctrl_c_task = tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel_clone.cancel();
        });
        let response = claude
            .chat(&msgs, &[], &self.system_prompt, cancel, &self.output)
            .await?;
        ctrl_c_task.abort();

        self.ledger
            .record(response.input_tokens, response.output_tokens);

        if let Some(log) = &mut self.log {
            log.append("user", "escalate", question, &["escalation"])
                .ok();
            log.append(
                "assistant",
                "claude-sonnet-4-6",
                &response.text,
                &["escalation"],
            )
            .ok();
        }

        self.messages.push(Message::user(format!(
            "[escalated to Claude]\nQuestion: {question}"
        )));
        self.messages.push(Message::Assistant {
            text: Some(response.text),
            tool_calls: vec![],
        });

        self.out(&self.ledger.format().dimmed().to_string());
        Ok(())
    }

    fn cmd_search_logs(&self, query: &str) {
        let logs_dir = self.config.logs_dir();
        match crate::sessions::log::search_logs(&logs_dir, query, 20) {
            Ok(matches) if matches.is_empty() => {
                self.out(&format!("no matches for '{query}'").dimmed().to_string());
            }
            Ok(matches) => {
                self.out(
                    &format!("found {} match(es):", matches.len())
                        .green()
                        .to_string(),
                );
                for m in &matches {
                    let session = m["session"].as_str().unwrap_or("?");
                    let role = m["role"].as_str().unwrap_or("?");
                    let agent = m["agent"].as_str().unwrap_or("?");
                    let content = m["content"].as_str().unwrap_or("");
                    let preview: String = content.chars().take(120).collect();
                    let important = m["important"].as_bool() == Some(true);
                    let flag = if important { " ★" } else { "" };
                    self.out(&format!(
                        "  {} {} @{} {}{}",
                        session.dimmed(),
                        role.cyan(),
                        agent,
                        preview,
                        flag.yellow()
                    ));
                }
            }
            Err(e) => self.out(&format!("search error: {e}").red().to_string()),
        }
    }

    fn cmd_list_sessions(&self) {
        let logs_dir = self.config.logs_dir();
        match crate::sessions::log::list_sessions(&logs_dir, 15) {
            Ok(sessions) if sessions.is_empty() => {
                self.out(&"no sessions found".dimmed().to_string());
            }
            Ok(sessions) => {
                self.out(&"Recent sessions:".green().to_string());
                for s in &sessions {
                    let flag = if s.flagged > 0 {
                        format!(" (★ {})", s.flagged)
                    } else {
                        String::new()
                    };
                    self.out(&format!(
                        "  {}  {} msgs{}",
                        s.session_id.dimmed(),
                        s.messages,
                        flag.yellow()
                    ));
                }
            }
            Err(e) => self.out(&format!("error: {e}").red().to_string()),
        }
    }

    fn cmd_recall_session(&self, session_id: &str) {
        let logs_dir = self.config.logs_dir();
        match crate::sessions::log::recall_session(&logs_dir, session_id) {
            Ok(records) => {
                self.out(
                    &format!("session: {} ({} records)", session_id, records.len())
                        .green()
                        .to_string(),
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
                    self.out(&format!(
                        "  {} {}{}{}",
                        prefix.cyan(),
                        preview,
                        ellipsis,
                        flag.yellow()
                    ));
                }
            }
            Err(e) => self.out(&format!("error: {e}").red().to_string()),
        }
    }
}
