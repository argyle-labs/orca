mod chat;
mod commands;
mod delegate;
mod ui;
pub mod util;

use brain_core::backend::{
    ModelBackend, OutputSink, build_backend, sink_write, sink_writeln, stdout_sink,
};
use brain_core::tools::ToolRegistry;
use brain_jobs::JobManager;
use brain_utils::config::{Config, Model};
use brain_utils::ledger::TokenLedger;
use brain_utils::log::SessionLog;
use brain_utils::types::Message;
use crate::context::ProjectContext;
use crate::tui::{self, TuiAction, TuiApp};
use anyhow::{Context, Result};
use colored::Colorize;
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
        let system_prompt = ctx.build_system_prompt(&config);
        let project = ctx.project.clone();

        let model = util::resolve_model(&config).await?;
        let context_window = util::estimate_context_window(&model);
        let backend = build_backend(&config, &model)?;

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
        if let Some(prompt) = brain_agents::load_agent_prompt(agent, &self.config.agents_dir()) {
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
                                    let _ = self.handle_command(&input).await;
                                } else {
                                    let _ = self.chat(input).await;
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
