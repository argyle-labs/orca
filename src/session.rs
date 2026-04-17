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
            let prompt = format!("{} {} ",
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
                    let result = match input.as_str() {
                        "exit" | "quit" | "q" | "bye" => {
                            println!("{}", "bye.".dimmed());
                            if let Some(p) = &history_path {
                                rl.save_history(p).ok();
                            }
                            std::process::exit(0);
                        }
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
            if !response.text.is_empty() && let Some(log) = &mut self.log {
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
                let mut r = self.tools.execute(&tc.name, &tc.input);
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
            "/escalate" => {
                if parts.len() < 2 {
                    println!("{}", "usage: /escalate <question>".yellow());
                } else {
                    let question = parts[1..].join(" ");
                    self.cmd_escalate(&question).await?;
                }
            }
            "/cleanup" => {
                cleanup_phantom_processes();
            }
            "/quit" | "/exit" | "/q" => {
                println!("{}", "bye.".dimmed());
                std::process::exit(0);
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

        println!("{}", "Available models:".green());
        for (i, (display, _)) in all.iter().enumerate() {
            let active = if current.ends_with(display.trim_start_matches("lmstudio:").trim_start_matches("claude:"))
                || current == *display { "●" } else { " " };
            println!("  {} {}  {display}", active, format!("[{i}]").dimmed());
        }
        print!("{} ", "switch to [enter to cancel]:".cyan());
        std::io::Write::flush(&mut std::io::stdout())?;

        let mut input = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if let Ok(idx) = trimmed.parse::<usize>() {
            if let Some((_, spec)) = all.get(idx) {
                self.cmd_switch_model(spec).await?;
            }
        } else {
            self.cmd_switch_model(trimmed).await?;
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

    fn check_commit_status(&self) {
        let cwd = std::env::current_dir().ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        if let Some(dir) = cwd && let Some(count) = check_git_changes(&dir) {
            if count >= 5 {
                println!("{}", format!(
                    "⚠  {} uncommitted files in {} — good time to commit",
                    count,
                    dir.split('/').next_back().unwrap_or(&dir)
                ).yellow());
            }
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
                println!("  {}  {m}", format!("[{i}]").dimmed());
            }
            print!("{} ", "model [0]:".cyan());
            std::io::Write::flush(&mut std::io::stdout())?;
            let mut input = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
            let choice: usize = input.trim().parse().unwrap_or(0);
            let selected = chat_models.get(choice).unwrap_or(&chat_models[0]);
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

fn print_banner(backend: &str, model: &str, project: Option<&str>, agent: &str) {
    println!();
    if let Some(p) = project {
        println!("{}", format!("  brain  ·  {p}").bold());
    } else {
        println!("{}", "  brain".bold());
    }
    println!("  {}  {}", format!("@{agent}").cyan(), format!("{backend}:{model}").dimmed());
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
    println!("{}", "Logging:".green());
    println!("  /flag [note]      mark last message as important in session log");
    println!("  /log              show current session log path");
    println!();
    println!("{}", "Escalation:".green());
    println!("  /escalate <q>     send question to Claude, inject answer into context");
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

    print!("{} ", "kill them? [y/N]:".cyan());
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut input = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input).ok();
    if input.trim().eq_ignore_ascii_case("y") {
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
