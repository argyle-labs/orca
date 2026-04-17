use crate::backend::{ClaudeBackend, LMStudioBackend, ModelBackend};
use crate::config::{Config, Model};
use crate::context::ProjectContext;
use crate::ledger::TokenLedger;
use crate::log::SessionLog;
use crate::tools::ToolRegistry;
use crate::types::{Message, ToolResult};
use anyhow::{Context, Result};
use colored::Colorize;
use rustyline::DefaultEditor;
use tokio_util::sync::CancellationToken;

pub struct Session {
    config: Config,
    backend: Box<dyn ModelBackend>,
    messages: Vec<Message>,
    system_prompt: String,
    active_agent: String,
    ledger: TokenLedger,
    tools: ToolRegistry,
    project: Option<String>,
    log: Option<SessionLog>,
    context_window: usize,
    narration: bool,
}

impl Session {
    pub async fn new(config: Config, ctx: ProjectContext) -> Result<Self> {
        let system_prompt = ctx.build_system_prompt(&config);
        let project = ctx.project.clone();

        let model = resolve_model(&config).await?;
        let context_window = estimate_context_window(&model);
        let backend = build_backend(&config, &model)?;

        // Start Pinky log
        let log = SessionLog::new(
            project.as_deref(),
            &config.logs_dir(),
        )
        .ok(); // non-fatal — if logging fails, session continues

        Ok(Session {
            system_prompt,
            active_agent: "brain".to_string(),
            project,
            backend,
            messages: Vec::new(),
            ledger: TokenLedger::default(),
            tools: ToolRegistry::default(),
            context_window,
            log,
            narration: true,
            config,
        })
    }

