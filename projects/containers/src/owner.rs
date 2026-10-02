//! Find the system a container lives on, so the caller never has to name one.
//!
//! Before this, `container.list` and `container.detail` answered only for the
//! local host. Seeing a container on another system meant `--peer <host>` —
//! the caller choosing where to run, by name, which is exactly what #647
//! removes. Deleting the flag without this would not tidy an interface, it
//! would delete the capability.
//!
//! So the addressing is inverted: a caller names the CONTAINER, and finding it
//! is orca's problem. A container id is runtime-native and only unique within
//! its host, so resolution is a search — ask each system what it has, and the
//! one that reports the id owns it.
//!
//! Two rules this encodes, both of them about not lying:
//!
//! - **Local first, and local alone when it hits.** A container on this host
//!   resolves with no mesh traffic at all. The search is what an addressed
//!   read costs when the answer is elsewhere, never what a local read costs.
//! - **An unreachable system yields no answer, never a wrong one.** A system
//!   that cannot be asked is reported as unreachable. It is NOT evidence that
//!   the container is absent, and a search that quietly treated it as such
//!   would report "no such container" about a host it never managed to ask.

use std::sync::Arc;
use std::time::Duration;

pub use contract::owner::{Answers, answers_for};

/// How long to wait on one system during an owner search.
///
/// A reachable system answers `container.list` well inside this. The bound is
/// what keeps an addressed read fast when a system is down: it resolves to
/// "could not ask", not to a hang. Systems are searched concurrently, so this
/// bounds the whole search, not each hop in sequence.
pub const OWNER_SEARCH_TIMEOUT_SECS: u64 = 5;

/// Outcome of searching the mesh for the system that owns a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// This system has it. The termination point — answer here.
    Locally,
    /// Exactly one other system reported it. Its stable machine id.
    On(String),
    /// No system that could be reached reported it. `unreachable` lists the
    /// systems that could not be asked, because "not found" means something
    /// different when part of the mesh was never searched.
    Missing { unreachable: Vec<String> },
    /// More than one system reported this id. Runtime-native ids are unique
    /// only within a host, so this is expected — two Proxmox nodes can both
    /// hold a CT 101. Refuse rather than pick: acting on the wrong one of
    /// these is a mutation on a machine the operator did not mean.
    Ambiguous { systems: Vec<String> },
}

/// Does this system hold a container with `id`?
///
/// Matches the runtime-native id or the container's name, because an operator
/// addresses the one they know and both name the same container.
pub fn held_locally(rows: &[crate::Container], id: &str) -> bool {
    let id = id.trim();
    rows.iter()
        .any(|c| c.id.eq_ignore_ascii_case(id) || c.name.eq_ignore_ascii_case(id))
}

/// The transport's view of the mesh.
///
/// `None` means there is NO mesh transport on this call at all — a standalone
/// install, a plugin subprocess, a unit test. That is not a failure: such a
/// process can see exactly one system, and this one is it.
///
/// `Some(vec)` means a transport answered. An empty vec from a live transport
/// is a real empty roster. The two are kept apart deliberately: collapsing
/// them would let a transient roster-read failure on a meshed host read as
/// "no other systems exist", and an action aimed at another host would then
/// quietly land here — the exact misapplication this module exists to stop.
async fn mesh(ctx: &contract::ToolCtx) -> Option<Vec<contract::PeerRef>> {
    let svc = ctx.service::<Arc<dyn contract::RemoteExec>>().ok()?;
    // A transport that cannot produce a roster is still a transport. Report
    // an empty roster rather than "no mesh": the caller must not conclude it
    // is standalone on the strength of a failed read.
    Some(svc.peers().await.unwrap_or_default())
}

