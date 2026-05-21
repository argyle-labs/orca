//! Server-side `ProfileService` impl — wraps `ProfileManager`.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use orca_tools_def::orca_profile::{
    ProfileCurrentReport, ProfileDetail, ProfileListReport, ProfileMutationResult,
    ProfileShareEntry, ProfileSharesReport, ProfileSummary,
};
use orca_tools_def::services::profile::ProfileService;
use orca_utils::config::{Config, LOCAL_USER};
use std::sync::Arc;

use crate::profile::{ProfileManager, Role};

fn user_id() -> String {
    LOCAL_USER.to_string()
}

pub struct ServerProfile {
    pub config: Arc<Config>,
}

impl ServerProfile {
    fn open(&self) -> Result<(rusqlite::Connection, ProfileManager)> {
        let conn = db::open(&self.config.db_path).context("open orca.db")?;
        let mgr = ProfileManager::from_config(&self.config);
        Ok((conn, mgr))
    }

    fn summary(p: &crate::profile::Profile, active_id: Option<&str>) -> ProfileSummary {
        ProfileSummary {
            id: p.id.clone(),
            name: p.name.clone(),
            owner_user_id: p.owner_user_id.clone(),
            is_active: active_id == Some(p.id.as_str()),
        }
    }
}

#[async_trait]
impl ProfileService for ServerProfile {
    async fn list(&self) -> Result<ProfileListReport> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let profiles = mgr.list_for_user(&conn, &me)?;
        let active = db::profiles::get_active(&conn, &me).ok().flatten();
        let summaries = profiles
            .iter()
            .map(|p| Self::summary(p, active.as_deref()))
            .collect();
        Ok(ProfileListReport {
            profiles: summaries,
        })
    }

    async fn show(&self, spec: Option<&str>) -> Result<ProfileDetail> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let p = match spec {
            Some(s) => mgr
                .resolve_spec(&conn, &me, s)?
                .ok_or_else(|| anyhow!("profile not found: {s}"))?,
            None => mgr
                .resolve_active(&conn, &me)?
                .ok_or_else(|| anyhow!("no active profile"))?,
        };
        let access = mgr.access(&conn, &p.id, &me)?;
        Ok(ProfileDetail {
            id: p.id,
            name: p.name,
            owner_user_id: p.owner_user_id,
            description: p.description,
            root: p.root.display().to_string(),
            access: format!("{access:?}").to_lowercase(),
        })
    }

    async fn current(&self) -> Result<ProfileCurrentReport> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let active = mgr.resolve_active(&conn, &me)?;
        let active_id = active.as_ref().map(|p| p.id.clone());
        Ok(ProfileCurrentReport {
            active: active.map(|p| Self::summary(&p, active_id.as_deref())),
        })
    }

    async fn create(&self, name: &str, description: Option<&str>) -> Result<ProfileDetail> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let p = mgr.create(&conn, &me, name, description)?;
        Ok(ProfileDetail {
            id: p.id,
            name: p.name,
            owner_user_id: p.owner_user_id,
            description: p.description,
            root: p.root.display().to_string(),
            access: "owner".into(),
        })
    }

    async fn delete(&self, spec: &str) -> Result<ProfileMutationResult> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let p = mgr
            .resolve_spec(&conn, &me, spec)?
            .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
        mgr.delete(&conn, &p.id, &me)?;
        Ok(ProfileMutationResult {
            id: p.id,
            name: p.name,
            changed: true,
        })
    }

    async fn use_profile(&self, spec: &str) -> Result<ProfileMutationResult> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let p = mgr
            .resolve_spec(&conn, &me, spec)?
            .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
        mgr.set_active(&conn, &me, &p.id)?;
        Ok(ProfileMutationResult {
            id: p.id,
            name: p.name,
            changed: true,
        })
    }

    async fn share(&self, spec: &str, user: &str, role: &str) -> Result<ProfileMutationResult> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let p = mgr
            .resolve_spec(&conn, &me, spec)?
            .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
        let role_enum = Role::parse(role)
            .ok_or_else(|| anyhow!("invalid role: {role} (want viewer|collaborator)"))?;
        mgr.share(&conn, &p.id, &me, user, role_enum)?;
        Ok(ProfileMutationResult {
            id: p.id,
            name: p.name,
            changed: true,
        })
    }

    async fn unshare(&self, spec: &str, user: &str) -> Result<ProfileMutationResult> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let p = mgr
            .resolve_spec(&conn, &me, spec)?
            .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
        let removed = mgr.unshare(&conn, &p.id, &me, user)?;
        Ok(ProfileMutationResult {
            id: p.id,
            name: p.name,
            changed: removed,
        })
    }

    async fn shares(&self, spec: &str) -> Result<ProfileSharesReport> {
        let (conn, mgr) = self.open()?;
        let me = user_id();
        let p = mgr
            .resolve_spec(&conn, &me, spec)?
            .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
        let shares = mgr
            .list_shares(&conn, &p.id, &me)?
            .into_iter()
            .map(|(user_id, role)| ProfileShareEntry {
                user_id,
                role: role.as_str().into(),
            })
            .collect();
        Ok(ProfileSharesReport {
            profile_id: p.id,
            shares,
        })
    }
}
