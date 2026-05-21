//! Server-side impl of `SystemService` — orca's own install lifecycle plus
//! the install-status snapshot.
#![allow(clippy::disallowed_types)] // install_status() returns Value — local helpers forced by that boundary

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::services::system::SystemService;
use orca_tools_def::system::{
    McpRegistration, PathExists, PathInitialized, PathInstalled, PathLinked, SystemStatusReport,
};
use serde_json::Value;

pub struct ServerSystem;

fn field_path(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.get("path"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn field_bool(v: &Value, k: &str, prop: &str) -> bool {
    v.get(k)
        .and_then(|x| x.get(prop))
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

#[async_trait]
impl SystemService for ServerSystem {
    async fn status(&self) -> Result<SystemStatusReport> {
        let v = crate::commands::install_status();
        Ok(SystemStatusReport {
            binary: PathInstalled {
                installed: field_bool(&v, "binary", "installed"),
                path: field_path(&v, "binary"),
            },
            claude_md: PathLinked {
                linked: field_bool(&v, "claude_md", "linked"),
                path: field_path(&v, "claude_md"),
            },
            vault: PathExists {
                exists: field_bool(&v, "vault", "exists"),
                path: field_path(&v, "vault"),
            },
            agents: PathLinked {
                linked: field_bool(&v, "agents", "linked"),
                path: field_path(&v, "agents"),
            },
            pki: PathInitialized {
                initialized: field_bool(&v, "pki", "initialized"),
                path: field_path(&v, "pki"),
            },
            mcp: McpRegistration {
                registered: field_bool(&v, "mcp", "registered"),
            },
        })
    }
}