/// Ask one system for its containers, under the search timeout.
///
/// `Err` means the system could not be asked — unreachable, too slow, or it
/// answered with something undecodable. That is deliberately not the same as
/// an empty list: one says we do not know, the other says we looked.
async fn containers_on(
    peer: &str,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<Vec<crate::Container>> {
    let svc = ctx.service::<Arc<dyn contract::RemoteExec>>()?;
    // Ask for every container, not just running ones: a stopped container is
    // still owned by its host, and `container.detail --view logs` on a stopped
    // one is a common thing to want.
    #[allow(clippy::disallowed_types)]
    let args = serde_json::json!({ "all": true });
    let value = tokio::time::timeout(
        Duration::from_secs(OWNER_SEARCH_TIMEOUT_SECS),
        svc.exec(
            peer,
            <crate::ContainersList as contract::OrcaToolDef>::NAME,
            args,
            ctx.caller(),
            ctx.correlation_id().map(str::to_string),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("did not answer within {OWNER_SEARCH_TIMEOUT_SECS}s"))??;
    #[allow(clippy::disallowed_types)]
    let out: crate::ContainersListOutput = serde_json::from_value(value)?;
    Ok(out.containers)
}

/// Classify a completed search. Pure, so every outcome — including the two
/// that matter most, a partial search and an ambiguous id — is testable
/// without a mesh.
///
/// `hits` are the systems that reported the container; `unreachable` the ones
/// that could not be asked. A hit wins over an unreachable system: the
/// container was found, and the systems we failed to reach cannot make a
/// positive answer less true. Nothing found with a gap in the search is
/// `Missing` carrying that gap, never a bare "no".
pub fn classify(mut hits: Vec<String>, unreachable: Vec<String>) -> Found {
    hits.sort();
    hits.dedup();
    match hits.len() {
        0 => Found::Missing { unreachable },
        1 => Found::On(hits.remove(0)),
        _ => Found::Ambiguous { systems: hits },
    }
}

/// Find the system that owns the container named `id`.
///
/// `local_rows` is this system's own container list, already in hand at every
/// call site — passing it in keeps the local hit free rather than re-listing.
pub async fn find(id: &str, local_rows: &[crate::Container], ctx: &contract::ToolCtx) -> Found {
    if held_locally(local_rows, id) {
        return Found::Locally;
    }
    let peers: Vec<contract::PeerRef> = mesh(ctx)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.is_local)
        .collect();
    if peers.is_empty() {
        return Found::Missing {
            unreachable: Vec::new(),
        };
    }
    // Concurrently: a search is read-only, so there is no ordering requirement
    // and no reason for one slow or down system to set the latency of the
    // whole answer.
    let mut set = tokio::task::JoinSet::new();
    for p in peers {
        let ctx = ctx.clone();
        let id = id.trim().to_string();
        set.spawn(async move {
            match containers_on(&p.id, &ctx).await {
                Ok(rows) => (held_locally(&rows, &id)).then_some(Ok(p.id)),
                Err(e) => Some(Err(format!("{}: {e:#}", p.name))),
            }
        });
    }
    let mut hits = Vec::new();
    let mut unreachable = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Some(Ok(peer_id))) => hits.push(peer_id),
            Ok(Some(Err(why))) => unreachable.push(why),
            // Reported nothing and did not fail: it genuinely does not have it.
            Ok(None) => {}
            // A panicked search task is a gap in the search, not a "no".
            Err(e) => unreachable.push(format!("search task failed: {e}")),
        }
    }
    unreachable.sort();
    classify(hits, unreachable)
}

/// Render a failed resolution as the error an operator acts on.
///
/// Says which systems went unsearched, so "no such container" is never
/// mistaken for "I searched everywhere".
pub fn unresolved(id: &str, found: &Found) -> anyhow::Error {
    match found {
        Found::Missing { unreachable } if unreachable.is_empty() => {
            anyhow::anyhow!("no container `{id}` on any system in the mesh")
        }
        Found::Missing { unreachable } => anyhow::anyhow!(
            "no container `{id}` on any system that could be searched; \
             NOT searched: {}",
            unreachable.join("; ")
        ),
        Found::Ambiguous { systems } => anyhow::anyhow!(
            "`{id}` names a container on more than one system ({}) — \
             container ids are unique only within a host. Narrow it with \
             `--system <id>`.",
            systems.join(", ")
        ),
        // Not a failure; callers match these before asking for an error.
        Found::Locally => anyhow::anyhow!("container `{id}` is on this system"),
        Found::On(s) => anyhow::anyhow!("container `{id}` is on system `{s}`"),
    }
}

