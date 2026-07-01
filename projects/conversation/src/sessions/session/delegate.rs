// Session delegation; HashMap/Value appear in delegated tool calls as protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use super::{Session, util};
use ::model::Message;
use ::model::tools::ToolRegistry;
use colored::Colorize;
use contract::ToolResult;
use tokio_util::sync::CancellationToken;

impl Session {
    pub(super) async fn execute_delegate(&mut self, input: &serde_json::Value) -> ToolResult {
        let agent = input["agent"].as_str().unwrap_or("");
        let task = input["task"].as_str().unwrap_or("");

        if agent.is_empty() || task.is_empty() {
            return ToolResult {
                tool_use_id: String::new(),
                content: "error: agent and task are required".into(),
                is_error: true,
            };
        }

        let agent_prompt = match agents::resolve::load_agent_prompt(agent, &self.config) {
            Some(prompt) => prompt,
            None => {
                return ToolResult {
                    tool_use_id: String::new(),
                    content: format!("error: agent @{agent} not found"),
                    is_error: true,
                };
            }
        };

        let agent_icon = util::agent_emoji(agent);
        if self.narration {
            self.out("");
            self.out_fmt(
                format!(
                    "  🐳 Orca: \"Otter, I'm sending this to {agent_icon} @{agent}. {}\"",
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
                        "otter" => "Otter will search the session logs.",
                        "boar" => "Boar will charge through the carl commands.",
                        _ => "This specialist knows what to do.",
                    }
                )
                .dimmed(),
            );
            self.out_fmt(
                format!("  🦦 Otter: \"Ooh! {agent_icon} @{agent}!  I'll write everything down!\"")
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

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let ctrl_c_task = tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel_clone.cancel();
        });

        for round in 0..max_rounds {
            self.out_raw(&"  │ ".cyan().to_string());
            let result = self
                .backend
                .chat(
                    &sub_messages,
                    &specialist_tools,
                    &agent_prompt,
                    cancel.child_token(),
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
                let r = self.tools.execute(tc.id.clone(), &tc.name, &tc.input).await;

                if let Some(log) = &mut self.log {
                    log.append(
                        "tool",
                        agent,
                        &format!("{}({})", tc.name, tc.input),
                        &["delegation"],
                    )
                    .ok();
                }

                let summary = util::summarize_result(&tc.name, &r.content, r.is_error);
                self.out_raw(&"  │ ".cyan().to_string());
                if r.is_error {
                    self.out(&summary.red().to_string());
                } else {
                    self.out(&summary.dimmed().to_string());
                }

                tool_results.push(r);
            }

            sub_messages.push(Message::ToolResults(tool_results));

            if round == max_rounds - 1 {
                self.out(&"  │ (max rounds reached)".yellow().to_string());
            }
        }
        ctrl_c_task.abort();

        self.out_fmt(format!("  └─ {agent_icon} @{agent} done ──────────────────────").cyan());
        if self.narration {
            let (orca_line, otter_line) = match agent {
                "fox" => (
                    format!("\"Excellent work, {agent_icon} Fox. The trail was well-traced.\""),
                    "\"Ooh! Was it a mystery? I love mysteries! \"".to_string(),
                ),
                "bear" => (
                    format!("\"Thorough as always, {agent_icon} Bear. Nothing escapes you.\""),
                    "\"Bear is thorough, Orca! \"".to_string(),
                ),
                "crow" => (
                    format!("\"Clean implementation, {agent_icon} Crow. Well built.\""),
                    "\"Ooh! New code! Can I name a variable? \"".to_string(),
                ),
                _ => (
                    format!("\"Thank you, {agent_icon} @{agent}. Otter, did you get all that?\""),
                    "\"Every word, Orca! \"".to_string(),
                ),
            };
            self.out("");
            self.out_fmt(format!("  🐳 Orca: {orca_line}").dimmed());
            self.out_fmt(format!("  🦦 Otter: {otter_line}").dimmed());
            self.out("");
        }

        ToolResult {
            tool_use_id: String::new(),
            content: full_response,
            is_error: false,
        }
    }

    pub(super) fn execute_confirm(&self, input: &serde_json::Value) -> ToolResult {
        let question = input["question"].as_str().unwrap_or("Proceed?");
        self.out(&format!("{} (auto-confirmed)", question).cyan().to_string());
        ToolResult {
            tool_use_id: String::new(),
            content: "yes".to_string(),
            is_error: false,
        }
    }
}
