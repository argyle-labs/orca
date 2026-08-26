use super::{Session, util};
use ::model::Message;
use ::model::tools::ToolRegistry;
use anyhow::Result;
use colored::Colorize;
use contract::ToolResult;
use tokio_util::sync::CancellationToken;

impl Session {
    pub(super) async fn chat(&mut self, input: String) -> Result<()> {
        if let Some(log) = &mut self.log {
            log.append("user", &self.active_agent.clone(), &input, &[])
                .ok();
        }

        self.messages.push(Message::user(input));

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let ctrl_c_task = tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel_clone.cancel();
        });

        let max_rounds = 30;
        let tools = if self.backend.supports_tools() {
            ToolRegistry::definitions()
        } else {
            vec![]
        };
        for round in 0..max_rounds {
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
                    self.tools.execute(tc.id.clone(), &tc.name, &tc.input).await
                };
                r.tool_use_id = tc.id.clone();

                let summary = util::summarize_result(&tc.name, &r.content, r.is_error);
                if r.is_error {
                    self.out(&summary.red().to_string());
                } else {
                    self.out(&summary.dimmed().to_string());
                }

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
        ctrl_c_task.abort();

        Ok(())
    }

    fn check_commit_status(&self) {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        if let Some(dir) = cwd
            && let Some(count) = util::check_git_changes(&dir)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::context::ProjectContext;
    use crate::sessions::log::SessionLog;
    use ::model::backend::BoxFuture;
    use ::model::{BackendResponse, ModelBackend, OutputSink, buffer_sink};
    use contract::ToolCall;
    use contract::ToolDef;
    use contract::config::{Config, Model};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    async fn base_session() -> (Session, Arc<Mutex<Vec<u8>>>, tempfile::TempDir) {
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

    /// A deterministic in-memory backend that plays a fixed script of responses,
    /// one per `chat` round. Once the script is exhausted it returns a terminal
    /// text-only response (no tool calls) so the loop always ends.
    struct ScriptedBackend {
        rounds: Vec<BackendResponse>,
        calls: AtomicUsize,
        tools_supported: bool,
    }

    impl ScriptedBackend {
        fn new(rounds: Vec<BackendResponse>) -> Self {
            Self {
                rounds,
                calls: AtomicUsize::new(0),
                tools_supported: true,
            }
        }
    }

    impl ModelBackend for ScriptedBackend {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDef],
            _system: &'a str,
            _cancel: CancellationToken,
            _output: &'a OutputSink,
        ) -> BoxFuture<'a, Result<BackendResponse>> {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            let resp = self.rounds.get(idx).cloned().unwrap_or(BackendResponse {
                text: "done".into(),
                tool_calls: vec![],
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            });
            Box::pin(async move { Ok(resp) })
        }

        fn name(&self) -> &str {
            "scripted"
        }

        fn model_id(&self) -> &str {
            "scripted-1"
        }

        fn supports_tools(&self) -> bool {
            self.tools_supported
        }
    }

    fn confirm_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "confirm".into(),
            input: serde_json::json!({ "question": "proceed?" }),
        }
    }

    // ── A single text-only response terminates immediately ────────────────────

    #[tokio::test]
    async fn chat_text_only_response_records_turn_and_terminates() {
        let (mut s, buf, _tmp) = base_session().await;
        s.backend = Box::new(ScriptedBackend::new(vec![BackendResponse {
            text: "hello back".into(),
            tool_calls: vec![],
            input_tokens: 12,
            output_tokens: 7,
            ..Default::default()
        }]));

        s.chat("hi there".to_string()).await.unwrap();

        // user turn + one assistant turn, nothing more (no tool round).
        assert_eq!(s.messages.len(), 2);
        assert!(matches!(&s.messages[0], Message::User { .. }));
        assert!(matches!(
            &s.messages[1],
            Message::Assistant { text: Some(t), tool_calls } if t == "hello back" && tool_calls.is_empty()
        ));
        // Ledger recorded exactly one backend call with the reported tokens.
        assert_eq!(s.ledger.total_calls, 1);
        assert_eq!(s.ledger.session_input, 12);
        assert_eq!(s.ledger.session_output, 7);
        // The token ledger footer is emitted on the terminal (no-tools) path.
        assert!(output(&buf).contains("calls: 1"));
    }

    // ── A tool round then a terminal response drives the full loop ────────────

    #[tokio::test]
    async fn chat_tool_round_executes_then_terminates() {
        let (mut s, buf, _tmp) = base_session().await;
        s.backend = Box::new(ScriptedBackend::new(vec![
            BackendResponse {
                text: "let me confirm".into(),
                tool_calls: vec![confirm_call("tc1")],
                input_tokens: 10,
                output_tokens: 4,
                ..Default::default()
            },
            BackendResponse {
                text: "all set".into(),
                tool_calls: vec![],
                input_tokens: 5,
                output_tokens: 3,
                ..Default::default()
            },
        ]));

        s.chat("do the thing".to_string()).await.unwrap();

        // user, assistant(w/ tool call), tool results, assistant(final).
        assert_eq!(s.messages.len(), 4);
        assert!(matches!(&s.messages[0], Message::User { .. }));
        assert!(matches!(
            &s.messages[1],
            Message::Assistant { tool_calls, .. } if tool_calls.len() == 1
        ));
        assert!(matches!(&s.messages[2], Message::ToolResults(r) if r.len() == 1));
        assert!(matches!(
            &s.messages[3],
            Message::Assistant { text: Some(t), tool_calls } if t == "all set" && tool_calls.is_empty()
        ));

        // The confirm tool result carries the originating tool_use id.
        if let Message::ToolResults(r) = &s.messages[2] {
            assert_eq!(r[0].tool_use_id, "tc1");
            assert_eq!(r[0].content, "yes");
            assert!(!r[0].is_error);
        } else {
            panic!("expected tool results");
        }

        // Both rounds recorded; confirm question surfaced to output.
        assert_eq!(s.ledger.total_calls, 2);
        assert_eq!(s.ledger.session_input, 15);
        let text = output(&buf);
        assert!(
            text.contains("auto-confirmed"),
            "confirm output missing: {text}"
        );
    }

    // ── Empty-text assistant response stores None, not Some("") ───────────────

    #[tokio::test]
    async fn chat_empty_text_response_stores_none_text() {
        let (mut s, _buf, _tmp) = base_session().await;
        s.backend = Box::new(ScriptedBackend::new(vec![BackendResponse {
            text: String::new(),
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        }]));

        s.chat("quiet".to_string()).await.unwrap();

        assert!(matches!(
            &s.messages[1],
            Message::Assistant { text: None, .. }
        ));
    }

    // ── When logging is active, user + assistant turns are appended to the log ─

    #[tokio::test]
    async fn chat_with_active_log_appends_user_and_assistant_turns() {
        let (mut s, _buf, tmp) = base_session().await;
        s.log = SessionLog::new(None, &tmp.path().join("logs")).ok();
        assert!(s.log.is_some(), "log should be active for this test");
        s.backend = Box::new(ScriptedBackend::new(vec![BackendResponse {
            text: "logged reply".into(),
            tool_calls: vec![],
            input_tokens: 2,
            output_tokens: 2,
            ..Default::default()
        }]));

        s.chat("logged input".to_string()).await.unwrap();

        // The turns landed in messages regardless of the log branch executing.
        assert_eq!(s.messages.len(), 2);
        assert!(matches!(
            &s.messages[1],
            Message::Assistant { text: Some(t), .. } if t == "logged reply"
        ));
    }

    // ── round-counter banner appears once the loop enters a second tool round ──

    #[tokio::test]
    async fn chat_multiple_tool_rounds_emit_round_banner() {
        let (mut s, buf, _tmp) = base_session().await;
        s.backend = Box::new(ScriptedBackend::new(vec![
            BackendResponse {
                text: "round one".into(),
                tool_calls: vec![confirm_call("a")],
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            BackendResponse {
                text: "round two".into(),
                tool_calls: vec![confirm_call("b")],
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            BackendResponse {
                text: "final".into(),
                tool_calls: vec![],
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
        ]));

        s.chat("go".to_string()).await.unwrap();

        // Three backend calls: two tool rounds + one terminal.
        assert_eq!(s.ledger.total_calls, 3);
        // The "[round 2/30]" banner only prints for round index > 0.
        assert!(
            output(&buf).contains("round 2/30"),
            "expected second-round banner: {}",
            output(&buf)
        );
    }
}
