use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use std::fs::{File, OpenOptions};
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
        let mut log = SessionLog {
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

    pub fn append(
        &mut self,
        role: &str,
        agent: &str,
        content: &str,
        tags: &[&str],
    ) -> Result<()> {
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
                if let Ok(mut record) = serde_json::from_str::<serde_json::Value>(line) {
                    if record["id"].as_str() == Some(id.as_str()) {
                        record["important"] = json!(true);
                        record["note"] = json!(note);
                        return record.to_string();
                    }
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