    pub async fn one_shot(&mut self, prompt: String) -> Result<()> {
        self.chat(prompt).await
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut rl = DefaultEditor::new().context("failed to init readline")?;

        // Persist history across sessions
        let history_path = history_file();
        if let Some(p) = &history_path {
            rl.load_history(p).ok();
        }

        print_banner(
            self.backend.name(),
            self.backend.model_id(),
            self.project.as_deref(),
            &self.active_agent,
        );

        // Warn about phantom brain processes on startup
        warn_phantom_processes();

        loop {
            let emoji = agent_emoji(&self.active_agent);
            let prompt = format!("{emoji} {} {} ",
                self.active_agent.cyan(),
                "›".dimmed(),
            );
            let readline = rl.readline(&prompt);

            match readline {
                Ok(line) => {
                    let input = line.trim().to_string();
                    if input.is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(&input);

                    // Bare commands — no slash needed
                    if matches!(input.as_str(), "exit" | "quit" | "q" | "bye") {
                        println!("{}", "bye.".dimmed());
                        if let Some(p) = &history_path {
                            rl.save_history(p).ok();
                        }
                        break;
                    }

                    let result = match input.as_str() {
                        "help" => { print_help(); Ok(()) }
                        "clear" => {
                            self.messages.clear();
                            println!("{}", "context cleared.".dimmed());
                            Ok(())
                        }
                        _ if input.starts_with('/') => {
                            self.handle_command(&input).await
                        }
                        _ => {
                            self.chat(input).await
                        }
                    };

                    if let Err(e) = result {
                        if e.to_string() == "__exit__" {
                            if let Some(p) = &history_path {
                                rl.save_history(p).ok();
                            }
                            break;
                        }
                        eprintln!("{}", format!("error: {e}").red());
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    println!("{}", "^C".dimmed());
                    continue;
                }
                Err(rustyline::error::ReadlineError::Eof) => {
                    println!("{}", "\nbye.".dimmed());
                    if let Some(p) = &history_path {
                        rl.save_history(p).ok();
                    }
                    break;
                }
                Err(e) => {
                    eprintln!("{}", format!("readline error: {e}").red());
                    if let Some(p) = &history_path {
                        rl.save_history(p).ok();
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    async fn chat(&mut self, input: String) -> Result<()> {
        // Log user message
        if let Some(log) = &mut self.log {
            log.append("user", &self.active_agent.clone(), &input, &[]).ok();
        }

        self.messages.push(Message::user(input));

        loop {
            let tools = ToolRegistry::definitions();
            let cancel = CancellationToken::new();
            let cancel_clone = cancel.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                cancel_clone.cancel();
            });
            let response = self
                .backend
                .chat(&self.messages, &tools, &self.system_prompt, cancel)
                .await?;

            self.ledger.record(response.input_tokens, response.output_tokens);

            let has_tools = !response.tool_calls.is_empty();

            // Log assistant response
            if !response.text.trim().is_empty() && let Some(log) = &mut self.log {
                log.append("assistant", &self.active_agent.clone(), &response.text, &[]).ok();
            }

            self.messages.push(Message::Assistant {
                text: if response.text.is_empty() { None } else { Some(response.text.clone()) },
                tool_calls: response.tool_calls.clone(),
            });

            if !has_tools {
                self.ledger.display();
                self.check_commit_status();
                println!();
                break;
            }

            // Execute tool calls
            let mut results: Vec<ToolResult> = Vec::new();
            for tc in &response.tool_calls {
                println!("{}", format!("  input: {}", tc.input).dimmed());

                let mut r = if tc.name == "delegate" {
                    self.execute_delegate(&tc.input).await
                } else if tc.name == "confirm" {
                    execute_confirm(&tc.input)
                } else {
                    self.tools.execute(&tc.name, &tc.input)
                };
                r.tool_use_id = tc.id.clone();

                let preview = if r.content.len() > 200 {
                    format!("{}…", &r.content[..200])
                } else {
                    r.content.clone()
                };
                println!("{}", if r.is_error { preview.red().to_string() } else { preview.dimmed().to_string() });

                results.push(r);
            }

            self.messages.push(Message::ToolResults(results));
        }

        Ok(())
    }

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
            "/models" => {
                self.cmd_list_models().await?;
            }
            "/clear" => {
                self.messages.clear();
                println!("{}", "context cleared.".dimmed());
            }
            "/tokens" | "/t" => {
                self.ledger.display();
            }
            "/context" | "/ctx" => {
                self.cmd_context();
            }
            "/system" => {
                println!("{}", self.system_prompt.dimmed());
            }
            "/agent" => {
                println!("{}", "you're talking to Brain.".cyan());
            }
            "/flag" => {
                let note = parts.get(1).copied().unwrap_or("flagged as important");
                if let Some(log) = &mut self.log {
                    log.flag_last(note).ok();
                    println!("{}", format!("flagged: {note}").green());
                } else {
                    println!("{}", "logging not active".yellow());
                }
            }
            "/log" => {
                if let Some(log) = &self.log {
                    println!("{}", format!("session: {}", log.session_id()).dimmed());
                    println!("{}", format!("file: {}", log.path().display()).dimmed());
                } else {
                    println!("{}", "logging not active".yellow());
                }
            }
            "/search" => {
                if parts.len() < 2 {
                    println!("{}", "usage: /search <query>".yellow());
                } else {
                    let query = parts[1..].join(" ");
                    self.cmd_search_logs(&query);
                }
            }
            "/sessions" => {
                self.cmd_list_sessions();
            }
            "/recall" => {
                if parts.len() < 2 {
                    println!("{}", "usage: /recall <session_id or partial>".yellow());
                } else {
                    self.cmd_recall_session(parts[1]);
                }
            }
            "/escalate" => {
                if parts.len() < 2 {
                    println!("{}", "usage: /escalate <question>".yellow());
                } else {
                    let question = parts[1..].join(" ");
                    self.cmd_escalate(&question).await?;
                }
            }
            "/narration" => {
                self.narration = !self.narration;
                let state = if self.narration { "on" } else { "off" };
                println!("{}", format!("narration: {state}").green());
            }
            "/cleanup" => {
                cleanup_phantom_processes();
            }
            "/quit" | "/exit" | "/q" => {
                println!("{}", "bye.".dimmed());
                anyhow::bail!("__exit__");
            }
            "/help" | "/h" => {
                print_help();
            }
            _ => {
                println!("{}", format!("unknown command: {}", parts[0]).yellow());
                println!("{}", "  type /help or help for commands".dimmed());
            }
        }
        Ok(())
    }



    fn cmd_context(&self) {
        let msg_count = self.messages.len();
        let turns = self.messages.iter().filter(|m| matches!(m, Message::User { .. })).count();
        let session_tokens = self.ledger.session_input + self.ledger.session_output;
        let pct = if self.context_window > 0 {
            (session_tokens as f64 / self.context_window as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        println!("{}", "Context:".green());
        println!("  active agent:  @{}", self.active_agent);
        println!("  model:         {}:{}", self.backend.name(), self.backend.model_id());
        println!("  messages:      {} ({} turns)", msg_count, turns);
        println!("  tokens (est):  {} / {} ({:.0}%)",
            fmt_tokens(session_tokens),
            fmt_tokens(self.context_window as u32),
            pct,
        );
        if let Some(log) = &self.log {
            println!("  session log:   {}", log.session_id());
        }
        if pct > 75.0 {
            println!("{}", "  ⚠ context over 75% — consider /clear or starting a new session".yellow());
        }
    }

    async fn cmd_switch_model(&mut self, spec: &str) -> Result<()> {
        let model = Model::parse(spec);
        let new_backend = build_backend(&self.config, &model)?;
        self.context_window = estimate_context_window(&model);
        println!("{}", format!("switched to {}:{}", new_backend.name(), new_backend.model_id()).green());
        self.backend = new_backend;
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
            Err(_) => println!("{}", "  LM Studio: not reachable".dimmed()),
        }

        if self.config.anthropic_api_key.is_some() {
            for m in ["claude-sonnet-4-6", "claude-opus-4-6", "claude-haiku-4-5-20251001"] {
                all.push((format!("claude:{m}"), m.to_string()));
            }
        } else {
            println!("{}", "  Claude: no API key (run `brain login`)".dimmed());
        }

        if all.is_empty() {
            println!("{}", "no models available".yellow());
            return Ok(());
        }

        println!("{}", "Switch model:".green());
        for (i, (display, _)) in all.iter().enumerate() {
            let marker = if current.ends_with(display.trim_start_matches("lmstudio:").trim_start_matches("claude:"))
                || current == *display { "●" } else { " " };
            println!("  {} {}  {display}", marker, format!("[{}]", i + 1).dimmed());
        }
        print!("{} ", "[enter to cancel]:".cyan());
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
        let trimmed = input.trim();
        if !trimmed.is_empty()
            && let Ok(idx) = trimmed.parse::<usize>()
            && idx > 0
            && let Some((_, spec)) = all.get(idx - 1) {
            self.cmd_switch_model(spec).await?;
        }
        Ok(())
    }

    async fn cmd_escalate(&mut self, question: &str) -> Result<()> {
        let api_key = self.config.anthropic_api_key.clone()
            .context("no API key — run `brain login` first")?;

        println!("{}", "↑ escalating to Claude…".yellow());

        let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
        let msgs = vec![Message::user(question)];
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel_clone.cancel();
        });
        let response = claude.chat(&msgs, &[], &self.system_prompt, cancel).await?;

        self.ledger.record(response.input_tokens, response.output_tokens);

        if let Some(log) = &mut self.log {
            log.append("user", "escalate", question, &["escalation"]).ok();
            log.append("assistant", "claude-sonnet-4-6", &response.text, &["escalation"]).ok();
        }

        self.messages.push(Message::user(format!("[escalated to Claude]\nQuestion: {question}")));
        self.messages.push(Message::Assistant { text: Some(response.text), tool_calls: vec![] });

        self.ledger.display();
        Ok(())
    }

    fn cmd_search_logs(&self, query: &str) {
        let logs_dir = self.config.logs_dir();
        match crate::log::search_logs(&logs_dir, query, 20) {
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
                    println!("  {} {} @{} {}{}", session.dimmed(), role.cyan(), agent, preview, flag.yellow());
                }
            }
            Err(e) => println!("{}", format!("search error: {e}").red()),
        }
    }

    fn cmd_list_sessions(&self) {
        let logs_dir = self.config.logs_dir();
        match crate::log::list_sessions(&logs_dir, 15) {
            Ok(sessions) if sessions.is_empty() => {
                println!("{}", "no sessions found".dimmed());
            }
            Ok(sessions) => {
                println!("{}", "Recent sessions:".green());
                for s in &sessions {
                    let flag = if s.flagged > 0 { format!(" (★ {})", s.flagged) } else { String::new() };
                    println!("  {}  {} msgs{}", s.session_id.dimmed(), s.messages, flag.yellow());
                }
            }
            Err(e) => println!("{}", format!("error: {e}").red()),
        }
    }

    fn cmd_recall_session(&self, session_id: &str) {
        let logs_dir = self.config.logs_dir();
        match crate::log::recall_session(&logs_dir, session_id) {
            Ok(records) => {
                println!("{}", format!("session: {} ({} records)", session_id, records.len()).green());
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
                    let ellipsis = if content.len() > 200 { "…" } else { "" };
                    println!("  {} {}{}{}", prefix.cyan(), preview, ellipsis, flag.yellow());
                }
            }
            Err(e) => println!("{}", format!("error: {e}").red()),
        }
    }

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

        // Load agent system prompt (filesystem first, embedded fallback)
        let agent_prompt = match crate::agents::load_agent_prompt(agent, &self.config.agents_dir()) {
            Some(prompt) => prompt,
            None => {
                return ToolResult {
                    tool_use_id: String::new(),
                    content: format!("error: agent @{agent} not found"),
                    is_error: true,
                };
            }
        };

        // Brain narrates to Pinky
        let agent_icon = agent_emoji(agent);
        if self.narration {
            println!();
            println!("{}", format!(
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
                    "scribe" => "Scribe will check the documentation.",
                    "lynx" => "Lynx will plan the most efficient path.",
                    "smith" => "Smith will inspect and repair the agent definitions.",
                    "raven" => "Raven will capture this in the vault.",
                    "pinky" => "Pinky will search the session logs. NARF!",
                    "magpie" => "Magpie will check for scope graduation candidates.",
                    "oracle" => "Oracle will judge whether to escalate.",
                    "boar" => "Boar will charge through the carl commands.",
                    _ => "This specialist knows what to do.",
                }
            ).dimmed());
            println!("{}", format!(
                "  🐭 Pinky: \"Ooh! {agent_icon} @{agent}! NARF! I'll write everything down!\""
            ).dimmed());
            println!();
        }

        // Log the delegation
        if let Some(log) = &mut self.log {
            log.append("system", &self.active_agent, &format!("delegated to @{agent}: {task}"), &["delegation"]).ok();
        }

        // Specialist tools — everything except delegate (no recursion)
        let specialist_tools: Vec<_> = ToolRegistry::definitions()
            .into_iter()
            .filter(|t| t.name != "delegate")
            .collect();

        // Run sub-conversation with full tool loop
        let mut sub_messages = vec![Message::user(task)];
        let mut full_response = String::new();
        let max_rounds = 20; // safety limit

        println!("{}", format!("  ┌─ {agent_icon} @{agent} ────────────────────────────").cyan());

        for round in 0..max_rounds {
            let cancel = CancellationToken::new();
            let cancel_clone = cancel.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                cancel_clone.cancel();
            });

            print!("{}", "  │ ".cyan());
            let result = self.backend.chat(
                &sub_messages, &specialist_tools, &agent_prompt, cancel
            ).await;

            let response = match result {
                Ok(r) => r,
                Err(e) => {
                    println!("{}", format!("  └─ @{agent} error ──────────────────────").red());
                    return ToolResult {
                        tool_use_id: String::new(),
                        content: format!("delegation error: {e}"),
                        is_error: true,
                    };
                }
            };

            self.ledger.record(response.input_tokens, response.output_tokens);

            if !response.text.is_empty() {
                full_response.push_str(&response.text);

                // Log specialist response
                if let Some(log) = &mut self.log {
                    log.append("assistant", agent, &response.text, &["delegation"]).ok();
                }
            }

            sub_messages.push(Message::Assistant {
                text: if response.text.is_empty() { None } else { Some(response.text.clone()) },
                tool_calls: response.tool_calls.clone(),
            });

            // No tool calls = specialist is done
            if response.tool_calls.is_empty() {
                break;
            }

            // Execute specialist's tool calls
            let mut tool_results: Vec<ToolResult> = Vec::new();
            for tc in &response.tool_calls {
                print!("{}", "  │ ".cyan());
                println!("{}", format!("⚙ {} {}", tc.name, tc.input).dimmed());
                let mut r = self.tools.execute(&tc.name, &tc.input);
                r.tool_use_id = tc.id.clone();

                // Log tool usage
                if let Some(log) = &mut self.log {
                    log.append("tool", agent, &format!("{}({})", tc.name, tc.input), &["delegation"]).ok();
                }

                let preview = if r.content.len() > 200 {
                    format!("{}…", &r.content[..200])
                } else {
                    r.content.clone()
                };
                print!("{}", "  │ ".cyan());
                println!("{}", if r.is_error { preview.red().to_string() } else { preview.dimmed().to_string() });

                tool_results.push(r);
            }

            sub_messages.push(Message::ToolResults(tool_results));

            if round == max_rounds - 1 {
                println!("{}", "  │ (max rounds reached)".yellow());
            }
        }

        println!("{}", format!("  └─ {agent_icon} @{agent} done ──────────────────────").cyan());
        if self.narration {
            // Varied post-delegation dialogue based on agent
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
                "ferret" => (
                    format!("\"Good eye, {agent_icon} Ferret. Standards matter.\""),
                    "\"Ferret found ALL the things! Every single one! ZORT!\"".to_string(),
                ),
                "owl" => (
                    format!("\"Clear explanation, {agent_icon} Owl. Wisdom earned.\""),
                    "\"I understood some of those words, Brain! NARF!\"".to_string(),
                ),
                "spider" => (
                    format!("\"Elegant simplification, {agent_icon} Spider. Less is more.\""),
                    "\"The web is so pretty now! POIT!\"".to_string(),
                ),
                "scribe" => (
                    format!("\"Good catch, {agent_icon} Scribe. Documentation is truth.\""),
                    "\"Words! So many words! I'll file them all! TROZ!\"".to_string(),
                ),
                "smith" => (
                    format!("\"Good work, {agent_icon} Smith. The tools are sharper now.\""),
                    "\"Smith fixed the things that fix the things! ZORT!\"".to_string(),
                ),
                _ => (
                    format!("\"Thank you, {agent_icon} @{agent}. Pinky, did you get all that?\""),
                    "\"Every word, Brain! POIT!\"".to_string(),
                ),
            };
            println!();
            println!("{}", format!("  🧠 Brain: {brain_line}").dimmed());
            println!("{}", format!("  🐭 Pinky: {pinky_line}").dimmed());
            println!();
        }

        ToolResult {
            tool_use_id: String::new(),
            content: full_response,
            is_error: false,
        }
    }

    fn check_commit_status(&self) {
        let cwd = std::env::current_dir().ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        if let Some(dir) = cwd && let Some(count) = check_git_changes(&dir) && count >= 5 {
            println!("{}", format!(
                "⚠  {} uncommitted files in {} — good time to commit",
                count,
                dir.split('/').next_back().unwrap_or(&dir)
            ).yellow());
        }
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────────

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
            let chat_models: Vec<&str> = models.iter()
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
            let selected = chat_models.get(choice.saturating_sub(1)).unwrap_or(&chat_models[0]);
            Ok(Model::LMStudio(selected.to_string()))
        }
    }
}

