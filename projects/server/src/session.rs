use brain_core::backend::{
    ClaudeBackend, LMStudioBackend, ModelBackend, OutputSink, build_backend,
    sink_write, sink_writeln, stdout_sink,
};
use brain_core::tools::ToolRegistry;
use brain_jobs::JobManager;
use brain_utils::config::{Config, Model};
use brain_utils::ledger::TokenLedger;
use brain_utils::log::SessionLog;
use brain_utils::types::{Message, ToolResult, truncate_preview};
use crate::context::ProjectContext;
use crate::tui::{self, TuiAction, TuiApp};
use anyhow::{Context, Result};
use colored::Colorize;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use rustyline::DefaultEditor;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct Session {
    config: Config,
    backend: Box<dyn ModelBackend>,
    current_model: Model,
    messages: Vec<Message>,
    system_prompt: String,
    active_agent: String,
    ledger: TokenLedger,
    tools: ToolRegistry,
    project: Option<String>,
    log: Option<SessionLog>,
    context_window: usize,
    narration: bool,
    output: OutputSink,
    jobs: JobManager,
}

impl Session {
    pub async fn new(config: Config, ctx: ProjectContext) -> Result<Self> {
        Self::new_with_output(config, ctx, stdout_sink()).await
    }

    pub async fn new_with_output(
        config: Config,
        ctx: ProjectContext,
        output: OutputSink,
    ) -> Result<Self> {
        let system_prompt = ctx.build_system_prompt(&config);
        let project = ctx.project.clone();

        let model = resolve_model(&config).await?;
        let context_window = estimate_context_window(&model);
        let backend = build_backend(&config, &model)?;

        let log = SessionLog::new(project.as_deref(), &config.logs_dir()).ok();

        Ok(Session {
            system_prompt,
            active_agent: "brain".to_string(),
            project,
            backend,
            current_model: model,
            messages: Vec::new(),
            ledger: TokenLedger::default(),
            tools: ToolRegistry::default(),
            context_window,
            log,
            narration: false,
            output,
            jobs: JobManager::new(),
            config,
        })
    }

    // ── Output helpers ───────────────────────────────────────────────────────
    // ALL session output goes through these so TUI mode can redirect to channel.

    fn out(&self, s: &str) {
        sink_writeln(&self.output, s);
    }

    fn out_raw(&self, s: &str) {
        sink_write(&self.output, s);
    }

    fn out_fmt(&self, s: impl std::fmt::Display) {
        sink_writeln(&self.output, &s.to_string());
    }

    /// Replace the output sink (used when switching to TUI mode).
    pub fn set_output(&mut self, sink: OutputSink) {
        self.output = sink.clone();
        self.tools.output = sink;
    }

    /// Enable TUI mode settings (auto-approve bash, etc.)
    pub fn enable_tui_mode(&mut self) {
        self.tools.permissions.auto_approve = true;
    }

    // ── Public entry points ──────────────────────────────────────────────────

    pub async fn one_shot(&mut self, prompt: String) -> Result<()> {
        self.chat(prompt).await
    }

