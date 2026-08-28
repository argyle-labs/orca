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

            let emoji = util::agent_emoji(&self.active_agent, &self.config);
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
            util::agent_emoji(&self.active_agent, &self.config),
            self.active_agent,
        );
        let mut app = TuiApp::new(&prompt_str);

        app.push_line(format!(
            "{} orca · {}",
            util::agent_emoji(&self.active_agent, &self.config),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::context::ProjectContext;
    use ::model::buffer_sink;
    use std::sync::{Arc, Mutex};

    /// Config rooted at a throwaway temp dir so any filesystem access (logs,
    /// history, agent resolution) resolves to an empty, isolated directory
    /// rather than the developer's real `~/.orca`. Mirrors `commands.rs`.
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

    /// A Session wired to an in-memory buffer with a forced local model (no
    /// network, no API key). `log` is forced to `None` for a deterministic
    /// "logging inactive" baseline, matching the sibling `commands.rs` harness.
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

    // ── Construction with a forced model ─────────────────────────────────────

    #[tokio::test]
    async fn forced_model_bypasses_discovery_and_sets_defaults() {
        let (s, _buf, _tmp) = test_session().await;
        // A forced LMStudio model is honored verbatim — no auto-discovery.
        assert!(matches!(s.current_model, Model::LMStudio { .. }));
        // Fresh session baselines.
        assert_eq!(s.active_agent, "orca");
        assert!(s.messages.is_empty());
        assert!(!s.narration);
        // Local backends get a nonzero estimated context window.
        assert!(s.context_window > 0);
    }

    // ── Discovery path (forced_model = None) ─────────────────────────────────

    #[tokio::test]
    async fn new_with_output_resolves_default_model_without_network() {
        // A non-empty LMStudio default_model short-circuits resolve_model, so the
        // discovery path (forced_model = None) never touches the network and honors
        // the configured default verbatim.
        colored::control::set_override(false);
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let ctx = ProjectContext::default();
        let (sink, _buf) = buffer_sink();
        let s = Session::new_with_output(config, ctx, sink).await.unwrap();
        assert!(matches!(
            s.current_model,
            Model::LMStudio { ref id, .. } if id == "test-model"
        ));
        assert_eq!(s.active_agent, "orca");
        assert!(s.context_window > 0);
    }

    #[tokio::test]
    async fn new_delegates_to_output_variant() {
        // Session::new is a thin delegator to new_with_output(stdout_sink); it must
        // still produce a fully-formed session off the resolved default model.
        colored::control::set_override(false);
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let ctx = ProjectContext::default();
        let s = Session::new(config, ctx).await.unwrap();
        assert!(matches!(s.current_model, Model::LMStudio { .. }));
        assert_eq!(s.active_agent, "orca");
        assert!(s.messages.is_empty());
    }

    // ── one_shot delegates to chat and records the user turn ──────────────────

    #[tokio::test]
    async fn one_shot_pushes_user_message_and_surfaces_backend_error() {
        // The forced LMStudio backend points at an unreachable URL, so the chat
        // round-trip fails — but one_shot must have appended the user turn before
        // the backend was ever consulted, and the error propagates rather than
        // being swallowed.
        let (mut s, _buf, _tmp) = test_session().await;
        let err = s.one_shot("hello there".to_string()).await;
        assert!(err.is_err(), "unreachable backend must surface an error");
        assert_eq!(
            s.messages.len(),
            1,
            "user turn recorded before backend call"
        );
        assert!(matches!(&s.messages[0], Message::User { .. }));
    }

    // ── Output helpers ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn out_appends_newline_out_raw_does_not() {
        let (s, buf, _tmp) = test_session().await;
        s.out("line-one");
        s.out_raw("raw-no-newline");
        let text = output(&buf);
        assert!(
            text.contains("line-one\n"),
            "out must append newline: {text:?}"
        );
        // out_raw writes its argument without a trailing newline appended by us.
        assert!(
            text.ends_with("raw-no-newline"),
            "out_raw must not append: {text:?}"
        );
    }

    #[tokio::test]
    async fn out_fmt_renders_display_with_newline() {
        let (s, buf, _tmp) = test_session().await;
        s.out_fmt(42);
        assert_eq!(output(&buf), "42\n");
    }

    // ── set_output rewires both the session and tool sinks ───────────────────

    #[tokio::test]
    async fn set_output_redirects_subsequent_writes() {
        let (mut s, first_buf, _tmp) = test_session().await;
        let (new_sink, new_buf) = buffer_sink();
        s.set_output(new_sink);

        s.out("after-redirect");
        // The new buffer receives the write; the original does not.
        assert!(output(&new_buf).contains("after-redirect"));
        assert!(!output(&first_buf).contains("after-redirect"));
        // The tool registry's sink was rewired to the same target.
        ::model::sink_writeln(&s.tools.output, "tool-line");
        assert!(output(&new_buf).contains("tool-line"));
    }

    // ── enable_tui_mode flips auto-approve on the permission gate ─────────────

    #[tokio::test]
    async fn enable_tui_mode_sets_auto_approve() {
        let (mut s, _buf, _tmp) = test_session().await;
        assert!(!s.tools.permissions.auto_approve);
        s.enable_tui_mode();
        assert!(s.tools.permissions.auto_approve);
    }

    // ── set_agent updates the active agent name ───────────────────────────────

    // ── run() drives the readline loop; a non-interactive stdin yields EOF ────

    #[tokio::test]
    async fn run_prints_banner_then_exits_on_eof() {
        // Under nextest stdin is not a TTY, so rustyline's first readline returns
        // EOF, which drives the loop straight through the banner and the graceful
        // "bye." exit branch. run() must return Ok and the banner output must have
        // been emitted to the session sink.
        let (mut s, buf, _tmp) = test_session().await;
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), s.run()).await;
        let ret = res.expect("run() must not hang on a non-interactive stdin");
        assert!(ret.is_ok(), "run() should exit cleanly on EOF: {ret:?}");
        let text = output(&buf);
        // The banner names the active agent; the EOF branch prints a "bye." line.
        assert!(
            text.contains("orca") || text.contains("bye"),
            "run() must emit banner/exit output: {text:?}"
        );
    }

    // ── set_agent updates the active agent name ───────────────────────────────

    #[tokio::test]
    async fn set_agent_updates_active_agent_name() {
        let (mut s, _buf, _tmp) = test_session().await;
        // The temp config has no agent roster, so load_agent_prompt returns None
        // and the system prompt is left untouched — but the active agent name is
        // always updated regardless of prompt resolution.
        let before_prompt = s.system_prompt.clone();
        s.set_agent("wolf");
        assert_eq!(s.active_agent, "wolf");
        assert_eq!(
            s.system_prompt, before_prompt,
            "no roster → prompt unchanged"
        );
    }

    /// Seed an active profile whose `agents/<name>.md` exists, so
    /// `set_agent` resolves a prompt and swaps the system prompt (the `Some`
    /// arm of `load_agent_prompt`, uncovered by the no-roster path above).
    fn seed_agent(config: &Config, name: &str, body: &str) {
        let conn = db::open(&config.db_path).unwrap();
        let mgr = namespace::NamespaceManager::from_config(config);
        let profile = mgr
            .create(&conn, contract::config::LOCAL_USER, "test-profile", None)
            .unwrap();
        mgr.set_active(&conn, contract::config::LOCAL_USER, &profile.id)
            .unwrap();
        let agents_dir = profile.agents_dir();
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join(format!("{name}.md")),
            format!("---\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn set_agent_swaps_system_prompt_when_roster_resolves() {
        let (mut s, _buf, _tmp) = test_session().await;
        seed_agent(&s.config, "wolf", "You are Wolf, the orchestrator.");
        let before = s.system_prompt.clone();
        s.set_agent("wolf");
        assert_eq!(s.active_agent, "wolf");
        assert_ne!(s.system_prompt, before, "resolved roster replaces prompt");
        assert!(
            s.system_prompt.contains("You are Wolf, the orchestrator."),
            "prompt body loaded from the agent file: {:?}",
            s.system_prompt
        );
    }
}
