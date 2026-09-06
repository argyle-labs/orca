//! Share permission introspection — the "detect what other shares are doing"
//! capability behind the write-denied repair.
//!
//! Repairing a permission-drifted share must **never guess** a mode. Instead we
//! observe what comparable shares actually use and present the most-evidenced
//! mode as a *candidate* for a human to confirm before anything is applied.
//!
//! Introspection is a plugin-exposable capability, not core-hardcoded Unraid
//! knowledge: filesystem backends register a [`PermissionsProvider`] that knows
//! how to read a path's permissions and enumerate reference peers for their
//! filesystem. Core ships a default POSIX provider (a plain local filesystem —
//! which is what an Unraid `/mnt/user/<share>` tree is), so the capability works
//! out of the box; smb/nfs/unraid/future-s3 can register richer providers (e.g.
//! reading server-reported ACLs) without core changing.

use std::sync::{Arc, LazyLock, RwLock};

use serde::{Deserialize, Serialize};

/// A path's POSIX permission facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermCandidate {
    /// The mode the reference peers carry.
    pub mode: u32,
    /// How many reference peers use it (the evidence weight).
    pub count: usize,
    /// A few example peer paths that use it, so the choice is auditable.
    pub sample_paths: Vec<String>,
}

/// A filesystem's permission-introspection capability. Registered per backend so
/// core never hardcodes one filesystem's rules. `supports` lets the resolver pick
/// the provider that owns a path; the default POSIX provider supports everything
/// as a fallback.
pub trait PermissionsProvider: Send + Sync {
    /// Registry name (e.g. `"posix"`, `"unraid"`).
    fn name(&self) -> &str;
    /// Does this provider own `path`? The resolver prefers a specific provider
    /// over the POSIX fallback.
    fn supports(&self, path: &str) -> bool;
    /// Read `path`'s current permissions, if it can.
    fn read(&self, path: &str) -> Option<PermInfo>;
    /// Enumerate the reference peers of `path` (its sibling shares) with their
    /// permissions — the raw material for candidate detection.
    fn reference_peers(&self, path: &str) -> Vec<(String, PermInfo)>;
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

/// Deregister the provider named `name` (on plugin unload). Returns whether one
/// was removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("permissions registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

/// Register the core-owned default POSIX provider. Called once at daemon boot so
/// the capability works before any filesystem plugin loads.
pub fn register_builtin() {
    register_provider(Arc::new(PosixPermissions));
}

/// Resolve the provider that owns `path`: the first registered provider whose
/// `supports` matches, else the POSIX fallback (which is also registered but may
/// be shadowed by a more specific provider).
fn provider_for(path: &str) -> Arc<dyn PermissionsProvider> {
    let g = GLOBAL.read().expect("permissions registry poisoned");
    g.iter()
        .find(|p| p.name() != "posix" && p.supports(path))
        .or_else(|| g.iter().find(|p| p.supports(path)))
        .cloned()
        .unwrap_or_else(|| Arc::new(PosixPermissions))
}

/// Read a path's current permissions via its owning provider.
pub fn read(path: &str) -> Option<PermInfo> {
    provider_for(path).read(path)
}

/// Detect candidate modes for `path` from its reference peers, ranked by evidence
/// (most-used first). Only peers that differ from `path`'s current mode are
/// offered — if the drifted share already matches its peers there is nothing to
/// suggest. Returns an empty list when no peer evidence exists (never guess).
pub fn detect_candidates(path: &str) -> Vec<PermCandidate> {
    let provider = provider_for(path);
    let current = provider.read(path).map(|p| p.mode);
    rank_candidates(provider.reference_peers(path), current)
}

/// Pure ranking core: fold reference peers into candidates by mode, drop the
/// mode the target already has, and order by evidence weight (count desc, then
/// mode for determinism). Split out so it is unit-testable without a filesystem.
fn rank_candidates(
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
    // Most evidence first; mode as the stable tiebreak.
    candidates.sort_by(|a, b| b.count.cmp(&a.count).then(a.mode.cmp(&b.mode)));
    candidates
}

/// Default provider: a plain local POSIX filesystem. Reference peers of a share
/// path are its sibling directories (the other shares under the same parent),
/// which is exactly the Unraid `/mnt/user/<share>` layout. Supports every path as
/// the universal fallback.
struct PosixPermissions;

impl PermissionsProvider for PosixPermissions {
    fn name(&self) -> &str {
        "posix"
    }

