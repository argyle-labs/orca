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
