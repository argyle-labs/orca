//! Cross-crate share-permission introspection types and provider registry.
//!
//! A permissions provider exposes how to read a filesystem path's permissions
//! and enumerate its reference peers (sibling shares) — the raw material the
//! write-denied share repair uses to *detect what comparable shares are doing*
//! and present a candidate mode to confirm, rather than ever guessing.
//!
//! Introspection is a plugin-exposable capability: a filesystem plugin (unraid,
//! and later others) registers a [`PermissionsProvider`] so the host that serves
//! a share owns the answer for its own filesystem. Core ships a default POSIX
//! provider (registered natively by the daemon) so the capability works before
//! any plugin loads. Providers register either in-process or, for a subprocess
//! plugin, via the [`register_from_def`] JSON proxy the plugin-loader installs
//! for `domain = "permissions"` — the same shape the `diagnostics` domain uses.

// The erased-invoke boundary carries args/results/errors as `serde_json::Value`,
// the sanctioned opaque seam scoped to this module's proxy.
#![allow(clippy::disallowed_types)]

use std::sync::{Arc, LazyLock, RwLock};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// A path's POSIX permission facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermInfo {
    /// Permission bits (the low 12: rwx triples + setuid/setgid/sticky).
    pub mode: u32,
    /// Owning user id.
    pub uid: u32,
    /// Owning group id.
    pub gid: u32,
}

/// A candidate mode inferred from reference peers — "N comparable shares use this
/// mode, here are examples". The repair presents these, ranked by evidence, for a
/// human to confirm; it is never applied automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermCandidate {
    /// The mode the reference peers carry.
    pub mode: u32,
    /// How many reference peers use it (the evidence weight).
    pub count: usize,
    /// A few example peer paths that use it, so the choice is auditable.
    pub sample_paths: Vec<String>,
}

/// A filesystem's permission-introspection capability. Registered per backend so
/// core never hardcodes one filesystem's rules; the host serving a share owns the
/// answer for its own filesystem. A provider that does not own `path` returns
/// `None` from [`read`](PermissionsProvider::read), and the resolver moves on —
/// so ownership needs no separate synchronous predicate that would have to cross
/// the FFI boundary.
pub trait PermissionsProvider: Send + Sync {
    /// Registry name (e.g. `"posix"`, `"unraid"`). The default fallback is named
    /// `"posix"` and is always tried last.
    fn name(&self) -> &str;
    /// Read `path`'s current permissions, or `None` if this provider does not own
    /// / cannot read it.
    fn read(&self, path: &str) -> BoxFuture<'_, Option<PermInfo>>;
    /// Enumerate the reference peers of `path` (its sibling shares) with their
    /// permissions — the raw material for candidate detection.
    fn reference_peers(&self, path: &str) -> BoxFuture<'_, Vec<(String, PermInfo)>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn PermissionsProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a permissions provider (replace-in-place by name, mirroring the
/// storage/diagnostics registries so a plugin reload doesn't duplicate it).
pub fn register_provider(provider: Arc<dyn PermissionsProvider>) {
    let mut g = GLOBAL.write().expect("permissions registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn PermissionsProvider>> {
    GLOBAL
        .read()
        .expect("permissions registry poisoned")
        .clone()
}

/// Deregister the provider named `name` (on plugin unload). Returns whether one
/// was removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("permissions registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

/// Resolve the provider that owns `path` and its current perms: try every
/// registered provider except the `"posix"` fallback first (a specific plugin,
/// e.g. unraid, owns its filesystem), then the fallback. The first whose `read`
/// returns `Some` owns the path.
async fn resolve(path: &str) -> Option<(Arc<dyn PermissionsProvider>, PermInfo)> {
    let all = providers();
    for specific in all.iter().filter(|p| p.name() != "posix") {
        if let Some(info) = specific.read(path).await {
            return Some((specific.clone(), info));
        }
    }
    for fallback in all.iter().filter(|p| p.name() == "posix") {
        if let Some(info) = fallback.read(path).await {
            return Some((fallback.clone(), info));
        }
    }
    None
}

/// Read a path's current permissions via its owning provider.
pub async fn read(path: &str) -> Option<PermInfo> {
    resolve(path).await.map(|(_, info)| info)
}

/// Detect candidate modes for `path` from its reference peers, ranked by evidence
/// (most-used first). Only peers that differ from `path`'s current mode are
/// offered. Returns an empty list when there is no owning provider or no peer
/// evidence — never a guess.
pub async fn detect_candidates(path: &str) -> Vec<PermCandidate> {
    let Some((provider, current)) = resolve(path).await else {
        return Vec::new();
    };
    let peers = provider.reference_peers(path).await;
    rank_candidates(peers, Some(current.mode))
}

/// Pure ranking core: fold reference peers into candidates by mode, drop the mode
/// the target already has, and order by evidence weight (count desc, then mode for
/// determinism). Split out so it is unit-testable without a filesystem.
pub fn rank_candidates(
    peers: Vec<(String, PermInfo)>,
    current_mode: Option<u32>,
) -> Vec<PermCandidate> {
    use std::collections::BTreeMap;
    let mut by_mode: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (peer_path, info) in peers {
        if Some(info.mode) == current_mode {
            continue; // a peer that already matches the target is not a suggestion
        }
        by_mode.entry(info.mode).or_default().push(peer_path);
    }
    let mut candidates: Vec<PermCandidate> = by_mode
        .into_iter()
        .map(|(mode, mut paths)| {
            paths.sort();
            let count = paths.len();
            paths.truncate(3); // a few examples for auditability
            PermCandidate {
                mode,
                count,
                sample_paths: paths,
            }
        })
        .collect();
    candidates.sort_by(|a, b| b.count.cmp(&a.count).then(a.mode.cmp(&b.mode)));
    candidates
}

// ── plugin proxy boundary (mirrors `diagnostics`) ─────────────────────────────

/// Operation names the [`PermissionsProxy`] invokes across the FFI boundary. The
/// plugin exposes tools `"{invoke_prefix}.{READ_OP|REFERENCE_PEERS_OP}"`.
pub const READ_OP: &str = "read";
pub const REFERENCE_PEERS_OP: &str = "reference_peers";

/// Plugin-side dispatch: answer a proxied permissions op by calling the typed
/// [`PermissionsProvider`]. Both ops take the target path as a bare JSON string.
pub async fn dispatch_op(
    provider: &dyn PermissionsProvider,
    op: &str,
    args: serde_json::Value,
) -> std::result::Result<serde_json::Value, serde_json::Value> {
    fn err(msg: impl Into<String>) -> serde_json::Value {
        serde_json::Value::String(msg.into())
    }
    let path: String =
        serde_json::from_value(args).map_err(|e| err(format!("decode {op} path arg: {e}")))?;
    match op {
        READ_OP => {
            let out = provider.read(&path).await;
            serde_json::to_value(out).map_err(|e| err(e.to_string()))
        }
        REFERENCE_PEERS_OP => {
            let out = provider.reference_peers(&path).await;
            serde_json::to_value(out).map_err(|e| err(e.to_string()))
        }
        other => Err(err(format!("unknown permissions op: {other}"))),
    }
}

/// The erased-invoke thunk a loaded plugin exposes; args/results carried as
/// `serde_json::Value`, keeping `contract` free of any ABI/loader dependency.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk = Arc<
    dyn Fn(&str, serde_json::Value) -> std::result::Result<serde_json::Value, serde_json::Value>
        + Send
        + Sync
        + 'static,
>;

/// Build and register a [`PermissionsProvider`] from a plugin backend descriptor
/// plus an [`InvokeThunk`]. The plugin-loader calls this for `domain =
/// "permissions"`.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> anyhow::Result<()> {
    register_provider(Arc::new(PermissionsProxy { name, invoke }));
    Ok(())
}

/// A [`PermissionsProvider`] backed by a subprocess plugin reached over the
/// JSON-proxy FFI boundary. Each op offloads the synchronous [`InvokeThunk`] onto
/// `spawn_blocking` and (de)serializes JSON at the seam.
#[cfg(feature = "in-process")]
struct PermissionsProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl PermissionsProxy {
    fn call<T: for<'de> Deserialize<'de> + Default + Send + 'static>(
        &self,
        op: &'static str,
        path: &str,
    ) -> BoxFuture<'_, T> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let args = serde_json::Value::String(path.to_string());
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || invoke(op, args)).await;
            match result {
                Ok(Ok(out)) => serde_json::from_value(out).unwrap_or_default(),
                Ok(Err(e)) => {
                    tracing::debug!(
                        "permissions '{name}' {op} failed: {}",
                        crate::render_invoke_error(&e)
                    );
                    T::default()
                }
                Err(e) => {
                    tracing::debug!("permissions '{name}' {op} task panicked: {e}");
                    T::default()
                }
            }
        })
    }
}

