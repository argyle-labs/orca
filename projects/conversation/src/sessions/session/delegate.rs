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

    // ── Delegation loop tests (mock backend + seeded agent) ───────────────────
    //
    // The delegation loop only runs once `load_agent_prompt` resolves, which
    // requires an active profile whose `agents/` dir holds `<name>.md`. We seed
    // exactly that against the session's own encrypted DB + profiles dir, then
    // swap in a scripted `MockBackend` so the loop's response handling, tool
    // dispatch, ledger accounting, narration, and max-rounds paths run
    // deterministically without a live model.

    use ::model::types::{BackendResponse, StopReason};
    use ::model::{ModelBackend, OutputSink};
    use contract::ToolCall;
    use std::collections::VecDeque;
    use tokio_util::sync::CancellationToken;

    /// A backend that replays a scripted queue of responses. When `always` is
    /// set it returns that response on every call (used to drive the max-rounds
    /// path). A `None` entry (empty queue with no `always`) yields an error,
    /// exercising the delegation-error branch.
    struct MockBackend {
        queue: std::sync::Mutex<VecDeque<BackendResponse>>,
        always: Option<BackendResponse>,
        fail: bool,
    }

    impl MockBackend {
        fn scripted(responses: Vec<BackendResponse>) -> Self {
            Self {
                queue: std::sync::Mutex::new(responses.into_iter().collect()),
                always: None,
                fail: false,
            }
        }
        fn always(resp: BackendResponse) -> Self {
            Self {
                queue: std::sync::Mutex::new(VecDeque::new()),
                always: Some(resp),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                queue: std::sync::Mutex::new(VecDeque::new()),
                always: None,
                fail: true,
            }
        }
    }

    impl ModelBackend for MockBackend {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [contract::ToolDef],
            _system: &'a str,
            _cancel: CancellationToken,
            _output: &'a OutputSink,
        ) -> ::model::backend::BoxFuture<'a, anyhow::Result<BackendResponse>> {
            Box::pin(async move {
                if self.fail {
                    return Err(anyhow::anyhow!("backend exploded"));
                }
                if let Some(r) = &self.always {
                    return Ok(r.clone());
                }
                self.queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| anyhow::anyhow!("no more scripted responses"))
            })
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn model_id(&self) -> &str {
            "mock-model"
        }
    }

    fn text_response(text: &str) -> BackendResponse {
        BackendResponse {
            text: text.to_string(),
            tool_calls: Vec::new(),
            input_tokens: 10,
            output_tokens: 5,
            stop_reason: StopReason::EndTurn,
        }
    }

    fn tool_response(text: &str, tool: &str) -> BackendResponse {
        BackendResponse {
            text: text.to_string(),
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: tool.to_string(),
                input: serde_json::json!({}),
            }],
            input_tokens: 7,
            output_tokens: 3,
            stop_reason: StopReason::ToolUse,
        }
    }

    /// Seed an active profile for LOCAL_USER against the session's encrypted DB
    /// and write `agents/<name>.md` so `load_agent_prompt(name)` resolves.
    fn seed_agent(config: &Config, name: &str, tagline: Option<&str>) {
        let conn = db::open(&config.db_path).unwrap();
        let mgr = namespace::NamespaceManager::from_config(config);
        let profile = mgr
            .create(&conn, contract::config::LOCAL_USER, "test-profile", None)
            .unwrap();
        mgr.set_active(&conn, contract::config::LOCAL_USER, &profile.id)
            .unwrap();
        let agents_dir = profile.agents_dir();
        std::fs::create_dir_all(&agents_dir).unwrap();
        let frontmatter = match tagline {
            Some(t) => format!("---\ntagline: {t}\n---\nYou are the {name} specialist.\n"),
            None => format!("---\n---\nYou are the {name} specialist.\n"),
        };
        std::fs::write(agents_dir.join(format!("{name}.md")), frontmatter).unwrap();
    }

    #[tokio::test]
    async fn delegate_runs_loop_and_returns_accumulated_text() {
        let (mut s, buf, tmp) = test_session().await;
        seed_agent(&s.config, "wolf", None);
        // Sanity: the agent now resolves.
        assert!(agents::resolve::load_agent_prompt("wolf", &s.config).is_some());

        // Round 0 emits text + a tool call (unknown tool → error ToolResult,
        // exercising the is_error summary branch); round 1 emits text with no
        // tool calls, which breaks the loop.
        s.backend = Box::new(MockBackend::scripted(vec![
            tool_response("investigating", "nonexistent_tool"),
            text_response(" and done"),
        ]));

        let calls_before = s.ledger.total_calls;
        let r = s
            .execute_delegate(&serde_json::json!({ "agent": "wolf", "task": "look into it" }))
            .await;

        assert!(!r.is_error, "successful delegation is not an error");
        assert_eq!(r.content, "investigating and done");
        assert!(r.tool_use_id.is_empty());
        // Both rounds' token usage was recorded on the shared ledger.
        assert!(
            s.ledger.total_calls > calls_before,
            "delegation must accrue tokens"
        );
        let text = output(&buf);
        assert!(text.contains("@wolf"), "banner names the agent: {text:?}");
        drop(tmp);
    }

    #[tokio::test]
    async fn delegate_surfaces_backend_error() {
        let (mut s, _buf, _tmp) = test_session().await;
        seed_agent(&s.config, "otter", None);
        s.backend = Box::new(MockBackend::failing());

        let r = s
            .execute_delegate(&serde_json::json!({ "agent": "otter", "task": "go" }))
            .await;

        assert!(r.is_error);
        assert!(
            r.content.starts_with("delegation error: backend exploded"),
            "propagates backend error: {:?}",
            r.content
        );
    }

    #[tokio::test]
    async fn delegate_narration_emits_orca_otter_banter() {
        let (mut s, buf, _tmp) = test_session().await;
        s.narration = true;
        seed_agent(&s.config, "wolf", Some("Wolf hunts bugs."));
        // No tool calls → the loop breaks after the first round.
        s.backend = Box::new(MockBackend::scripted(vec![text_response("all clear")]));

        let r = s
            .execute_delegate(&serde_json::json!({ "agent": "wolf", "task": "scan" }))
            .await;

        assert!(!r.is_error);
        assert_eq!(r.content, "all clear");
        let text = output(&buf);
        // Dispatch banter uses the agent's own tagline frontmatter.
        assert!(text.contains("Wolf hunts bugs."), "tagline used: {text:?}");
        assert!(text.contains("Orca:"), "orca line present: {text:?}");
        assert!(text.contains("Otter:"), "otter line present: {text:?}");
    }

    #[tokio::test]
    async fn delegate_stops_at_max_rounds() {
        let (mut s, buf, _tmp) = test_session().await;
        seed_agent(&s.config, "wolf", None);
        // Every round returns a tool call, so the loop never breaks early and
        // runs the full 20 rounds, hitting the "(max rounds reached)" branch.
        s.backend = Box::new(MockBackend::always(tool_response(
            "step",
            "nonexistent_tool",
        )));

        let r = s
            .execute_delegate(&serde_json::json!({ "agent": "wolf", "task": "loop" }))
            .await;

        assert!(!r.is_error);
        // 20 rounds each appended "step".
        assert_eq!(r.content, "step".repeat(20));
        assert!(
            output(&buf).contains("(max rounds reached)"),
            "max-rounds notice emitted"
        );
    }

    #[tokio::test]
    async fn delegate_writes_to_session_log_when_active() {
        let (mut s, _buf, tmp) = test_session().await;
        seed_agent(&s.config, "wolf", None);
        // Activate logging so the `if let Some(log)` branches (delegation,
        // assistant, tool) all execute.
        s.log = crate::sessions::log::SessionLog::new(None, &s.config.logs_dir()).ok();
        assert!(s.log.is_some(), "log must be active for this test");

        s.backend = Box::new(MockBackend::scripted(vec![
            tool_response("checking", "nonexistent_tool"),
            text_response(" ok"),
        ]));

        let r = s
            .execute_delegate(&serde_json::json!({ "agent": "wolf", "task": "audit" }))
            .await;
        assert!(!r.is_error);
        assert_eq!(r.content, "checking ok");
        drop(tmp);
    }
}
