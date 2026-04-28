//! Brain jobs — background agent execution.
//!
//! `JobManager` spawns independent Tokio tasks that run a full chat+tool loop
//! without blocking the foreground session. Results are buffered in memory and
//! retrieved via `get_output`. Cancellation is handled via `CancellationToken`.

use brain_core::backend::{ModelBackend, OutputSink, buffer_sink, sink_write};
use brain_core::tools::ToolRegistry;
use brain_utils::config::{Config, Model};
use brain_utils::types::{Message, ToolResult, truncate_preview};
use anyhow::Result;
use colored::Colorize;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A background agent job that runs independently of the foreground session.
pub struct BackgroundJob {
    pub id: usize,
    pub prompt: String,
    pub buffer: Arc<Mutex<Vec<u8>>>,
    pub handle: JoinHandle<Result<()>>,
    pub cancel: CancellationToken,
    pub notified: bool,
}

impl BackgroundJob {
    /// Check if the background task has finished.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Read the buffered output as a string.
    pub fn output(&self) -> String {
        if let Ok(buf) = self.buffer.lock() {
            String::from_utf8_lossy(&buf).to_string()
        } else {
            "(buffer locked)".to_string()
        }
    }
}

/// Manages background jobs for a session.
pub struct JobManager {
    jobs: Vec<BackgroundJob>,
    next_id: usize,
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl JobManager {
    pub fn new() -> Self {
        JobManager {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    /// Spawn a background agent job. Returns the job ID.
    pub fn spawn(
        &mut self,
        config: &Config,
        model: &Model,
        system_prompt: String,
        prompt: String,
    ) -> Result<usize> {
        let id = self.next_id;
        self.next_id += 1;

        let backend = brain_core::backend::build_backend(config, model)?;
        let (sink, buffer) = buffer_sink();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let prompt_clone = prompt.clone();

        let handle = tokio::spawn(async move {
            run_background_chat(backend, system_prompt, prompt_clone, sink, cancel_clone).await
        });

        self.jobs.push(BackgroundJob {
            id,
            prompt,
            buffer,
            handle,
            cancel,
            notified: false,
        });

        Ok(id)
    }

    /// Check for newly completed jobs and return notifications.
    /// Call this before each readline prompt.
    pub fn drain_notifications(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for job in &mut self.jobs {
            if job.is_finished() && !job.notified {
                job.notified = true;
                let status = if job.handle.is_finished() {
                    "done".green().to_string()
                } else {
                    "running".yellow().to_string()
                };
                notes.push(format!(
                    "  {} job #{} [{}]: {}",
                    "⚡".dimmed(),
                    job.id,
                    status,
                    truncate_preview(&job.prompt, 60),
                ));
            }
        }
        notes
    }

    /// List all jobs with their status.
    pub fn list(&self) -> Vec<String> {
        if self.jobs.is_empty() {
            return vec!["no background jobs.".dimmed().to_string()];
        }
        self.jobs
            .iter()
            .map(|j| {
                let status = if j.is_finished() {
                    "done".green().to_string()
                } else {
                    "running".yellow().to_string()
                };
                format!(
                    "  #{:<3} [{}]  {}",
                    j.id,
                    status,
                    truncate_preview(&j.prompt, 60),
                )
            })
            .collect()
    }

    /// Get the output of a specific job.
    pub fn get_output(&self, id: usize) -> Option<String> {
        self.jobs.iter().find(|j| j.id == id).map(|j| j.output())
    }

    /// Cancel a running job.
    pub fn cancel(&mut self, id: usize) -> bool {
        if let Some(job) = self
            .jobs
            .iter_mut()
            .find(|j| j.id == id && !j.is_finished())
        {
            job.cancel.cancel();
            true
        } else {
            false
        }
    }
}

/// Run a full chat + tool loop in the background, writing all output to the sink.
async fn run_background_chat(
    backend: Box<dyn ModelBackend>,
    system_prompt: String,
    prompt: String,
    output: OutputSink,
    cancel: CancellationToken,
) -> Result<()> {
    let mut messages = vec![Message::user(&prompt)];
    let mut tools = ToolRegistry {
        output: output.clone(),
        permissions: {
            let mut p = brain_core::tools::bash::BashPermissions::default();
            p.auto_approve = true;
            p
        },
        working_dir: None,
    };
    let tool_defs = ToolRegistry::definitions()
        .into_iter()
        .filter(|t| t.name != "delegate" && t.name != "confirm")
        .collect::<Vec<_>>();

    let max_rounds = 20;

    for _round in 0..max_rounds {
        let round_cancel = cancel.child_token();

        let response = backend
            .chat(&messages, &tool_defs, &system_prompt, round_cancel, &output)
            .await?;

        if cancel.is_cancelled() {
            write_to_sink(&output, &format!("{}\n", "[cancelled]".yellow()));
            break;
        }

        messages.push(Message::Assistant {
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

        let mut results: Vec<ToolResult> = Vec::new();
        for tc in &response.tool_calls {
            write_to_sink(
                &output,
                &format!("{}\n", format!("  ⚙ {} {}", tc.name, tc.input).dimmed()),
            );

            let r = tools.execute(tc.id.clone(), &tc.name, &tc.input).await;

            let preview = truncate_preview(&r.content, 200);
            if r.is_error {
                write_to_sink(&output, &format!("{}\n", preview.red()));
            } else {
                write_to_sink(&output, &format!("{}\n", preview.dimmed()));
            }

            results.push(r);
        }

        messages.push(Message::ToolResults(results));
    }

    Ok(())
}

fn write_to_sink(sink: &OutputSink, data: &str) {
    sink_write(sink, data);
}
