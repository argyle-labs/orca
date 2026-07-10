// Conversation log entries; HashMap/Value appear in log payloads as protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use anyhow::Result;
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
        let now = utils::time::now().compact();
        let session_id = format!("{now}_{project}");
        let path = logs_dir.join(format!("{session_id}.jsonl"));

        let log = SessionLog {
            path,
            session_id: session_id.clone(),
            project,
            last_id: None,
        };

        log.write_record(json!({
            "type": "session_start",
            "session": session_id,
            "timestamp": utils::time::now_rfc3339(),
        }))?;

        Ok(log)
    }

    pub fn append(&mut self, role: &str, agent: &str, content: &str, tags: &[&str]) -> Result<()> {
        let id = uuid::Uuid::now_v7().to_string();
        self.last_id = Some(id.clone());

        self.write_record(json!({
            "id": id,
            "session": self.session_id,
            "timestamp": utils::time::now_rfc3339(),
            "project": self.project,
            "role": role,
            "agent": agent,
            "content": content,
            "important": false,
            "tags": tags,
            "note": "",
        }))
    }

    pub fn flag_last(&mut self, note: &str) -> Result<()> {
        let Some(id) = self.last_id.clone() else {
            return Ok(());
        };
        self.write_record(json!({
            "type": "flag",
            "ref": id,
            "note": note,
            "important": true,
            "timestamp": utils::time::now_rfc3339(),
        }))
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

fn session_files(logs_dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut files: Vec<_> = std::fs::read_dir(logs_dir)?
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    Ok(files)
}

fn read_records(path: &Path) -> Vec<serde_json::Value> {
    let raw: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let mut flags: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for r in &raw {
        if r["type"].as_str() == Some("flag")
            && let (Some(ref_id), Some(note)) = (r["ref"].as_str(), r["note"].as_str())
        {
            flags.insert(ref_id.to_string(), note.to_string());
        }
    }

    raw.into_iter()
        .filter(|r| r["type"].as_str() != Some("flag"))
        .map(|mut r| {
            if let Some(id) = r["id"].as_str().map(str::to_string)
                && let Some(note) = flags.get(&id)
            {
                r["important"] = json!(true);
                r["note"] = json!(note);
            }
            r
        })
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_log(dir: &std::path::Path, project: Option<&str>) -> SessionLog {
        SessionLog::new(project, dir).expect("SessionLog::new")
    }

    // ── SessionLog construction ───────────────────────────────────────────────

    #[test]
    fn session_log_creates_file_on_new() {
        let tmp = tempfile::tempdir().unwrap();
        let log = make_log(tmp.path(), Some("testproject"));
        assert!(log.path().exists(), "log file should be created");
        assert!(log.session_id().contains("testproject"));
    }

    #[test]
    fn session_log_default_project_is_general() {
        let tmp = tempfile::tempdir().unwrap();
        let log = make_log(tmp.path(), None);
        assert!(log.session_id().contains("general"));
    }

    #[test]
    fn session_log_creates_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("deep").join("logs");
        let log = SessionLog::new(Some("proj"), &nested).expect("should create dirs");
        assert!(log.path().exists());
    }

    // ── append + read_records ─────────────────────────────────────────────────

    #[test]
    fn append_writes_readable_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        log.append("user", "wolf", "hello world", &["tag1"])
            .unwrap();

        let records = read_records(log.path());
        // First record is session_start, second is the appended message
        let msg = records.iter().find(|r| r["role"].as_str() == Some("user"));
        assert!(msg.is_some(), "should have a user record");
        let msg = msg.unwrap();
        assert_eq!(msg["content"].as_str(), Some("hello world"));
        assert_eq!(msg["agent"].as_str(), Some("wolf"));
    }

    #[test]
    fn append_sets_last_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        assert!(log.last_id.is_none());
        log.append("user", "wolf", "msg", &[]).unwrap();
        assert!(log.last_id.is_some());
    }

    // ── flag_last ─────────────────────────────────────────────────────────────

    #[test]
    fn flag_last_marks_record_as_important() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        log.append("assistant", "wolf", "important answer", &[])
            .unwrap();
        log.flag_last("key insight").unwrap();

        let records = read_records(log.path());
        let flagged = records
            .iter()
            .find(|r| r["important"].as_bool() == Some(true));
        assert!(flagged.is_some(), "should have a flagged record");
        assert_eq!(flagged.unwrap()["note"].as_str(), Some("key insight"));
    }

    #[test]
    fn flag_last_noop_when_no_last_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        // No append — flag_last should be a no-op
        assert!(log.flag_last("note").is_ok());
    }

    // ── list_sessions ─────────────────────────────────────────────────────────

    #[test]
    fn list_sessions_empty_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let summaries = list_sessions(tmp.path(), 10).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn list_sessions_counts_messages_and_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("myproj"));
        log.append("user", "wolf", "q1", &[]).unwrap();
        log.append("assistant", "wolf", "a1", &[]).unwrap();
        log.flag_last("remember this").unwrap();
        log.append("user", "wolf", "q2", &[]).unwrap();
        drop(log);

        let summaries = list_sessions(tmp.path(), 10).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].messages, 3);
        assert_eq!(summaries[0].flagged, 1);
    }

    #[test]
    fn list_sessions_respects_limit() {
        let tmp = tempfile::tempdir().unwrap();
        // Create 3 session logs
        for i in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let _ = make_log(tmp.path(), Some(&format!("proj{i}")));
        }
        let summaries = list_sessions(tmp.path(), 2).unwrap();
        assert_eq!(summaries.len(), 2);
    }

    // ── search_logs ───────────────────────────────────────────────────────────

    #[test]
    fn search_logs_finds_matching_content() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        log.append("user", "wolf", "the quick brown fox", &[])
            .unwrap();
        log.append("user", "wolf", "something unrelated", &[])
            .unwrap();
        drop(log);

        let results = search_logs(tmp.path(), "quick brown", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["content"].as_str(), Some("the quick brown fox"));
    }

    #[test]
    fn search_logs_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        log.append("user", "wolf", "Hello World", &[]).unwrap();
        drop(log);

        let results = search_logs(tmp.path(), "hello world", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_logs_respects_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        for i in 0..5 {
            log.append("user", "wolf", &format!("match {i}"), &[])
                .unwrap();
        }
        drop(log);

        let results = search_logs(tmp.path(), "match", 3).unwrap();
        assert_eq!(results.len(), 3);
    }

    // ── recall_session ────────────────────────────────────────────────────────

    #[test]
    fn recall_session_returns_records_for_known_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = make_log(tmp.path(), Some("proj"));
        let sid = log.session_id().to_string();
        log.append("user", "wolf", "recalled content", &[]).unwrap();
        drop(log);

        let records = recall_session(tmp.path(), &sid).unwrap();
        let msg = records
            .iter()
            .find(|r| r["content"].as_str() == Some("recalled content"));
        assert!(msg.is_some(), "should find appended message");
    }

    #[test]
    fn recall_session_errors_for_unknown_session() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let err = recall_session(tmp.path(), "nonexistent-session-id").unwrap_err();
        assert!(err.to_string().contains("session not found"), "got: {err}");
    }
}