    fn supports(&self, _path: &str) -> bool {
        true
    }

    fn read(&self, path: &str) -> Option<PermInfo> {
        stat_perm(path)
    }

    fn reference_peers(&self, path: &str) -> Vec<(String, PermInfo)> {
        // The share root is the target's top-level share dir; its peers are the
        // sibling entries under the same parent. We compare at whatever level the
        // caller passed (a share root or a subpath) by using the path's parent.
        let p = std::path::Path::new(path);
        let Some(parent) = p.parent() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(parent) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|sib| sib.as_path() != p) // exclude the target itself
            .filter(|sib| sib.is_dir()) // shares are directories
            .filter_map(|sib| {
                let s = sib.to_string_lossy().to_string();
                stat_perm(&s).map(|info| (s, info))
            })
            .collect()
    }
}

/// Read POSIX mode/uid/gid for a path (no symlink following surprises: we stat
/// the path as given). `None` if it can't be stat'd.
fn stat_perm(path: &str) -> Option<PermInfo> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(PermInfo {
        mode: meta.permissions().mode() & 0o7777,
        uid: meta.uid(),
        gid: meta.gid(),
    })
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
            peer("/mnt/user/isos", 0o775), // == current: must be dropped
        ];
        let got = rank_candidates(peers, Some(0o775));
        // 0o777 (2 peers) ranks above 0o770 (1); 0o775 dropped as current.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].mode, 0o777);
        assert_eq!(got[0].count, 2);
        assert_eq!(got[1].mode, 0o770);
        assert!(!got.iter().any(|c| c.mode == 0o775));
    }

    #[test]
    fn rank_candidates_empty_when_all_peers_match_current() {
        let peers = vec![peer("/mnt/user/a", 0o777), peer("/mnt/user/b", 0o777)];
        assert!(rank_candidates(peers, Some(0o777)).is_empty());
    }

    #[test]
    fn rank_candidates_samples_are_capped_and_sorted() {
        let peers = vec![
            peer("/mnt/user/d", 0o777),
            peer("/mnt/user/a", 0o777),
            peer("/mnt/user/c", 0o777),
            peer("/mnt/user/b", 0o777),
        ];
        let got = rank_candidates(peers, None);
        assert_eq!(got[0].count, 4, "count reflects all peers");
        assert_eq!(
            got[0].sample_paths,
            vec![
                "/mnt/user/a".to_string(),
                "/mnt/user/b".to_string(),
                "/mnt/user/c".to_string()
            ],
            "samples sorted + capped to 3"
        );
    }

    #[cfg(unix)]
    #[test]
    fn posix_reference_peers_reads_sibling_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        for (name, mode) in [("data", 0o775u32), ("media", 0o777), ("photos", 0o777)] {
            let d = root.path().join(name);
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        // Also a regular file sibling — must be ignored (shares are dirs).
        std::fs::write(root.path().join("note.txt"), b"x").unwrap();

        let target = root.path().join("data");
        let peers = PosixPermissions.reference_peers(target.to_str().unwrap());
        // media + photos, not data (the target) and not the file.
        assert_eq!(peers.len(), 2);
        assert!(peers.iter().all(|(_, i)| i.mode == 0o777));
        assert!(!peers.iter().any(|(p, _)| p.ends_with("/data")));
        assert!(!peers.iter().any(|(p, _)| p.ends_with("note.txt")));

        // End to end: detect_candidates would suggest 0o777 for the 0o775 target.
        let ranked = rank_candidates(peers, Some(0o775));
        assert_eq!(ranked[0].mode, 0o777);
        assert_eq!(ranked[0].count, 2);
    }
}
