use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SessionLog {
    path: PathBuf,
    session_id: String,
    project: String,
    last_id: Option<String>,
}

impl SessionLog {
    pub fn new(project: Option<&str>, logs_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(logs_dir)?;

        let project = project.unwrap_or("general").to_string();
        let now = Utc::now().format("%Y-%m-%d_%H%M%S");
        let session_id = format!("{now}_{project}");
        let path = logs_dir.join(format!("{session_id}.jsonl"));

        // Write session-start record
        let log = SessionLog {
            path,
            session_id: session_id.clone(),
            project,
            last_id: None,
        };

        log.write_record(json!({
            "type": "session_start",
            "session": session_id,
            "timestamp": Utc::now().to_rfc3339(),
        }))?;

        Ok(log)
    }

    pub fn append(&mut self, role: &str, agent: &str, content: &str, tags: &[&str]) -> Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        self.last_id = Some(id.clone());

        self.write_record(json!({
            "id": id,
            "session": self.session_id,
            "timestamp": Utc::now().to_rfc3339(),
            "project": self.project,
            "role": role,
            "agent": agent,
            "content": content,
            "important": false,
            "tags": tags,
            "note": "",
        }))
    }

    /// Flag the last appended message as important.
    pub fn flag_last(&mut self, note: &str) -> Result<()> {
        let Some(id) = &self.last_id else {
            return Ok(());
        };

        // Read the file, find the record by id, update it in-place
        let content = std::fs::read_to_string(&self.path)?;
        let updated: String = content
            .lines()
            .map(|line| {
                if let Ok(mut record) = serde_json::from_str::<serde_json::Value>(line)
                    && record["id"].as_str() == Some(id.as_str())
                {
                    record["important"] = json!(true);
                    record["note"] = json!(note);
                    return record.to_string();
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        std::fs::write(&self.path, updated + "\n")?;
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_record(&self, record: serde_json::Value) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", record)?;
        Ok(())
    }
}

// ─── search across sessions ──────────────────────────────────────────────────

/// List JSONL session files in logs_dir, most recent first.
fn session_files(logs_dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut files: Vec<_> = std::fs::read_dir(logs_dir)?
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    Ok(files)
}

/// Parse all JSONL records from a file, skipping malformed lines.
fn read_records(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Search all session logs for a keyword. Returns matching records with context.
pub fn search_logs(logs_dir: &Path, query: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
    let mut matches = Vec::new();
    let query_lower = query.to_lowercase();

    for entry in session_files(logs_dir)? {
        for record in read_records(&entry.path()) {
            if let Some(text) = record["content"].as_str()
                && text.to_lowercase().contains(&query_lower)
            {
                matches.push(record);
                if matches.len() >= limit {
                    return Ok(matches);
                }
            }
        }
    }

    Ok(matches)
}

/// List recent sessions with summary info.
pub fn list_sessions(logs_dir: &Path, limit: usize) -> Result<Vec<SessionSummary>> {
    let mut summaries = Vec::new();

    for entry in session_files(logs_dir)?.into_iter().take(limit) {
        let records = read_records(&entry.path());
        let msg_count = records
            .iter()
            .filter(|r| r["role"].as_str().is_some())
            .count();
        let flagged = records
            .iter()
            .filter(|r| r["important"].as_bool() == Some(true))
            .count();
        let session_id = entry
            .path()
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        summaries.push(SessionSummary {
            session_id,
            messages: msg_count,
            flagged,
        });
    }

    Ok(summaries)
}

/// Recall all messages from a specific session.
pub fn recall_session(logs_dir: &Path, session_id: &str) -> Result<Vec<serde_json::Value>> {
    let target = session_files(logs_dir)?
        .into_iter()
        .find(|e| e.file_name().to_string_lossy().contains(session_id))
        .map(|e| e.path())
        .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;

    Ok(read_records(&target))
}

pub struct SessionSummary {
    pub session_id: String,
    pub messages: usize,
    pub flagged: usize,
}
