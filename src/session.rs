use crate::backend::{ClaudeBackend, LMStudioBackend, ModelBackend};
use crate::config::{Config, Model};
use crate::context::ProjectContext;
use crate::ledger::TokenLedger;
use crate::tools::ToolRegistry;
use crate::types::{Message, ToolResult};
use anyhow::{Context, Result};
use colored::Colorize;
use rustyline::DefaultEditor;
use std::io::Write;

pub struct Session {
    config: Config,
    backend: Box<dyn ModelBackend>,
    messages: Vec<Message>,
    system_prompt: String,
    ledger: TokenLedger,
    tools: ToolRegistry,
    project: Option<String>,
}

impl Session {
    pub fn new(config: Config, ctx: ProjectContext) -> Result<Self> {
        let system_prompt = ctx.build_system_prompt(&config);
        let project = ctx.project.clone();

        let backend = build_backend(&config, &config.default_model.clone())?;

        Ok(Session {
            system_prompt,
            project,
            backend,
            messages: Vec::new(),
            ledger: TokenLedger::default(),
            tools: ToolRegistry::default(),
            config,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut rl = DefaultEditor::new().context("failed to init readline")?;

        print_banner(
            self.backend.name(),
            self.backend.model_id(),
            self.project.as_deref(),
        );

        loop {
            let prompt = format!("{} ", "›".cyan());
            let readline = rl.readline(&prompt);

            match readline {
                Ok(line) => {
                    let input = line.trim().to_string();
                    if input.is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(&input);

                    if input.starts_with('/') {
                        if let Err(e) = self.handle_command(&input).await {
                            eprintln!("{}", format!("error: {e}").red());
                        }
                    } else {
                        if let Err(e) = self.chat(input).await {
                            eprintln!("{}", format!("error: {e}").red());
                        }
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    println!("{}", "^C".dimmed());
                    continue;
                }
                Err(rustyline::error::ReadlineError::Eof) => {
                    println!("{}", "\nbye.".dimmed());
                    break;
                }
                Err(e) => {
                    eprintln!("{}", format!("readline error: {e}").red());
                    break;
                }
            }
        }

        Ok(())
    }

    async fn chat(&mut self, input: String) -> Result<()> {
        self.messages.push(Message::user(input));

        // Tool call loop: keep sending until the model stops requesting tools
        loop {
            let tools = ToolRegistry::definitions();
            let response = self
                .backend
                .chat(&self.messages, &tools, &self.system_prompt)
                .await?;

            self.ledger.record(response.input_tokens, response.output_tokens);

            let has_tools = !response.tool_calls.is_empty();

            // Record the assistant turn
            self.messages.push(Message::Assistant {
                text: if response.text.is_empty() {
                    None
                } else {
                    Some(response.text.clone())
                },
                tool_calls: response.tool_calls.clone(),
            });

            if !has_tools {
                // Done — print token info and exit the tool loop
                self.ledger.display();
                println!();
                break;
            }

            // Execute all tool calls, collect results
            let mut results: Vec<ToolResult> = Vec::new();
            for tc in &response.tool_calls {
                println!("{}", format!("  input: {}", tc.input).dimmed());
                let mut r = self.tools.execute(&tc.name, &tc.input);
                r.tool_use_id = tc.id.clone();

                // Print result preview (first 200 chars)
                let preview = if r.content.len() > 200 {
                    format!("{}…", &r.content[..200])
                } else {
                    r.content.clone()
                };
                let color = if r.is_error {
                    preview.red().to_string()
                } else {
                    preview.dimmed().to_string()
                };
                println!("{color}");

                results.push(r);
            }

            self.messages.push(Message::ToolResults(results));
            // Loop — send tool results back to the model
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
            "/tokens" => {
                self.ledger.display();
            }
            "/system" => {
                println!("{}", self.system_prompt.dimmed());
            }
            "/escalate" => {
                if parts.len() < 2 {
                    println!("{}", "usage: /escalate <question>".yellow());
                } else {
                    let question = parts[1..].join(" ");
                    self.cmd_escalate(&question).await?;
                }
            }
            "/quit" | "/exit" | "/q" => {
                println!("{}", "bye.".dimmed());
                std::process::exit(0);
            }
            "/help" => {
                print_help();
            }
            _ => {
                println!("{}", format!("unknown command: {}", parts[0]).yellow());
                print_help();
            }
        }
        Ok(())
    }

    async fn cmd_switch_model(&mut self, spec: &str) -> Result<()> {
        let model = Model::parse(spec);
        let new_backend = build_backend(&self.config, &model)?;
        println!(
            "{}",
            format!(
                "switched to {} ({})",
                new_backend.name(),
                new_backend.model_id()
            )
            .green()
        );
        self.backend = new_backend;
        Ok(())
    }

    async fn cmd_list_models(&mut self) -> Result<()> {
        match self.backend.name() {
            "lmstudio" => {
                // Reach into the backend to list models
                // For now, use a fresh client call
                let lms = LMStudioBackend::new(
                    &self.config.lmstudio_url,
                    self.backend.model_id(),
                );
                match lms.list_models().await {
                    Ok(models) if models.is_empty() => {
                        println!("{}", "no models loaded in LM Studio".yellow());
                    }
                    Ok(models) => {
                        println!("{}", "LM Studio models:".green());
                        for (i, m) in models.iter().enumerate() {
                            println!("  {}  {m}", format!("[{i}]").dimmed());
                        }
                    }
                    Err(e) => {
                        println!("{}", format!("LM Studio not reachable: {e}").red());
                    }
                }
            }
            _ => {
                println!("{}", "Claude models (common):".green());
                let models = [
                    "claude-opus-4-6",
                    "claude-sonnet-4-6",
                    "claude-haiku-4-5-20251001",
                ];
                for m in models {
                    let marker = if m == self.backend.model_id() { "●" } else { " " };
                    println!("  {marker} {m}");
                }
            }
        }
        Ok(())
    }

    /// Inline escalation: send a question to Claude, inject the answer as
    /// an assistant message in the current context, continue.
    async fn cmd_escalate(&mut self, question: &str) -> Result<()> {
        let api_key = self
            .config
            .anthropic_api_key
            .clone()
            .context("ANTHROPIC_API_KEY not set — cannot escalate")?;

        println!("{}", "escalating to Claude…".yellow());

        let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
        let escalate_messages = vec![Message::user(question)];
        let response = claude
            .chat(&escalate_messages, &[], &self.system_prompt)
            .await?;

        self.ledger.record(response.input_tokens, response.output_tokens);

        // Inject the answer into the current session context
        self.messages.push(Message::user(format!(
            "[escalated to Claude]\nQuestion: {question}"
        )));
        self.messages.push(Message::Assistant {
            text: Some(response.text),
            tool_calls: vec![],
        });

        self.ledger.display();
        Ok(())
    }
}

fn build_backend(config: &Config, model: &Model) -> Result<Box<dyn ModelBackend>> {
    match model {
        Model::Claude(id) => {
            let key = config
                .anthropic_api_key
                .clone()
                .context("ANTHROPIC_API_KEY not set")?;
            Ok(Box::new(ClaudeBackend::new(key, id)))
        }
        Model::LMStudio(id) => Ok(Box::new(LMStudioBackend::new(
            &config.lmstudio_url,
            id,
        ))),
    }
}

fn print_banner(backend: &str, model: &str, project: Option<&str>) {
    println!();
    if let Some(p) = project {
        println!("{}", format!("  brain  ·  {p}").bold());
    } else {
        println!("{}", "  brain".bold());
    }
    println!("{}", format!("  {backend}:{model}").dimmed());
    println!("{}", "  /help for commands, ^D to quit".dimmed());
    println!();
}

fn print_help() {
    println!("{}", "Commands:".green());
    println!("  /model <spec>     switch model  (e.g. claude-sonnet-4-6, lmstudio:qwen3)");
    println!("  /models           list available models");
    println!("  /escalate <q>     ask Claude, inject answer into context");
    println!("  /clear            clear conversation context");
    println!("  /tokens           show token usage");
    println!("  /system           show current system prompt");
    println!("  /quit             exit");
}
