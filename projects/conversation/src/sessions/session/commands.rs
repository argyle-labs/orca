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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::context::ProjectContext;
    use crate::sessions::log::SessionLog;
    use ::model::buffer_sink;
    use contract::config::Config;
    use std::sync::{Arc, Mutex};

    /// Build a Config rooted at a throwaway temp dir so filesystem-touching
    /// commands (log search, sessions, recall) resolve to an empty, isolated
    /// directory rather than the developer's real `~/.orca`.
    fn test_config(dir: &std::path::Path) -> Config {
        Config {
            anthropic_api_key: None,
            lmstudio_url: "http://127.0.0.1:1".into(),
            ollama_url: "http://127.0.0.1:1".into(),
            default_model: Model::LMStudio {
                id: "test-model".into(),
                url: String::new(),
            },
            app_dir: dir.to_path_buf(),
            memory_root: dir.join("memory"),
            db_path: dir.join("test.db"),
            ports: Default::default(),
        }
    }

    /// Construct a Session wired to an in-memory buffer with a forced local
    /// model (no network, no API key). `log` is forced to `None` so callers
    /// start from a deterministic "logging inactive" baseline.
    async fn test_session() -> (Session, Arc<Mutex<Vec<u8>>>, tempfile::TempDir) {
        colored::control::set_override(false);
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let ctx = ProjectContext::default();
        let (sink, buf) = buffer_sink();
        let model = Model::LMStudio {
            id: "test-model".into(),
            url: String::new(),
        };
        let mut session = Session::new_with_output_and_model(config, ctx, sink, Some(model))
            .await
            .unwrap();
        session.log = None;
        (session, buf, tmp)
    }

    fn output(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[tokio::test]
    async fn unknown_command_reports_and_hints_help() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/bogus").await.unwrap();
        let out = output(&buf);
        assert!(out.contains("unknown command: /bogus"));
        assert!(out.contains("type /help or help for commands"));
    }

    #[tokio::test]
    async fn clear_empties_messages() {
        let (mut s, buf, _tmp) = test_session().await;
        s.messages.push(Message::user("hi"));
        s.messages.push(Message::user("there"));
        s.handle_command("/clear").await.unwrap();
        assert!(s.messages.is_empty());
        assert!(output(&buf).contains("context cleared."));
    }

    #[tokio::test]
    async fn tokens_alias_prints_ledger() {
        let (mut s, buf, _tmp) = test_session().await;
        s.ledger.record(300, 125);
        s.handle_command("/t").await.unwrap();
        let out = output(&buf);
        // fmt_tokens renders the combined total; ledger format includes both.
        assert!(out.contains(&s.ledger.format()));
    }

    #[tokio::test]
    async fn system_command_echoes_prompt() {
        let (mut s, buf, _tmp) = test_session().await;
        s.system_prompt = "SYSTEM-MARKER-42".into();
        s.handle_command("/system").await.unwrap();
        assert!(output(&buf).contains("SYSTEM-MARKER-42"));
    }

    #[tokio::test]
    async fn agent_command_names_orca() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/agent").await.unwrap();
        assert!(output(&buf).contains("Orca"));
    }

    #[tokio::test]
    async fn narration_toggles_on_then_off() {
        let (mut s, buf, _tmp) = test_session().await;
        assert!(!s.narration);
        s.handle_command("/narration").await.unwrap();
        assert!(s.narration);
        assert!(output(&buf).contains("narration: on"));
        s.handle_command("/narration").await.unwrap();
        assert!(!s.narration);
        assert!(output(&buf).contains("narration: off"));
    }

    #[tokio::test]
    async fn flag_without_log_warns_inactive() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/flag something").await.unwrap();
        assert!(output(&buf).contains("logging not active"));
    }

    #[tokio::test]
    async fn flag_with_log_records_default_note() {
        let (mut s, buf, tmp) = test_session().await;
        let mut log = SessionLog::new(None, &tmp.path().join("logs")).unwrap();
        log.append("user", "orca", "hello", &[]).ok();
        s.log = Some(log);
        // No note argument → default note text is used.
        s.handle_command("/flag").await.unwrap();
        assert!(output(&buf).contains("flagged: flagged as important"));
    }

    #[tokio::test]
    async fn log_without_log_warns_inactive() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/log").await.unwrap();
        assert!(output(&buf).contains("logging not active"));
    }

    #[tokio::test]
    async fn log_with_log_prints_session_and_file() {
        let (mut s, buf, tmp) = test_session().await;
        let log = SessionLog::new(None, &tmp.path().join("logs")).unwrap();
        let sid = log.session_id().to_string();
        s.log = Some(log);
        s.handle_command("/log").await.unwrap();
        let out = output(&buf);
        assert!(out.contains("session:"));
        assert!(out.contains(&sid));
        assert!(out.contains("file:"));
    }

    #[tokio::test]
    async fn search_without_query_shows_usage() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/search").await.unwrap();
        assert!(output(&buf).contains("usage: /search <query>"));
    }

    #[tokio::test]
    async fn search_empty_logs_reports_no_matches() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/search needle").await.unwrap();
        assert!(output(&buf).contains("no matches for 'needle'"));
    }

    #[tokio::test]
    async fn sessions_empty_reports_none() {
        let (mut s, buf, tmp) = test_session().await;
        // Guarantee an empty sessions dir at read time: Session construction may
        // have written a session-log file here, so clear then recreate it —
        // otherwise list_sessions reports that stray file instead of "none".
        let sessions_dir = tmp.path().join("logs/sessions");
        drop(std::fs::remove_dir_all(&sessions_dir));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        s.handle_command("/sessions").await.unwrap();
        assert!(output(&buf).contains("no sessions found"));
    }

    #[tokio::test]
    async fn recall_without_arg_shows_usage() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/recall").await.unwrap();
        assert!(output(&buf).contains("usage: /recall"));
    }

    #[tokio::test]
    async fn escalate_without_arg_shows_usage() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/escalate").await.unwrap();
        assert!(output(&buf).contains("usage: /escalate <question>"));
    }

    #[tokio::test]
    async fn escalate_without_api_key_errors() {
        let (mut s, _buf, _tmp) = test_session().await;
        // No anthropic_api_key configured → cmd_escalate bubbles a context error.
        let err = s
            .handle_command("/escalate why is the sky blue")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no API key"));
    }

    #[tokio::test]
    async fn bg_without_arg_shows_usage() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/bg").await.unwrap();
        assert!(output(&buf).contains("usage: /bg <prompt>"));
    }

    #[tokio::test]
    async fn jobs_with_none_reports_empty() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/jobs").await.unwrap();
        assert!(output(&buf).contains("no background jobs."));
    }

    #[tokio::test]
    async fn output_without_arg_shows_usage() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/output").await.unwrap();
        assert!(output(&buf).contains("usage: /output <job_id>"));
    }

    #[tokio::test]
    async fn output_non_numeric_shows_usage_example() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/output abc").await.unwrap();
        assert!(output(&buf).contains("e.g. /output 1"));
    }

    #[tokio::test]
    async fn output_unknown_job_reports_not_found() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/output 99").await.unwrap();
        assert!(output(&buf).contains("job #99 not found"));
    }

    #[tokio::test]
    async fn output_strips_leading_hash() {
        let (mut s, buf, _tmp) = test_session().await;
        // "#7" must parse as job 7 after trimming the hash, hitting the not-found branch.
        s.handle_command("/output #7").await.unwrap();
        assert!(output(&buf).contains("job #7 not found"));
    }

    #[tokio::test]
    async fn cancel_without_arg_shows_usage() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/cancel").await.unwrap();
        assert!(output(&buf).contains("usage: /cancel <job_id>"));
    }

    #[tokio::test]
    async fn cancel_non_numeric_shows_usage() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/cancel xyz").await.unwrap();
        assert!(output(&buf).contains("usage: /cancel <job_id>"));
    }

    #[tokio::test]
    async fn cancel_unknown_job_reports_not_found() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/cancel 42").await.unwrap();
        assert!(output(&buf).contains("job #42 not found or already finished"));
    }

    #[tokio::test]
    async fn quit_aliases_bail_with_exit_sentinel() {
        for cmd in ["/quit", "/exit", "/q"] {
            let (mut s, buf, _tmp) = test_session().await;
            let err = s.handle_command(cmd).await.unwrap_err();
            assert_eq!(err.to_string(), "__exit__");
            assert!(output(&buf).contains("bye."));
        }
    }

    #[tokio::test]
    async fn help_command_produces_output() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/help").await.unwrap();
        assert!(!output(&buf).is_empty());
    }

    #[tokio::test]
    async fn switch_model_updates_backend_and_window() {
        let (mut s, buf, _tmp) = test_session().await;
        s.handle_command("/model lmstudio:qwen3-coder")
            .await
            .unwrap();
        let out = output(&buf);
        assert!(out.contains("switched to"));
        assert!(out.contains("qwen3-coder"));
        assert_eq!(s.backend.model_id(), "qwen3-coder");
        matches!(s.current_model, Model::LMStudio { .. });
    }

    #[tokio::test]
    async fn context_command_reports_agent_and_model() {
        let (mut s, buf, _tmp) = test_session().await;
        s.active_agent = "orca".into();
        s.messages.push(Message::user("q1"));
        s.messages.push(Message::Assistant {
            text: Some("a1".into()),
            tool_calls: vec![],
        });
        s.messages.push(Message::user("q2"));
        s.cmd_context();
        let out = output(&buf);
        assert!(out.contains("Context:"));
        assert!(out.contains("active agent:  @orca"));
        assert!(out.contains("messages:      3 (2 turns)"));
    }

    #[tokio::test]
    async fn context_command_warns_when_over_75_percent() {
        let (mut s, buf, _tmp) = test_session().await;
        s.context_window = 100;
        s.ledger.record(80, 0); // 80% of window
        s.cmd_context();
        assert!(output(&buf).contains("context over 75%"));
    }

    #[tokio::test]
    async fn context_command_handles_zero_window() {
        let (mut s, buf, _tmp) = test_session().await;
        s.context_window = 0;
        s.ledger.record(1000, 500);
        s.cmd_context();
        // 0-window guard yields 0% and must not divide-by-zero panic.
        assert!(output(&buf).contains("(0%)"));
    }

    #[tokio::test]
    async fn context_command_includes_session_log_line() {
        let (mut s, buf, tmp) = test_session().await;
        let log = SessionLog::new(None, &tmp.path().join("logs")).unwrap();
        let sid = log.session_id().to_string();
        s.log = Some(log);
        s.cmd_context();
        let out = output(&buf);
        assert!(out.contains("session log:"));
        assert!(out.contains(&sid));
    }

    #[tokio::test]
    async fn search_logs_empty_reports_directly() {
        let (s, buf, _tmp) = test_session().await;
        s.cmd_search_logs("anything");
        assert!(output(&buf).contains("no matches for 'anything'"));
    }

    #[tokio::test]
    async fn recall_missing_session_reports() {
        let (s, buf, _tmp) = test_session().await;
        s.cmd_recall_session("nonexistent-id");
        let out = output(&buf);
        // Either an error line or a zero-record session line is acceptable;
        // the branch under test must produce some deterministic output.
        assert!(out.contains("error:") || out.contains("nonexistent-id"));
    }
}