/// Resolve a `--system <id>` selector against the mesh roster.
///
/// Accepts the stable machine id or the display name, because an operator
/// types the name they know. The name is a lookup key only: it resolves to an
/// id and the id is what gets addressed. A name matching more than one system
/// is refused rather than guessed — `orca system list` has been observed
/// returning the same display name twice, so the ambiguity is real.
///
/// This is naming the system as a RESOURCE ("containers belonging to baldur"),
/// not selecting a host to execute on. The distinction is the whole of #647:
/// the same call still runs wherever orca decides, and a system that is not in
/// the mesh is an error rather than a silent local answer.
pub async fn resolve_system(sel: &str, ctx: &contract::ToolCtx) -> anyhow::Result<Answers> {
    let sel = sel.trim();
    // No transport: this process can see exactly one system. The selector
    // names it or it names nothing, and `answers_for` makes that call without
    // inventing a host to dial. A standalone install must keep working.
    let Some(peers) = mesh(ctx).await else {
        return Ok(Answers::Locally);
    };
    if peers.is_empty() {
        anyhow::bail!(
            "cannot resolve system `{sel}`: the mesh roster came back empty,              so whether `{sel}` is this system or another one is unknown"
        );
    }
    let matched: Vec<&contract::PeerRef> = peers
        .iter()
        .filter(|p| p.id.eq_ignore_ascii_case(sel) || p.name.eq_ignore_ascii_case(sel))
        .collect();
    match matched.as_slice() {
        [] => anyhow::bail!(
            "no system `{sel}` in the mesh (known: {})",
            peers
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        [p] if p.is_local => Ok(Answers::Locally),
        [p] => Ok(Answers::Owner(p.id.clone())),
        many => anyhow::bail!(
            "`{sel}` names {} systems — address it by machine id instead",
            many.len()
        ),
    }
}

/// Every system in the mesh, as `(id, display name)`, local one included.
///
/// The local system is returned with an EMPTY id: it has no roster row of its
/// own and must never be dialed over the mesh. Callers answer for it in
/// process.
pub async fn all_systems(ctx: &contract::ToolCtx) -> Vec<(String, String)> {
    mesh(ctx)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| (if p.is_local { String::new() } else { p.id }, p.name))
        .collect()
}

/// Ask one system for its containers. Public so `container.list` can fan out
/// over the same bounded, decoded call the owner search uses — one path, so a
/// fan-out and a search can never disagree about what a system reported.
pub async fn list_on(peer: &str, ctx: &contract::ToolCtx) -> anyhow::Result<Vec<crate::Container>> {
    containers_on(peer, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str) -> crate::Container {
        crate::Container {
            id: id.to_string(),
            name: name.to_string(),
            runtime: crate::RuntimeKind::Docker,
            host: "testhost".to_string(),
            state: crate::ContainerState::Running,
            health: contract::health::Health::Unknown,
            restart_policy: crate::RestartPolicy::No,
            image: None,
            labels: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            exit_code: None,
            startup: None,
        }
    }

    #[test]
    fn a_container_is_addressable_by_id_or_by_name() {
        let rows = vec![row("abc123", "sonarr")];
        assert!(held_locally(&rows, "abc123"));
        assert!(held_locally(&rows, "sonarr"));
        // An operator types the case they remember.
        assert!(held_locally(&rows, "Sonarr"));
        assert!(!held_locally(&rows, "radarr"));
    }

    #[test]
    fn nothing_found_and_nothing_unreachable_is_a_real_absence() {
        assert_eq!(
            classify(Vec::new(), Vec::new()),
            Found::Missing {
                unreachable: Vec::new()
            }
        );
    }

    #[test]
    fn nothing_found_with_a_system_unsearched_carries_the_gap() {
        // The distinction this whole module exists for: "not there" and
        // "I could not look" must never render as the same answer.
        let found = classify(Vec::new(), vec!["freyr: timed out".into()]);
        let Found::Missing { unreachable } = &found else {
            panic!("expected Missing, got {found:?}");
        };
        assert_eq!(unreachable, &["freyr: timed out".to_string()]);
        let msg = unresolved("abc", &found).to_string();
        assert!(msg.contains("NOT searched"), "{msg}");
        assert!(msg.contains("freyr"), "{msg}");
    }

    #[test]
    fn a_hit_stands_even_when_another_system_could_not_be_asked() {
        // Failing to reach freyr cannot make baldur's positive answer
        // less true.
        assert_eq!(
            classify(vec!["baldur".into()], vec!["freyr: down".into()]),
            Found::On("baldur".into())
        );
    }

    #[test]
    fn the_same_id_on_two_systems_is_refused_not_guessed() {
        // Runtime-native ids are unique only within a host: two Proxmox nodes
        // can both hold CT 101. Picking one would be a mutation on a machine
        // the operator did not name.
        let found = classify(vec!["thor".into(), "frigg".into()], Vec::new());
        assert_eq!(
            found,
            Found::Ambiguous {
                systems: vec!["frigg".into(), "thor".into()]
            }
        );
        let msg = unresolved("101", &found).to_string();
        assert!(msg.contains("more than one system"), "{msg}");
        assert!(msg.contains("--system"), "{msg}");
    }

    #[test]
    fn one_system_reporting_twice_is_one_hit_not_an_ambiguity() {
        assert_eq!(
            classify(vec!["baldur".into(), "baldur".into()], Vec::new()),
            Found::On("baldur".into())
        );
    }
}