fn build_backend(config: &Config, model: &Model) -> Result<Box<dyn ModelBackend>> {
    match model {
        Model::Claude(id) => {
            let key = config.anthropic_api_key.clone()
                .context("no API key — run `brain login` to store one")?;
            Ok(Box::new(ClaudeBackend::new(key, id)))
        }
        Model::LMStudio(id) => Ok(Box::new(LMStudioBackend::new(&config.lmstudio_url, id))),
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
        .output().ok()?;
    if !output.status.success() { return None; }
    let count = String::from_utf8_lossy(&output.stdout)
        .lines().filter(|l| !l.trim().is_empty()).count();
    if count == 0 { None } else { Some(count) }
}

fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
    else if n >= 1_000 { format!("{:.1}k", n as f64 / 1_000.0) }
    else { n.to_string() }
}

fn execute_confirm(input: &serde_json::Value) -> ToolResult {
    let question = input["question"].as_str().unwrap_or("Proceed?");

    println!("{}", question.cyan());
    println!("  {}  yes", "[1]".dimmed());
    println!("  {}  no", "[2]".dimmed());
    print!("{} ", "[1]:".cyan());
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut buf = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut buf).ok();

    let answer = if buf.trim() == "2" { "no" } else { "yes" };
    ToolResult {
        tool_use_id: String::new(),
        content: answer.to_string(),
        is_error: false,
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
        "scribe" => "✍️",
        "smith" => "🔨",
        _ => "🔧",
    }
}

