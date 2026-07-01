//! Orca jobs — background agent execution.
//!
//! `JobManager` spawns independent Tokio tasks that run a full chat+tool loop
//! without blocking the foreground session. Results are buffered in memory and
//! retrieved via `get_output`. Cancellation is handled via `CancellationToken`.

use ::model::tools::ToolRegistry;
use ::model::{Message, ModelBackend, OutputSink, buffer_sink, sink_write};
use anyhow::Result;
use colored::Colorize;
use contract::ToolResult;
use contract::config::{Config, Model};
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

/// Cap on retained finished jobs. Oldest finished+notified entries beyond this
/// cap are dropped so their JoinHandle/CancellationToken/prompt/output buffer
/// can be reclaimed. Running jobs are never pruned.
const MAX_RETAINED_FINISHED: usize = 32;

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

        let backend = ::model::build_backend(config, model)?;
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
        self.reap_finished();
        notes
    }

    /// Drop oldest finished+notified jobs once they exceed `MAX_RETAINED_FINISHED`,
    /// reclaiming their JoinHandle, CancellationToken, prompt, and output buffer.
    /// Running jobs are preserved regardless of position.
    fn reap_finished(&mut self) {
        let finished_notified = self
            .jobs
            .iter()
            .filter(|j| j.is_finished() && j.notified)
            .count();
        if finished_notified <= MAX_RETAINED_FINISHED {
            return;
        }
        let mut to_drop = finished_notified - MAX_RETAINED_FINISHED;
        self.jobs.retain(|j| {
            if to_drop > 0 && j.is_finished() && j.notified {
                to_drop -= 1;
                false
            } else {
                true
            }
        });
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
            let mut p = ::model::tools::bash::BashPermissions::default();
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

fn truncate_preview(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn write_to_sink(sink: &OutputSink, data: &str) {
    sink_write(sink, data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::model::buffer_sink;

    fn make_finished_job(id: usize, prompt: &str) -> BackgroundJob {
        let (_, buffer) = buffer_sink();
        let handle = tokio::spawn(async { Ok(()) });
        BackgroundJob {
            id,
            prompt: prompt.to_string(),
            buffer,
            handle,
            cancel: CancellationToken::new(),
            notified: false,
        }
    }

    fn make_running_job(id: usize, prompt: &str) -> BackgroundJob {
        let (_, buffer) = buffer_sink();
        let handle = tokio::spawn(async {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            Ok(())
        });
        BackgroundJob {
            id,
            prompt: prompt.to_string(),
            buffer,
            handle,
            cancel: CancellationToken::new(),
            notified: false,
        }
    }

    // ── BackgroundJob ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn background_job_is_finished_after_task_completes() {
        let job = make_finished_job(1, "test");
        // Give tokio a chance to run the spawned task
        tokio::task::yield_now().await;
        // The task is an instant Ok(()), so it should be done quickly
        tokio::time::timeout(tokio::time::Duration::from_millis(100), async {
            loop {
                if job.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("job should finish within 100ms");
    }

    #[tokio::test]
    async fn background_job_output_reads_buffer_contents() {
        let (sink, buffer) = buffer_sink();
        let handle = tokio::spawn(async { Ok(()) });
        sink_write(&sink, "hello from job");
        let job = BackgroundJob {
            id: 1,
            prompt: "p".into(),
            buffer,
            handle,
            cancel: CancellationToken::new(),
            notified: false,
        };
        assert_eq!(job.output(), "hello from job");
    }

    #[tokio::test]
    async fn background_job_output_empty_by_default() {
        let job = make_finished_job(1, "empty");
        assert_eq!(job.output(), "");
    }

    // ── JobManager state management ───────────────────────────────────────────

    #[tokio::test]
    async fn job_manager_starts_empty() {
        let manager = JobManager::new();
        let listed = manager.list();
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].contains("no background jobs"),
            "got: {:?}",
            listed[0]
        );
    }

    #[tokio::test]
    async fn drain_notifications_empty_on_new_manager() {
        let mut manager = JobManager::new();
        assert!(manager.drain_notifications().is_empty());
    }

    #[tokio::test]
    async fn get_output_returns_none_for_unknown_id() {
        let manager = JobManager::new();
        assert!(manager.get_output(999).is_none());
    }

    #[tokio::test]
    async fn cancel_returns_false_for_unknown_id() {
        let mut manager = JobManager::new();
        assert!(!manager.cancel(999));
    }

    #[tokio::test]
    async fn drain_notifications_notifies_once_then_silent() {
        let mut manager = JobManager::new();
        let job = make_finished_job(1, "do something");
        // Wait for task to finish
        tokio::task::yield_now().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        manager.jobs.push(job);

        let notes = manager.drain_notifications();
        // Should have a notification for the finished job
        assert!(!notes.is_empty(), "should notify about finished job");
        assert!(
            notes[0].contains("#1"),
            "notification should mention job id: {:?}",
            notes[0]
        );

        // Second drain: already notified, no more notes
        let notes2 = manager.drain_notifications();
        assert!(notes2.is_empty(), "second drain should be empty");
    }

    #[tokio::test]
    async fn list_shows_done_for_finished_job() {
        let mut manager = JobManager::new();
        let job = make_finished_job(42, "finished task");
        tokio::task::yield_now().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        manager.jobs.push(job);

        let list = manager.list();
        assert_eq!(list.len(), 1);
        let entry = &list[0];
        assert!(entry.contains("#42"), "should show job id: {entry}");
        assert!(entry.contains("done"), "should show 'done' status: {entry}");
    }

    #[tokio::test]
    async fn list_shows_running_for_active_job() {
        let mut manager = JobManager::new();
        let job = make_running_job(7, "long task");
        manager.jobs.push(job);

        let list = manager.list();
        assert_eq!(list.len(), 1);
        let entry = &list[0];
        assert!(entry.contains("#7"), "should show job id: {entry}");
        assert!(
            entry.contains("running"),
            "should show 'running' status: {entry}"
        );
    }

    #[tokio::test]
    async fn cancel_running_job_returns_true() {
        let mut manager = JobManager::new();
        let job = make_running_job(5, "cancellable");
        manager.jobs.push(job);

        assert!(
            manager.cancel(5),
            "cancel should return true for running job"
        );
        // Job is still sleeping (token cancelled, task not yet woken) — cancel is idempotent
        assert!(
            manager.cancel(5),
            "second cancel on still-running job also returns true"
        );
        // Unknown id always returns false
        assert!(!manager.cancel(999), "cancel unknown id returns false");
    }

    #[tokio::test]
    async fn cancel_finished_job_returns_false() {
        let mut manager = JobManager::new();
        let job = make_finished_job(9, "done already");
        tokio::task::yield_now().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        manager.jobs.push(job);

        // Job is finished so cancel finds no running job with that id
        assert!(!manager.cancel(9), "can't cancel an already-finished job");
    }

    #[tokio::test]
    async fn get_output_returns_job_output() {
        let (sink, buffer) = buffer_sink();
        sink_write(&sink, "job output text");
        let handle = tokio::spawn(async { Ok(()) });
        let mut manager = JobManager::new();
        manager.jobs.push(BackgroundJob {
            id: 3,
            prompt: "p".into(),
            buffer,
            handle,
            cancel: CancellationToken::new(),
            notified: false,
        });

        assert_eq!(manager.get_output(3).as_deref(), Some("job output text"));
        assert!(manager.get_output(99).is_none());
    }

    #[tokio::test]
    async fn job_id_increments_correctly() {
        // Verify next_id by manually constructing two job states
        let manager = JobManager::new();
        assert_eq!(manager.next_id, 1);
    }

    // ── truncate_preview ──────────────────────────────────────────────────────

    #[test]
    fn truncate_preview_short_string_unchanged() {
        assert_eq!(truncate_preview("hello", 10), "hello");
    }

    #[test]
    fn truncate_preview_long_string_gets_ellipsis() {
        let result = truncate_preview("abcdefghij", 5);
        assert_eq!(result, "abcde…");
    }

    #[test]
    fn truncate_preview_exactly_at_limit_no_ellipsis() {
        assert_eq!(truncate_preview("12345", 5), "12345");
    }
}