    /// Classic readline-based REPL (non-TUI).
    pub async fn run(&mut self) -> Result<()> {
        let mut rl = DefaultEditor::new().context("failed to init readline")?;

        let history_path = history_file();
        if let Some(p) = &history_path {
            rl.load_history(p).ok();
        }

        self.print_banner();
        self.warn_phantom_processes();

        loop {
            for note in self.jobs.drain_notifications() {
                self.out(&note);
            }

            let emoji = agent_emoji(&self.active_agent);
            let prompt = format!("{emoji} {} {} ", self.active_agent.cyan(), "›".dimmed(),);
            let readline = rl.readline(&prompt);

            match readline {
                Ok(line) => {
                    let input = line.trim().to_string();
                    if input.is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(&input);

                    if matches!(input.as_str(), "exit" | "quit" | "q" | "bye") {
                        self.out(&"bye.".dimmed().to_string());
                        if let Some(p) = &history_path {
                            rl.save_history(p).ok();
                        }
                        break;
                    }

                    let result = match input.as_str() {
                        "help" => {
                            self.print_help();
                            Ok(())
                        }
                        "clear" => {
                            self.messages.clear();
                            self.out(&"context cleared.".dimmed().to_string());
                            Ok(())
                        }
                        _ if input.starts_with('/') => self.handle_command(&input).await,
                        _ => self.chat(input).await,
                    };

                    if let Err(e) = result {
                        if e.to_string() == "__exit__" {
                            if let Some(p) = &history_path {
                                rl.save_history(p).ok();
                            }
                            break;
                        }
                        self.out(&format!("error: {e}").red().to_string());
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    self.out(&"^C".dimmed().to_string());
                    continue;
                }
                Err(rustyline::error::ReadlineError::Eof) => {
                    self.out(&"\nbye.".dimmed().to_string());
                    if let Some(p) = &history_path {
                        rl.save_history(p).ok();
                    }
                    break;
                }
                Err(e) => {
                    self.out(&format!("readline error: {e}").red().to_string());
                    if let Some(p) = &history_path {
                        rl.save_history(p).ok();
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    /// TUI-based REPL — split-pane with always-responsive input.
    pub async fn run_tui(&mut self) -> Result<()> {
        // Channel: session output → TUI rendering
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        self.set_output(tui::tui_sink(out_tx));
        self.enable_tui_mode();

        let mut terminal = tui::setup_terminal()?;

        let prompt_str = format!(
            "{} {} ›",
            agent_emoji(&self.active_agent),
            self.active_agent,
        );
        let mut app = TuiApp::new(&prompt_str);

        // Banner
        app.push_line(format!(
            "{} brain · {}",
            agent_emoji(&self.active_agent),
            self.project.as_deref().unwrap_or("general"),
        ));
        app.push_line(format!(
            "@{} · {}:{}",
            self.active_agent,
            self.backend.name(),
            self.backend.model_id(),
        ));
        app.push_line("/help · Ctrl+C to quit · PageUp/Down to scroll");
        app.push_line("");

        let mut event_stream = EventStream::new();

        loop {
            // Draw
            terminal.draw(|f| tui::render(f, &app))?;

            // Drain output channel (non-blocking)
            while let Ok(chunk) = out_rx.try_recv() {
                app.append(&chunk);
            }

            // Drain job notifications
            for note in self.jobs.drain_notifications() {
                app.push_line(note);
            }

            if app.should_quit {
                break;
            }

            // If a chat is running, we can't call self methods.
            // Instead, we process in a cooperative loop: try_recv + poll events.
            // When idle, we block on select. When busy, we poll.
            if app.busy {
                // We're mid-chat — just drain events and output until done.
                // This shouldn't happen in the actor model, but as a safety net:
                app.busy = false;
            }

            tokio::select! {
                Some(Ok(event)) = event_stream.next() => {
                    if let Event::Key(key) = event {
                        match app.handle_key(key) {
                            TuiAction::Submit(input) => {
                                app.push_line(format!("› {input}"));

                                if matches!(input.as_str(), "exit" | "quit" | "q" | "bye") {
                                    break;
                                }

                                if input == "help" || input == "/help" || input == "/h" {
                                    self.print_help();
                                    // Drain the output channel so help text appears
                                    while let Ok(chunk) = out_rx.try_recv() {
                                        app.append(&chunk);
                                    }
                                    continue;
                                }

                                // Process command or chat
                                app.busy = true;
                                terminal.draw(|f| tui::render(f, &app))?;

                                if input.starts_with('/') {
                                    let _ = self.handle_command(&input).await;
                                } else {
                                    let _ = self.chat(input).await;
                                }

                                app.busy = false;

                                // Drain all output produced during chat
                                while let Ok(chunk) = out_rx.try_recv() {
                                    app.append(&chunk);
                                }
                            }
                            TuiAction::Cancel => {
                                app.push_line("[cancelled]");
                            }
                            TuiAction::Quit => {
                                break;
                            }
                            TuiAction::None => {}
                        }
                    }
                }
                Some(chunk) = out_rx.recv() => {
                    app.append(&chunk);
                }
            }
        }

        tui::restore_terminal(&mut terminal);
        Ok(())
    }

    // ── Chat ─────────────────────────────────────────────────────────────────

    async fn chat(&mut self, input: String) -> Result<()> {
        if let Some(log) = &mut self.log {
            log.append("user", &self.active_agent.clone(), &input, &[])
                .ok();
        }

        self.messages.push(Message::user(input));

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel_clone.cancel();
        });

        let max_rounds = 30;
        for round in 0..max_rounds {
            let tools = ToolRegistry::definitions();
            let response = self
                .backend
                .chat(
                    &self.messages,
                    &tools,
                    &self.system_prompt,
                    cancel.child_token(),
                    &self.output,
                )
                .await?;

            self.ledger
                .record(response.input_tokens, response.output_tokens);

            if cancel.is_cancelled() {
                self.out(&"\n[chat interrupted]".yellow().to_string());
                break;
            }

            let has_tools = !response.tool_calls.is_empty();

            if !response.text.trim().is_empty()
                && let Some(log) = &mut self.log
            {
                log.append("assistant", &self.active_agent.clone(), &response.text, &[])
                    .ok();
            }

            self.messages.push(Message::Assistant {
                text: if response.text.is_empty() {
                    None
                } else {
                    Some(response.text.clone())
                },
                tool_calls: response.tool_calls.clone(),
            });

            if !has_tools {
                self.out(&self.ledger.format().dimmed().to_string());
                self.check_commit_status();
                self.out("");
                break;
            }

            if round > 0 {
                self.out_fmt(format!("  [round {}/{}]", round + 1, max_rounds).dimmed());
            }

            let mut results: Vec<ToolResult> = Vec::new();
            for tc in &response.tool_calls {
                if cancel.is_cancelled() {
                    self.out(&"\n[interrupted during tool execution]".yellow().to_string());
                    break;
                }

                self.out_fmt(format!("  input: {}", tc.input).dimmed());

                let mut r = if tc.name == "delegate" {
                    self.execute_delegate(&tc.input).await
                } else if tc.name == "confirm" {
                    self.execute_confirm(&tc.input)
                } else {
                    self.tools.execute(&tc.name, &tc.input)
                };
                r.tool_use_id = tc.id.clone();

                let preview = truncate_preview(&r.content, 200);
                let styled = if r.is_error {
                    preview.red().to_string()
                } else {
                    preview.dimmed().to_string()
                };
                self.out(&styled);

                results.push(r);
            }

            if cancel.is_cancelled() {
                break;
            }

            self.messages.push(Message::ToolResults(results));

            if round == max_rounds - 1 {
                self.out(&"(max tool rounds reached — stopping)".yellow().to_string());
            }
        }

        Ok(())
    }

    // ── Commands ─────────────────────────────────────────────────────────────

    async fn handle_command(&mut self, input: &str) -> Result<()> {
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
            "/agent" => self.out(&"you're talking to Brain.".cyan().to_string()),
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
                &"  Claude: no API key (run `brain login`)"
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
            .context("no API key — run `brain login` first")?;

        self.out(&"↑ escalating to Claude…".yellow().to_string());

        let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
        let msgs = vec![Message::user(question)];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel_clone.cancel();
        });
        let response = claude
            .chat(&msgs, &[], &self.system_prompt, cancel, &self.output)
            .await?;

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
        match brain_utils::log::search_logs(&logs_dir, query, 20) {
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
        match brain_utils::log::list_sessions(&logs_dir, 15) {
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
        match brain_utils::log::recall_session(&logs_dir, session_id) {
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

    // ── Delegation ───────────────────────────────────────────────────────────

    async fn execute_delegate(&mut self, input: &serde_json::Value) -> ToolResult {
        let agent = input["agent"].as_str().unwrap_or("");
        let task = input["task"].as_str().unwrap_or("");

        if agent.is_empty() || task.is_empty() {
            return ToolResult {
                tool_use_id: String::new(),
                content: "error: agent and task are required".into(),
                is_error: true,
            };
        }

        let agent_prompt = match brain_agents::load_agent_prompt(agent, &self.config.agents_dir())
        {
            Some(prompt) => prompt,
            None => {
                return ToolResult {
                    tool_use_id: String::new(),
                    content: format!("error: agent @{agent} not found"),
                    is_error: true,
                };
            }
        };

        let agent_icon = agent_emoji(agent);
        if self.narration {
            self.out("");
            self.out_fmt(
                format!(
                    "  🧠 Brain: \"Pinky, I'm sending this to {agent_icon} @{agent}. {}\"",
                    match agent {
                        "fox" => "Something is broken and Fox will sniff out the root cause.",
                        "owl" => "Owl will read the code and explain what's happening.",
                        "crow" => "Crow will write the implementation.",
                        "spider" => "Spider will find the pattern and simplify.",
                        "bear" => "Bear will tear this apart and find every weakness.",
                        "ferret" => "Ferret will check this against proper standards.",
                        "badger" => "Badger knows the homelab infrastructure.",
                        "hawk" => "Hawk will inspect the containers.",
                        "mole" => "Mole will dig into the system processes.",
                        "elephant" => "Elephant never forgets the docs.",
                        "raven" => "Raven will capture this in the vault.",
                        "lynx" => "Lynx will plan the most efficient path.",
                        "pinky" => "Pinky will search the session logs. NARF!",
                        "boar" => "Boar will charge through the carl commands.",
                        _ => "This specialist knows what to do.",
                    }
                )
                .dimmed(),
            );
            self.out_fmt(
                format!(
                    "  🐭 Pinky: \"Ooh! {agent_icon} @{agent}! NARF! I'll write everything down!\""
                )
                .dimmed(),
            );
            self.out("");
        }

        if let Some(log) = &mut self.log {
            log.append(
                "system",
                &self.active_agent,
                &format!("delegated to @{agent}: {task}"),
                &["delegation"],
            )
            .ok();
        }

        let specialist_tools: Vec<_> = ToolRegistry::definitions()
            .into_iter()
            .filter(|t| t.name != "delegate")
            .collect();

        let mut sub_messages = vec![Message::user(task)];
        let mut full_response = String::new();
        let max_rounds = 20;

        self.out_fmt(format!("  ┌─ {agent_icon} @{agent} ────────────────────────────").cyan());

        for round in 0..max_rounds {
            let cancel = CancellationToken::new();
            let cancel_clone = cancel.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                cancel_clone.cancel();
            });

            self.out_raw(&"  │ ".cyan().to_string());
            let result = self
                .backend
                .chat(
                    &sub_messages,
                    &specialist_tools,
                    &agent_prompt,
                    cancel,
                    &self.output,
                )
                .await;

            let response = match result {
                Ok(r) => r,
                Err(e) => {
                    self.out_fmt(format!("  └─ @{agent} error ──────────────────────").red());
                    return ToolResult {
                        tool_use_id: String::new(),
                        content: format!("delegation error: {e}"),
                        is_error: true,
                    };
                }
            };

            self.ledger
                .record(response.input_tokens, response.output_tokens);

            if !response.text.is_empty() {
                full_response.push_str(&response.text);
                if let Some(log) = &mut self.log {
                    log.append("assistant", agent, &response.text, &["delegation"])
                        .ok();
                }
            }

            sub_messages.push(Message::Assistant {
                text: if response.text.is_empty() {
                    None
                } else {
                    Some(response.text.clone())
                },
                tool_calls: response.tool_calls.clone(),
            });

            if response.tool_calls.is_empty() {
                break;
            }

            let mut tool_results: Vec<ToolResult> = Vec::new();
            for tc in &response.tool_calls {
                self.out_raw(&"  │ ".cyan().to_string());
                self.out_fmt(format!("⚙ {} {}", tc.name, tc.input).dimmed());
                let mut r = self.tools.execute(&tc.name, &tc.input);
                r.tool_use_id = tc.id.clone();

                if let Some(log) = &mut self.log {
                    log.append(
                        "tool",
                        agent,
                        &format!("{}({})", tc.name, tc.input),
                        &["delegation"],
                    )
                    .ok();
                }

                let preview = truncate_preview(&r.content, 200);
                self.out_raw(&"  │ ".cyan().to_string());
                let styled = if r.is_error {
                    preview.red().to_string()
                } else {
                    preview.dimmed().to_string()
                };
                self.out(&styled);

                tool_results.push(r);
            }

            sub_messages.push(Message::ToolResults(tool_results));

            if round == max_rounds - 1 {
                self.out(&"  │ (max rounds reached)".yellow().to_string());
            }
        }

        self.out_fmt(format!("  └─ {agent_icon} @{agent} done ──────────────────────").cyan());
        if self.narration {
            let (brain_line, pinky_line) = match agent {
                "fox" => (
                    format!("\"Excellent work, {agent_icon} Fox. The trail was well-traced.\""),
                    "\"Ooh! Was it a mystery? I love mysteries! TROZ!\"".to_string(),
                ),
                "bear" => (
                    format!("\"Thorough as always, {agent_icon} Bear. Nothing escapes you.\""),
                    "\"Bear is scary when he does that, Brain! NARF!\"".to_string(),
                ),
                "crow" => (
                    format!("\"Clean implementation, {agent_icon} Crow. Well built.\""),
                    "\"Ooh! New code! Can I name a variable? POIT!\"".to_string(),
                ),
                _ => (
                    format!("\"Thank you, {agent_icon} @{agent}. Pinky, did you get all that?\""),
                    "\"Every word, Brain! POIT!\"".to_string(),
                ),
            };
            self.out("");
            self.out_fmt(format!("  🧠 Brain: {brain_line}").dimmed());
            self.out_fmt(format!("  🐭 Pinky: {pinky_line}").dimmed());
            self.out("");
        }

        ToolResult {
            tool_use_id: String::new(),
            content: full_response,
            is_error: false,
        }
    }

    fn execute_confirm(&self, input: &serde_json::Value) -> ToolResult {
        let question = input["question"].as_str().unwrap_or("Proceed?");
        self.out(&format!("{} (auto-confirmed)", question).cyan().to_string());
        ToolResult {
            tool_use_id: String::new(),
            content: "yes".to_string(),
            is_error: false,
        }
    }

    fn check_commit_status(&self) {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        if let Some(dir) = cwd
            && let Some(count) = check_git_changes(&dir)
            && count >= 5
        {
            self.out_fmt(
                format!(
                    "⚠  {} uncommitted files in {} — good time to commit",
                    count,
                    dir.split('/').next_back().unwrap_or(&dir)
                )
                .yellow(),
            );
        }
    }

    // ── Output helpers (banner, help, etc.) ───────────────────────────────────

    fn print_banner(&self) {
        let emoji = agent_emoji(&self.active_agent);
        self.out("");
        if let Some(p) = &self.project {
            self.out_fmt(format!("  {emoji} brain  ·  {p}").bold());
        } else {
            self.out_fmt(format!("  {emoji} brain").bold());
        }
        self.out(&format!(
            "  {}  {}",
            format!("{emoji} @{}", self.active_agent).cyan(),
            format!("{}:{}", self.backend.name(), self.backend.model_id()).dimmed()
        ));
        self.out(&"  /help · exit to quit".dimmed().to_string());
        self.out("");
    }

    fn print_help(&self) {
        self.out(&"Navigation:".green().to_string());
        self.out("  /model            list models + interactive picker");
        self.out("  /model <spec>     switch directly  (lmstudio:qwen3, claude-sonnet-4-6)");
        self.out("  clear             clear conversation history");
        self.out("");
        self.out(&"Context:".green().to_string());
        self.out("  /context          show messages, token usage, context window %");
        self.out("  /tokens           token ledger");
        self.out("  /agent            show active agent");
        self.out("  /system           show current system prompt");
        self.out("");
        self.out(&"Logging (Pinky):".green().to_string());
        self.out("  /flag [note]      mark last message as important");
        self.out("  /log              show current session log path");
        self.out("  /search <query>   search all session logs for a keyword");
        self.out("  /sessions         list recent sessions");
        self.out("  /recall <id>      replay a session's messages");
        self.out("");
        self.out(&"Background jobs:".green().to_string());
        self.out("  /bg <prompt>      run a prompt in the background");
        self.out("  /jobs             list background jobs");
        self.out("  /output <id>      view a job's output  (e.g. /output 1)");
        self.out("  /cancel <id>      cancel a running job");
        self.out("");
        self.out(&"Escalation:".green().to_string());
        self.out("  /escalate <q>     send question to Claude, inject answer into context");
        self.out("");
        self.out(&"Preferences:".green().to_string());
        self.out("  /narration        toggle Brain/Pinky narration on/off");
        self.out("");
        self.out(&"Maintenance:".green().to_string());
        self.out("  /cleanup          find and kill orphaned brain processes");
        self.out("");
        self.out(&"Session:".green().to_string());
        self.out("  exit              quit  (also: quit, q, bye, ^D)");
    }

    fn warn_phantom_processes(&self) {
        let others = find_other_brain_pids();
        if !others.is_empty() {
            let pids: Vec<String> = others.iter().map(|p| p.to_string()).collect();
            self.out_fmt(
                format!(
                    "  {} other brain process(es) running: {}",
                    others.len(),
                    pids.join(", ")
                )
                .yellow(),
            );
            self.out(&"  run /cleanup to kill them".dimmed().to_string());
            self.out("");
        }
    }

    fn cleanup_phantom_processes(&self) {
        let others = find_other_brain_pids();
        if others.is_empty() {
            self.out(&"no phantom brain processes found.".green().to_string());
            return;
        }

        self.out_fmt(format!("found {} other brain process(es):", others.len()).yellow());
        for pid in &others {
            let info = std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "pid,etime,args"])
                .output();
            if let Ok(out) = info {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines().skip(1) {
                    self.out(&format!("  {}", line.trim()));
                }
            }
        }
        self.out(
            &"  use `kill <pid>` to stop them manually"
                .dimmed()
                .to_string(),
        );
    }
}

// ─── free helpers ────────────────────────────────────────────────────────────

async fn resolve_model(config: &Config) -> Result<Model> {
    match &config.default_model {
        Model::Claude(id) if !id.is_empty() => return Ok(Model::Claude(id.clone())),
        Model::LMStudio(id) if !id.is_empty() => return Ok(Model::LMStudio(id.clone())),
        _ => {}
    }

    let lms = LMStudioBackend::new(&config.lmstudio_url, "");
    match lms.list_models().await {
        Err(e) => {
            anyhow::bail!(
                "LM Studio not reachable at {}: {e}\nStart the local server in LM Studio.",
                config.lmstudio_url
            );
        }
        Ok(models) => {
            let chat_models: Vec<&str> = models
                .iter()
                .map(|s| s.as_str())
                .filter(|m| !m.contains("embed"))
                .collect();

            if chat_models.is_empty() {
                anyhow::bail!("LM Studio is running but no chat models are loaded.");
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
    }
}

fn estimate_context_window(model: &Model) -> usize {
    match model {
        Model::Claude(id) if id.contains("opus") => 200_000,
        Model::Claude(_) => 200_000,
        Model::LMStudio(id) if id.contains("35b") => 32_768,
        Model::LMStudio(_) => 32_768,
    }
}

fn history_file() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".brain");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("history"))
}

fn check_git_changes(dir: &str) -> Option<usize> {
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

fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn agent_emoji(name: &str) -> &'static str {
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

fn find_other_brain_pids() -> Vec<u32> {
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
