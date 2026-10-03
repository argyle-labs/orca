//! Ask every system in the mesh the same question, concurrently.
//!
//! The addressing rule orca is built on: **a caller names a resource, never a
//! host.** Every machine exposes the same API, ids are the handle, and the
//! mesh dispatches. `peer` is transport plumbing underneath — it must never
//! surface as something an operator types.
//!
//! A domain that reads only its local registry quietly breaks that rule: the
//! surface is then one host's, and seeing the rest of the fleet means naming
//! a host, which is exactly what #647 removes. `containers` grew this fan-out
//! first; `backup` needs the same shape, so it lives here rather than being
//! copied — one implementation, so two domains cannot disagree about what a
//! system reported or about what an unreachable one means.
//!
//! Two invariants, both about not lying:
//!
//! - **The local system is answered in process, never dialed.** It has no
//!   roster row of its own; a host calling itself over the mesh is a bug.
//! - **An unreachable system is reported, never silently dropped.** It lands
//!   in `system_errors`, because "the fleet has none of these" means something
//!   different when part of the fleet was never asked.

use crate::ToolCtx;
use std::sync::Arc;

/// Every system in the mesh, as `(id, display name)`, the local one included.
///
/// The local system carries an EMPTY id — it must be answered in process.
///
/// `None` from the transport means there is no mesh at all (a standalone
/// install, a plugin subprocess, a test double). That is NOT the same as an
/// empty roster, and callers must treat it as "answer locally" rather than
/// "the fleet is empty".
pub async fn systems(ctx: &ToolCtx) -> Vec<(String, String)> {
    let Ok(svc) = ctx.service::<Arc<dyn crate::RemoteExec>>() else {
        return Vec::new();
    };
    svc.peers()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| (if p.is_local { String::new() } else { p.id }, p.name))
        .collect()
}

/// Dispatch one tool to one system and decode its typed output.
///
/// Every fan-out needs the same four steps — resolve the transport, encode the
/// args, name the tool from its `OrcaToolDef`, decode the reply — and each
/// domain had been writing them out again. `containers::exec_list_at` and the
/// backup providers call were the same function with different types.
///
/// Forwarding the caller's identity and correlation id is not optional: the
/// recipient re-applies role checks against the ORIGINAL caller, and a hop
/// that dropped the correlation id would break tracing one action across the
/// hosts it touched.
pub async fn exec_at<D>(peer: &str, args: &D::Args, ctx: &ToolCtx) -> anyhow::Result<D::Output>
where
    D: crate::OrcaToolDef,
    D::Args: serde::Serialize,
    D::Output: serde::de::DeserializeOwned,
{
    let svc = ctx.service::<Arc<dyn crate::RemoteExec>>()?;
    #[allow(clippy::disallowed_types)]
    let payload = serde_json::to_value(args)?;
    let value = svc
        .exec(
            peer,
            D::NAME,
            payload,
            ctx.caller(),
            ctx.correlation_id().map(str::to_string),
        )
        .await?;
    #[allow(clippy::disallowed_types)]
    Ok(serde_json::from_value(value)?)
}

/// What a fan-out gathered: the merged rows, and the systems that could not
/// be asked.
#[derive(Debug, Default)]
pub struct Gathered<T> {
    pub rows: Vec<T>,
    /// One entry per system that failed, prefixed with its display name so an
    /// operator can tell WHICH host is missing from the answer. Sorted, so the
    /// output is stable between calls.
    pub system_errors: Vec<String>,
}

