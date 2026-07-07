//! Native implementations called directly from the `#[orca_tool]` entry
//! points in `lib.rs`. No service trait, no embedder indirection.
//!
//! Moved from `platform::profile_native::ServerProfile` in slice 3.

use crate::manager::{NamespaceManager, Role};
use crate::{
    NamespaceDetail, NamespaceListReport, NamespaceMutationResult, NamespaceShareEntry,
    NamespaceSharesReport, NamespaceSummary,
};
use anyhow::{Context, Result, anyhow};
use contract::config::{Config, LOCAL_USER};

fn user_id() -> String {
    LOCAL_USER.to_string()
}

fn open(cfg: &Config) -> Result<(rusqlite::Connection, NamespaceManager)> {
    let conn = db::open(&cfg.db_path).context("open orca.db")?;
    let mgr = NamespaceManager::from_config(cfg);
    Ok((conn, mgr))
}

fn summary(p: &crate::manager::Namespace, active_id: Option<&str>) -> NamespaceSummary {
    NamespaceSummary {
        id: p.id.clone(),
        name: p.name.clone(),
        owner_user_id: p.owner_user_id.clone(),
        is_active: active_id == Some(p.id.as_str()),
    }
}

pub async fn list(cfg: &Config) -> Result<NamespaceListReport> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let namespaces = mgr.list_for_user(&conn, &me)?;
    let active = db::profiles::get_active(&conn, &me).ok().flatten();
    let summaries = namespaces
        .iter()
        .map(|p| summary(p, active.as_deref()))
        .collect();
    Ok(NamespaceListReport {
        namespaces: summaries,
    })
}

pub async fn show(cfg: &Config, spec: Option<&str>) -> Result<NamespaceDetail> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = match spec {
        Some(s) => mgr
            .resolve_spec(&conn, &me, s)?
            .ok_or_else(|| anyhow!("namespace not found: {s}"))?,
        None => mgr
            .resolve_active(&conn, &me)?
            .ok_or_else(|| anyhow!("no active namespace"))?,
    };
    let access = mgr.access(&conn, &p.id, &me)?;
    Ok(NamespaceDetail {
        id: p.id,
        name: p.name,
        owner_user_id: p.owner_user_id,
        description: p.description,
        root: p.root.display().to_string(),
        access: format!("{access:?}").to_lowercase(),
    })
}

pub async fn create(
    cfg: &Config,
    name: &str,
    description: Option<&str>,
) -> Result<NamespaceDetail> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = mgr.create(&conn, &me, name, description)?;
    Ok(NamespaceDetail {
        id: p.id,
        name: p.name,
        owner_user_id: p.owner_user_id,
        description: p.description,
        root: p.root.display().to_string(),
        access: "owner".into(),
    })
}

pub async fn delete(cfg: &Config, spec: &str) -> Result<NamespaceMutationResult> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = mgr
        .resolve_spec(&conn, &me, spec)?
        .ok_or_else(|| anyhow!("namespace not found: {spec}"))?;
    mgr.delete(&conn, &p.id, &me)?;
    Ok(NamespaceMutationResult {
        id: p.id,
        name: p.name,
        changed: true,
    })
}

pub async fn use_namespace(cfg: &Config, spec: &str) -> Result<NamespaceMutationResult> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = mgr
        .resolve_spec(&conn, &me, spec)?
        .ok_or_else(|| anyhow!("namespace not found: {spec}"))?;
    mgr.set_active(&conn, &me, &p.id)?;
    Ok(NamespaceMutationResult {
        id: p.id,
        name: p.name,
        changed: true,
    })
}

pub async fn share(
    cfg: &Config,
    spec: &str,
    user: &str,
    role: &str,
) -> Result<NamespaceMutationResult> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = mgr
        .resolve_spec(&conn, &me, spec)?
        .ok_or_else(|| anyhow!("namespace not found: {spec}"))?;
    let role_enum = Role::parse(role)
        .ok_or_else(|| anyhow!("invalid role: {role} (want viewer|collaborator)"))?;
    mgr.share(&conn, &p.id, &me, user, role_enum)?;
    Ok(NamespaceMutationResult {
        id: p.id,
        name: p.name,
        changed: true,
    })
}

pub async fn unshare(cfg: &Config, spec: &str, user: &str) -> Result<NamespaceMutationResult> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = mgr
        .resolve_spec(&conn, &me, spec)?
        .ok_or_else(|| anyhow!("namespace not found: {spec}"))?;
    let removed = mgr.unshare(&conn, &p.id, &me, user)?;
    Ok(NamespaceMutationResult {
        id: p.id,
        name: p.name,
        changed: removed,
    })
}

pub async fn shares(cfg: &Config, spec: &str) -> Result<NamespaceSharesReport> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = mgr
        .resolve_spec(&conn, &me, spec)?
        .ok_or_else(|| anyhow!("namespace not found: {spec}"))?;
    let shares = mgr
        .list_shares(&conn, &p.id, &me)?
        .into_iter()
        .map(|(user_id, role)| NamespaceShareEntry {
            user_id,
            role: role.as_str().into(),
        })
        .collect();
    Ok(NamespaceSharesReport {
        namespace_id: p.id,
        shares,
    })
}
