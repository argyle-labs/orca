//! Native implementations called directly from the `#[orca_tool]` entry
//! points in `lib.rs`. No service trait, no embedder indirection.
//!
//! Moved from `platform::profile_native::ServerProfile` in slice 3.

use crate::manager::{NamespaceManager, Role};
use crate::{NamespaceDetail, NamespaceMutationResult, NamespaceShareEntry, NamespaceSummary};
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

pub async fn list(cfg: &Config) -> Result<Vec<NamespaceSummary>> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let namespaces = mgr.list_for_user(&conn, &me)?;
    let active = crate::profiles::get_active(&conn, &me).ok().flatten();
    let mut summaries: Vec<NamespaceSummary> = namespaces
        .iter()
        .map(|p| summary(p, active.as_deref()))
        .collect();
    summaries.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(summaries)
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

pub async fn shares(cfg: &Config, spec: &str) -> Result<(String, Vec<NamespaceShareEntry>)> {
    let (conn, mgr) = open(cfg)?;
    let me = user_id();
    let p = mgr
        .resolve_spec(&conn, &me, spec)?
        .ok_or_else(|| anyhow!("namespace not found: {spec}"))?;
    let mut shares: Vec<NamespaceShareEntry> = mgr
        .list_shares(&conn, &p.id, &me)?
        .into_iter()
        .map(|(user_id, role)| NamespaceShareEntry {
            user_id,
            role: role.as_str().into(),
        })
        .collect();
    shares.sort_by(|a, b| a.user_id.cmp(&b.user_id));
    Ok((p.id, shares))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // The `namespace` crate has no tokio dev-dependency and its `native::*`
    // functions are `async fn`s whose bodies contain no real `.await` points
    // (every call inside is synchronous). A single poll therefore drives each
    // to completion. This tiny no-op-waker executor lets plain `#[test]`s run
    // them without pulling in a runtime.
    fn block_on<F: Future>(fut: F) -> F::Output {
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = fut;
        // SAFETY: `fut` is owned and never moved after being pinned here.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    /// Extract an error message without requiring the `Ok` type to be `Debug`
    /// (the `Namespace*` result types intentionally don't derive `Debug`, so
    /// `Result::unwrap_err` is unavailable).
    fn err_str<T>(r: Result<T>) -> String {
        match r {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e.to_string(),
        }
    }

    /// A `Config` whose `app_dir` (→ `profiles_dir`) and `db_path` live inside a
    /// fresh tempdir, so every test operates on an isolated encrypted store and
    /// filesystem tree. The `TempDir` guard is returned to keep it alive.
    fn test_cfg() -> (Config, tempfile::TempDir) {
        use contract::config::Model;
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: tmp.path().to_path_buf(),
            memory_root: tmp.path().to_path_buf(),
            db_path: tmp.path().join("orca.db"),
            ports: Default::default(),
        };
        (cfg, tmp)
    }

    #[test]
    fn create_then_show_and_list_roundtrip() {
        let (cfg, _tmp) = test_cfg();
        let detail = block_on(create(&cfg, "homelab", Some("home stuff"))).unwrap();
        assert_eq!(detail.name, "homelab");
        assert_eq!(detail.owner_user_id, LOCAL_USER);
        assert_eq!(detail.access, "owner");
        assert_eq!(detail.description.as_deref(), Some("home stuff"));

        // show by name resolves the same row.
        let shown = block_on(show(&cfg, Some("homelab"))).unwrap();
        assert_eq!(shown.id, detail.id);
        assert_eq!(shown.access, "owner");

        // list contains it; not active yet.
        let listed = block_on(list(&cfg)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, detail.id);
        assert_eq!(listed[0].owner_user_id, LOCAL_USER);
        assert!(!listed[0].is_active);
    }

    #[test]
    fn list_is_empty_on_fresh_store() {
        let (cfg, _tmp) = test_cfg();
        let listed = block_on(list(&cfg)).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn list_sorts_by_id() {
        let (cfg, _tmp) = test_cfg();
        block_on(create(&cfg, "alpha", None)).unwrap();
        block_on(create(&cfg, "bravo", None)).unwrap();
        block_on(create(&cfg, "charlie", None)).unwrap();
        let listed = block_on(list(&cfg)).unwrap();
        assert_eq!(listed.len(), 3);
        let mut ids: Vec<String> = listed.iter().map(|s| s.id.clone()).collect();
        let sorted = {
            let mut c = ids.clone();
            c.sort();
            c
        };
        assert_eq!(ids, sorted);
        ids.dedup();
        assert_eq!(ids.len(), 3, "ids must be unique");
    }

    #[test]
    fn show_without_active_errors() {
        // No namespaces exist, so `resolve_active` has nothing to fall back to.
        let (cfg, _tmp) = test_cfg();
        let err = err_str(block_on(show(&cfg, None)));
        assert!(err.contains("no active namespace"), "{err}");
    }

    #[test]
    fn show_none_falls_back_to_first_accessible() {
        // With no explicit active selection, `resolve_active` returns the first
        // accessible namespace rather than erroring.
        let (cfg, _tmp) = test_cfg();
        let d = block_on(create(&cfg, "homelab", None)).unwrap();
        let shown = block_on(show(&cfg, None)).unwrap();
        assert_eq!(shown.id, d.id);
    }

    #[test]
    fn show_unknown_spec_errors() {
        let (cfg, _tmp) = test_cfg();
        let err = err_str(block_on(show(&cfg, Some("ghost"))));
        assert!(err.contains("namespace not found: ghost"), "{err}");
    }

    #[test]
    fn use_namespace_sets_active_and_show_none_resolves_it() {
        let (cfg, _tmp) = test_cfg();
        let d = block_on(create(&cfg, "homelab", None)).unwrap();
        let res = block_on(use_namespace(&cfg, "homelab")).unwrap();
        assert!(res.changed);
        assert_eq!(res.id, d.id);

        // active now reflected in list and in show(None).
        let listed = block_on(list(&cfg)).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_active);

        let shown = block_on(show(&cfg, None)).unwrap();
        assert_eq!(shown.id, d.id);
    }

    #[test]
    fn use_unknown_spec_errors() {
        let (cfg, _tmp) = test_cfg();
        let err = err_str(block_on(use_namespace(&cfg, "ghost")));
        assert!(err.contains("namespace not found: ghost"), "{err}");
    }

    #[test]
    fn delete_removes_row_and_content() {
        let (cfg, _tmp) = test_cfg();
        let d = block_on(create(&cfg, "homelab", None)).unwrap();
        let root = PathBuf::from(&d.root);
        assert!(root.exists());

        let res = block_on(delete(&cfg, "homelab")).unwrap();
        assert!(res.changed);
        assert_eq!(res.id, d.id);
        assert_eq!(res.name, "homelab");
        assert!(!root.exists());

        // gone from listing and no longer resolvable.
        assert!(block_on(list(&cfg)).unwrap().is_empty());
        let err = err_str(block_on(show(&cfg, Some("homelab"))));
        assert!(err.contains("namespace not found"), "{err}");
    }

    #[test]
    fn delete_unknown_spec_errors() {
        let (cfg, _tmp) = test_cfg();
        let err = err_str(block_on(delete(&cfg, "ghost")));
        assert!(err.contains("namespace not found: ghost"), "{err}");
    }

    #[test]
    fn share_grants_role_and_shares_lists_it() {
        let (cfg, _tmp) = test_cfg();
        let d = block_on(create(&cfg, "homelab", None)).unwrap();

        let res = block_on(share(&cfg, "homelab", "bob", "viewer")).unwrap();
        assert!(res.changed);
        assert_eq!(res.id, d.id);

        let (ns_id, entries) = block_on(shares(&cfg, "homelab")).unwrap();
        assert_eq!(ns_id, d.id);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].user_id, "bob");
        assert_eq!(entries[0].role, "viewer");
    }

    #[test]
    fn share_collaborator_role_recorded() {
        let (cfg, _tmp) = test_cfg();
        block_on(create(&cfg, "homelab", None)).unwrap();
        block_on(share(&cfg, "homelab", "carol", "collaborator")).unwrap();
        let (_id, entries) = block_on(shares(&cfg, "homelab")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].user_id, "carol");
        assert_eq!(entries[0].role, "collaborator");
    }

    #[test]
    fn shares_are_sorted_by_user_id() {
        let (cfg, _tmp) = test_cfg();
        block_on(create(&cfg, "homelab", None)).unwrap();
        block_on(share(&cfg, "homelab", "zoe", "viewer")).unwrap();
        block_on(share(&cfg, "homelab", "amy", "viewer")).unwrap();
        block_on(share(&cfg, "homelab", "max", "viewer")).unwrap();
        let (_id, entries) = block_on(shares(&cfg, "homelab")).unwrap();
        let users: Vec<String> = entries.iter().map(|e| e.user_id.clone()).collect();
        assert_eq!(users, vec!["amy", "max", "zoe"]);
    }

    #[test]
    fn share_invalid_role_errors() {
        let (cfg, _tmp) = test_cfg();
        block_on(create(&cfg, "homelab", None)).unwrap();
        let err = err_str(block_on(share(&cfg, "homelab", "bob", "admin")));
        assert!(err.contains("invalid role: admin"), "{err}");
        assert!(err.contains("viewer|collaborator"), "{err}");
    }

    #[test]
    fn share_unknown_spec_errors() {
        let (cfg, _tmp) = test_cfg();
        let err = err_str(block_on(share(&cfg, "ghost", "bob", "viewer")));
        assert!(err.contains("namespace not found: ghost"), "{err}");
    }

    #[test]
    fn unshare_removes_grant() {
        let (cfg, _tmp) = test_cfg();
        block_on(create(&cfg, "homelab", None)).unwrap();
        block_on(share(&cfg, "homelab", "bob", "viewer")).unwrap();

        let res = block_on(unshare(&cfg, "homelab", "bob")).unwrap();
        assert!(res.changed);
        let (_id, entries) = block_on(shares(&cfg, "homelab")).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn unshare_absent_user_reports_unchanged() {
        let (cfg, _tmp) = test_cfg();
        block_on(create(&cfg, "homelab", None)).unwrap();
        let res = block_on(unshare(&cfg, "homelab", "nobody")).unwrap();
        assert!(!res.changed);
    }

    #[test]
    fn unshare_unknown_spec_errors() {
        let (cfg, _tmp) = test_cfg();
        let err = err_str(block_on(unshare(&cfg, "ghost", "bob")));
        assert!(err.contains("namespace not found: ghost"), "{err}");
    }

    #[test]
    fn shares_unknown_spec_errors() {
        let (cfg, _tmp) = test_cfg();
        let err = err_str(block_on(shares(&cfg, "ghost")));
        assert!(err.contains("namespace not found: ghost"), "{err}");
    }
}