fn print_banner(backend: &str, model: &str, project: Option<&str>, agent: &str) {
    println!();
    let emoji = agent_emoji(agent);
    if let Some(p) = project {
        println!("{}", format!("  {emoji} brain  ·  {p}").bold());
    } else {
        println!("{}", format!("  {emoji} brain").bold());
    }
    println!("  {}  {}", format!("{emoji} @{agent}").cyan(), format!("{backend}:{model}").dimmed());
    println!("{}", "  /help · exit to quit".dimmed());
    println!();
}

fn print_help() {
    println!("{}", "Navigation:".green());
    println!("  /model            list models + interactive picker");
    println!("  /model <spec>     switch directly  (lmstudio:qwen3, claude-sonnet-4-6)");
    println!("  clear             clear conversation history");
    println!();
    println!("{}", "Context:".green());
    println!("  /context          show messages, token usage, context window %");
    println!("  /tokens           token ledger");
    println!("  /agent            show active agent");
    println!("  /system           show current system prompt");
    println!();
    println!("{}", "Logging (Pinky):".green());
    println!("  /flag [note]      mark last message as important");
    println!("  /log              show current session log path");
    println!("  /search <query>   search all session logs for a keyword");
    println!("  /sessions         list recent sessions");
    println!("  /recall <id>      replay a session's messages");
    println!();
    println!("{}", "Escalation:".green());
    println!("  /escalate <q>     send question to Claude, inject answer into context");
    println!();
    println!("{}", "Preferences:".green());
    println!("  /narration        toggle Brain/Pinky narration on/off");
    println!();
    println!("{}", "Maintenance:".green());
    println!("  /cleanup          find and kill orphaned brain processes");
    println!();
    println!("{}", "Session:".green());
    println!("  exit              quit  (also: quit, q, bye, ^D)");
}

