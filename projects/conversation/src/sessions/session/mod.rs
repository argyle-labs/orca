mod chat;
mod commands;
mod delegate;
mod ui;
pub mod util;

use crate::jobs::JobManager;
use crate::sessions::context::ProjectContext;
use crate::sessions::ledger::TokenLedger;
use crate::sessions::log::SessionLog;
use crate::sessions::tui::{self, TuiAction, TuiApp};
use ::model::tools::ToolRegistry;
use ::model::{
    Message, ModelBackend, OutputSink, build_backend, estimate_context_window, resolve_model,
    sink_write, sink_writeln, stdout_sink,
};
use anyhow::{Context, Result};
use colored::Colorize;
use contract::config::{Config, Model};
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use rustyline::DefaultEditor;
use tokio::sync::mpsc;

pub struct Session {
    pub(super) config: Config,
    pub(super) backend: Box<dyn ModelBackend>,
    pub(super) current_model: Model,
    pub(super) messages: Vec<Message>,
    pub(super) system_prompt: String,
    pub(super) active_agent: String,
    pub(super) ledger: TokenLedger,
    pub(super) tools: ToolRegistry,
    pub(super) project: Option<String>,
    pub(super) log: Option<SessionLog>,
    pub(super) context_window: usize,
    pub(super) narration: bool,
    pub(super) output: OutputSink,
    pub(super) jobs: JobManager,
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
        Self::new_with_output_and_model(config, ctx, output, None).await
    }

    /// Like `new_with_output` but bypasses model auto-discovery when `forced_model`
    /// is `Some` — the caller has already decided which backend serves this session
    /// and any failure to honor it must surface, not fall back.
    pub async fn new_with_output_and_model(
        config: Config,
        ctx: ProjectContext,
        output: OutputSink,
        forced_model: Option<Model>,
    ) -> Result<Self> {
        let project = ctx.project.clone();

        let model = match forced_model {
            Some(m) => m,
            None => resolve_model(&config, None).await?,
        };
        let context_window = estimate_context_window(&model);
        let backend = build_backend(&config, &model)?;

        // Claude and tool-capable backends get the full Wolf persona.
        // Local models that don't support tools get a stripped prompt without
        // the Otter narration and agent routing table, which confuse them.
        let system_prompt = ctx.build_system_prompt_for_backend(&config, !backend.is_local());

        let log = SessionLog::new(project.as_deref(), &config.logs_dir()).ok();

        Ok(Session {
            system_prompt,
            active_agent: "orca".to_string(),
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

    pub(super) fn out(&self, s: &str) {
        sink_writeln(&self.output, s);
    }

    pub(super) fn out_raw(&self, s: &str) {
        sink_write(&self.output, s);
    }

    pub(super) fn out_fmt(&self, s: impl std::fmt::Display) {
        sink_writeln(&self.output, &s.to_string());
    }

    pub fn set_output(&mut self, sink: OutputSink) {
        self.output = sink.clone();
        self.tools.output = sink;
    }

    pub fn enable_tui_mode(&mut self) {
        self.tools.permissions.auto_approve = true;
    }

    pub fn set_agent(&mut self, agent: &str) {
        if let Some(prompt) = agents::resolve::load_agent_prompt(agent, &self.config) {
            self.system_prompt = prompt;
        }
        self.active_agent = agent.to_string();
    }

    // ── Public entry points ──────────────────────────────────────────────────

    pub async fn one_shot(&mut self, prompt: String) -> Result<()> {
        self.chat(prompt).await
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut rl = DefaultEditor::new().context("failed to init readline")?;

        let history_path = util::history_file();
        if let Some(p) = &history_path {
            rl.load_history(p).ok();
        }

        self.print_banner();
        self.warn_phantom_processes();

        loop {
            for note in self.jobs.drain_notifications() {
                self.out(&note);
            }

            let emoji = util::agent_emoji(&self.active_agent);
            let prompt = format!("{emoji} {} {} ", self.active_agent.cyan(), "›".dimmed(),);
            let readline = rl.readline(&prompt);

            match readline {
                Ok(line) => {
                    let input = line.trim().to_string();
                    if input.is_empty() {
                        continue;
                    }
                    _ = rl.add_history_entry(&input);

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

    pub async fn run_tui(&mut self) -> Result<()> {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        self.set_output(tui::tui_sink(out_tx));
        self.enable_tui_mode();

        let mut terminal = tui::setup_terminal()?;

        let prompt_str = format!(
            "{} {} ›",
            util::agent_emoji(&self.active_agent),
            self.active_agent,
        );
        let mut app = TuiApp::new(&prompt_str);

        app.push_line(format!(
            "{} orca · {}",
            util::agent_emoji(&self.active_agent),
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
            terminal.draw(|f| tui::render(f, &app))?;

            while let Ok(chunk) = out_rx.try_recv() {
                app.append(&chunk);
            }

            for note in self.jobs.drain_notifications() {
                app.push_line(note);
            }

            if app.should_quit {
                break;
            }

            if app.busy {
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
                                    while let Ok(chunk) = out_rx.try_recv() {
                                        app.append(&chunk);
                                    }
                                    continue;
                                }

                                app.busy = true;
                                terminal.draw(|f| tui::render(f, &app))?;

                                if input.starts_with('/') {
                                    if let Err(e) = self.handle_command(&input).await {
                                        self.out(&format!("error: {e}"));
                                    }
                                } else if let Err(e) = self.chat(input).await {
                                    self.out(&format!("error: {e}"));
                                }

                                app.busy = false;

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
}