/// Run `call` against every REMOTE system concurrently and merge the results.
///
/// The caller supplies the local rows — the local system is never dialed, and
/// answering it in process is what keeps a purely-local read costing no mesh
/// traffic at all.
///
/// `call` receives a system's stable machine id. A failure becomes a
/// `system_errors` entry rather than failing the whole read: one unreachable
/// host must not hide what the others hold.
pub async fn gather<T, F, Fut>(ctx: &ToolCtx, local: Vec<T>, call: F) -> Gathered<T>
where
    T: Send + 'static,
    F: Fn(String, ToolCtx) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<Vec<T>>> + Send,
{
    let call = Arc::new(call);
    let mut set = tokio::task::JoinSet::new();
    for (id, name) in systems(ctx)
        .await
        .into_iter()
        .filter(|(id, _)| !id.is_empty())
    {
        let ctx = ctx.clone();
        let call = Arc::clone(&call);
        set.spawn(async move {
            match call(id, ctx).await {
                Ok(rows) => Ok(rows),
                // Named, because an error that does not say which system it is
                // about cannot be acted on.
                Err(e) => Err(format!("{name}: {e:#}")),
            }
        });
    }

    let mut rows = local;
    let mut system_errors = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(mut got)) => rows.append(&mut got),
            Ok(Err(why)) => system_errors.push(why),
            Err(e) => system_errors.push(format!("fan-out task failed: {e}")),
        }
    }
    system_errors.sort();
    Gathered {
        rows,
        system_errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallerIdentity, PeerRef, RemoteExec};
    use std::sync::Mutex;

    struct Roster {
        peers: Vec<PeerRef>,
        /// Ids actually dialed, so a test can prove the local one was not.
        dialed: Mutex<Vec<String>>,
        fail: Option<String>,
    }

    #[async_trait::async_trait]
    impl RemoteExec for Roster {
        async fn peers(&self) -> anyhow::Result<Vec<PeerRef>> {
            Ok(self.peers.clone())
        }
        #[allow(clippy::disallowed_types)]
        async fn exec(
            &self,
            peer: &str,
            _tool: &str,
            _args: serde_json::Value,
            _caller: Option<CallerIdentity>,
            _correlation_id: Option<String>,
        ) -> anyhow::Result<serde_json::Value> {
            self.dialed.lock().unwrap().push(peer.to_string());
            if self.fail.as_deref() == Some(peer) {
                anyhow::bail!("unreachable");
            }
            Ok(serde_json::Value::Null)
        }
    }

    fn peer(id: &str, name: &str, is_local: bool) -> PeerRef {
        PeerRef {
            id: id.into(),
            name: name.into(),
            is_local,
        }
    }

    fn cfg() -> Arc<crate::config::Config> {
        use std::path::PathBuf;
        Arc::new(crate::config::Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: crate::config::Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Default::default(),
        })
    }

    fn ctx_with(roster: Arc<Roster>) -> ToolCtx {
        let mut ctx = ToolCtx::new(cfg());
        ctx.register_service::<Arc<dyn RemoteExec>>(roster as Arc<dyn RemoteExec>);
        ctx
    }

    #[tokio::test]
    async fn the_local_system_is_listed_with_an_empty_id() {
        let r = Arc::new(Roster {
            peers: vec![peer("", "mint", true), peer("id-w", "willow", false)],
            dialed: Mutex::new(Vec::new()),
            fail: None,
        });
        let got = systems(&ctx_with(r)).await;
        assert!(got.contains(&(String::new(), "mint".to_string())));
        assert!(got.contains(&("id-w".to_string(), "willow".to_string())));
    }

    #[tokio::test]
    async fn no_transport_is_not_an_empty_fleet() {
        // A standalone install has no mesh. Callers must answer locally, not
        // conclude the fleet is empty — so this returns nothing and `gather`
        // still hands back exactly the local rows.
        let ctx = ToolCtx::new(cfg());
        assert!(systems(&ctx).await.is_empty());
        let out = gather(&ctx, vec![1, 2], |_, _| async { Ok(vec![99]) }).await;
        assert_eq!(out.rows, vec![1, 2], "local rows must survive untouched");
        assert!(out.system_errors.is_empty());
    }

    #[tokio::test]
    async fn the_local_system_is_never_dialed() {
        let r = Arc::new(Roster {
            peers: vec![peer("", "mint", true), peer("id-w", "willow", false)],
            dialed: Mutex::new(Vec::new()),
            fail: None,
        });
        let ctx = ctx_with(Arc::clone(&r));
        let out = gather(&ctx, vec!["local"], |id, ctx| async move {
            let svc = ctx.service::<Arc<dyn RemoteExec>>()?;
            svc.exec(&id, "t", serde_json::Value::Null, None, None)
                .await?;
            Ok(vec!["remote"])
        })
        .await;
        let dialed = r.dialed.lock().unwrap().clone();
        assert_eq!(dialed, vec!["id-w"], "only the remote system may be dialed");
        assert!(out.rows.contains(&"local"));
        assert!(out.rows.contains(&"remote"));
    }

    #[tokio::test]
    async fn one_unreachable_system_does_not_hide_the_others() {
        let r = Arc::new(Roster {
            peers: vec![
                peer("", "mint", true),
                peer("id-w", "willow", false),
                peer("id-f", "freyr", false),
            ],
            dialed: Mutex::new(Vec::new()),
            fail: Some("id-f".into()),
        });
        let ctx = ctx_with(r);
        let out = gather(&ctx, vec!["local".to_string()], |id, ctx| async move {
            let svc = ctx.service::<Arc<dyn RemoteExec>>()?;
            svc.exec(&id, "t", serde_json::Value::Null, None, None)
                .await?;
            Ok(vec![format!("row-from-{id}")])
        })
        .await;
        assert!(out.rows.contains(&"row-from-id-w".to_string()));
        assert_eq!(out.system_errors.len(), 1);
        assert!(
            out.system_errors[0].starts_with("freyr:"),
            "the error must name the system: {:?}",
            out.system_errors
        );
    }
}