#[cfg(feature = "in-process")]
impl PermissionsProvider for PermissionsProxy {
    fn name(&self) -> &str {
        &self.name
    }
    fn read(&self, path: &str) -> BoxFuture<'_, Option<PermInfo>> {
        self.call(READ_OP, path)
    }
    fn reference_peers(&self, path: &str) -> BoxFuture<'_, Vec<(String, PermInfo)>> {
        self.call(REFERENCE_PEERS_OP, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(path: &str, mode: u32) -> (String, PermInfo) {
        (
            path.to_string(),
            PermInfo {
                mode,
                uid: 99,
                gid: 100,
            },
        )
    }

    #[test]
    fn rank_candidates_orders_by_evidence_and_drops_current() {
        let peers = vec![
            peer("/mnt/user/media", 0o777),
            peer("/mnt/user/downloads", 0o777),
            peer("/mnt/user/appdata", 0o770),
            peer("/mnt/user/isos", 0o775), // == current: dropped
        ];
        let got = rank_candidates(peers, Some(0o775));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].mode, 0o777);
        assert_eq!(got[0].count, 2);
        assert_eq!(got[1].mode, 0o770);
    }

    #[test]
    fn rank_candidates_empty_when_all_peers_match_current() {
        let peers = vec![peer("/a", 0o777), peer("/b", 0o777)];
        assert!(rank_candidates(peers, Some(0o777)).is_empty());
    }

    #[test]
    fn rank_candidates_samples_capped_and_sorted() {
        let peers = vec![
            peer("/d", 0o777),
            peer("/a", 0o777),
            peer("/c", 0o777),
            peer("/b", 0o777),
        ];
        let got = rank_candidates(peers, None);
        assert_eq!(got[0].count, 4);
        assert_eq!(got[0].sample_paths, vec!["/a", "/b", "/c"]);
    }
}