fn find_other_brain_pids() -> Vec<u32> {
    let my_pid = std::process::id();
    let output = std::process::Command::new("pgrep")
        .args(["-x", "brain"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .filter(|&pid| pid != my_pid)
                .collect()
        }
        _ => vec![],
    }
}

fn warn_phantom_processes() {
    let others = find_other_brain_pids();
    if !others.is_empty() {
        let pids: Vec<String> = others.iter().map(|p| p.to_string()).collect();
        println!("{}", format!(
            "  {} other brain process(es) running: {}",
            others.len(),
            pids.join(", ")
        ).yellow());
        println!("{}", "  run /cleanup to kill them".dimmed());
        println!();
    }
}

fn cleanup_phantom_processes() {
    let others = find_other_brain_pids();
    if others.is_empty() {
        println!("{}", "no phantom brain processes found.".green());
        return;
    }

    println!("{}", format!("found {} other brain process(es):", others.len()).yellow());
    for pid in &others {
        // Show what the process is doing
        let info = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid,etime,args"])
            .output();
        if let Ok(out) = info {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines().skip(1) {
                println!("  {}", line.trim());
            }
        }
    }

    println!("{}", "Kill them?".cyan());
    println!("  {}  no", "[1]".dimmed());
    println!("  {}  yes, kill all", "[2]".dimmed());
    print!("{} ", "[1]:".cyan());
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut kill_input = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut kill_input).ok();

    if kill_input.trim() == "2" {
        for pid in &others {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        println!("{}", format!("killed {} process(es).", others.len()).green());
    } else {
        println!("{}", "skipped.".dimmed());
    }
}
