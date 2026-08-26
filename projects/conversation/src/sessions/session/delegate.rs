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

        let agent_icon = util::agent_emoji(agent, &self.config);
        if self.narration {
            // The dispatch one-liner is the agent's own `tagline:` frontmatter
            // field (supplied by the roster plugin), not a hardcoded roster in
            // core; fall back to a generic line when none is declared.
            let tagline = agents::resolve::load_agent_field(agent, &self.config, "tagline")
                .unwrap_or_else(|| "This specialist knows what to do.".to_string());
            self.out("");
            self.out_fmt(
                format!(
                    "  🐳 Orca: \"Otter, I'm sending this to {agent_icon} @{agent}. {tagline}\""
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
            // Generic completion banter — core knows no agent by name.
            self.out("");
            self.out_fmt(
                format!(
                    "  🐳 Orca: \"Thank you, {agent_icon} @{agent}. Otter, did you get all that?\""
                )
                .dimmed(),
            );
            self.out_fmt("  🦦 Otter: \"Every word, Orca! \"".dimmed());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::context::ProjectContext;
    use ::model::buffer_sink;
    use contract::config::{Config, Model};
    use std::sync::{Arc, Mutex};

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

    // ── execute_delegate: required-argument validation ───────────────────────

    #[tokio::test]
    async fn delegate_rejects_missing_agent() {
        let (mut s, _buf, _tmp) = test_session().await;
        let r = s
            .execute_delegate(&serde_json::json!({ "task": "do a thing" }))
            .await;
        assert!(r.is_error);
        assert_eq!(r.content, "error: agent and task are required");
    }

    #[tokio::test]
    async fn delegate_rejects_empty_task() {
        let (mut s, _buf, _tmp) = test_session().await;
        let r = s
            .execute_delegate(&serde_json::json!({ "agent": "wolf", "task": "" }))
            .await;
        assert!(r.is_error);
        assert_eq!(r.content, "error: agent and task are required");
    }

    #[tokio::test]
    async fn delegate_rejects_missing_both() {
        let (mut s, _buf, _tmp) = test_session().await;
        let r = s.execute_delegate(&serde_json::json!({})).await;
        assert!(r.is_error);
        assert_eq!(r.content, "error: agent and task are required");
    }

    // ── execute_delegate: unknown agent (no roster in temp config) ────────────

    #[tokio::test]
    async fn delegate_reports_unknown_agent() {
        let (mut s, _buf, _tmp) = test_session().await;
        let r = s
            .execute_delegate(&serde_json::json!({
                "agent": "ghost",
                "task": "investigate"
            }))
            .await;
        assert!(r.is_error);
        assert_eq!(r.content, "error: agent @ghost not found");
        assert!(r.tool_use_id.is_empty());
    }

    // ── execute_confirm: auto-approves and echoes the question ────────────────

    #[tokio::test]
    async fn confirm_auto_approves_with_question() {
        let (s, buf, _tmp) = test_session().await;
        let r = s.execute_confirm(&serde_json::json!({ "question": "Delete all?" }));
        assert!(!r.is_error);
        assert_eq!(r.content, "yes");
        let text = output(&buf);
        assert!(text.contains("Delete all?"), "echoes question: {text:?}");
        assert!(
            text.contains("(auto-confirmed)"),
            "notes auto-confirm: {text:?}"
        );
    }

    #[tokio::test]
    async fn confirm_defaults_question_when_absent() {
        let (s, buf, _tmp) = test_session().await;
        let r = s.execute_confirm(&serde_json::json!({}));
        assert!(!r.is_error);
        assert_eq!(r.content, "yes");
        assert!(output(&buf).contains("Proceed?"));
    }
}
