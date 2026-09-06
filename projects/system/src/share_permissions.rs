//! The core-owned default POSIX permissions provider.
//!
//! The [`PermissionsProvider`](contract::permissions::PermissionsProvider)
//! capability — the trait, registry, resolver, and candidate ranking — lives in
//! `contract::permissions` so a filesystem plugin (unraid, …) can *expose*
//! permission introspection for the shares it serves. This module supplies the
//! fallback: a plain local POSIX filesystem, registered natively at daemon boot
//! so the capability works before any plugin loads. An Unraid `/mnt/user/<share>`
//! tree is plain POSIX, so this fallback already handles it; a plugin registers a
//! richer provider only when its filesystem has non-POSIX semantics (ACLs, S3
//! policies), and — named anything but `"posix"` — it is preferred over this one.

use std::sync::Arc;

use contract::BoxFuture;
use contract::permissions::{PermInfo, PermissionsProvider, register_provider};

/// Register the core-owned default POSIX provider. Called once at daemon boot so
/// the share-permission capability works before any filesystem plugin loads.
pub fn register_builtin() {
    register_provider(Arc::new(PosixPermissions));
}

/// Default provider: a plain local POSIX filesystem. Reference peers of a share
/// path are its sibling directories (the other shares under the same parent),
/// which is exactly the Unraid `/mnt/user/<share>` layout. Named `"posix"` so the
/// resolver always tries it last, after any filesystem-specific provider.
struct PosixPermissions;

impl PermissionsProvider for PosixPermissions {
    fn name(&self) -> &str {
        "posix"
    }

    fn read(&self, path: &str) -> BoxFuture<'_, Option<PermInfo>> {
        let path = path.to_string();
        Box::pin(async move { stat_perm(&path) })
    }

    fn reference_peers(&self, path: &str) -> BoxFuture<'_, Vec<(String, PermInfo)>> {
        let path = path.to_string();
        Box::pin(async move { sibling_perms(&path) })
    }
}

/// The sibling directories of `path` (its reference peers) with their perms.
fn sibling_perms(path: &str) -> Vec<(String, PermInfo)> {
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

/// Read POSIX mode/uid/gid for a path. `None` if it can't be stat'd.
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
    use contract::permissions::rank_candidates;

    #[cfg(unix)]
    #[test]
    fn posix_sibling_perms_reads_sibling_dirs_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        for (name, mode) in [("data", 0o775u32), ("media", 0o777), ("photos", 0o777)] {
            let d = root.path().join(name);
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        // A regular-file sibling must be ignored (shares are dirs).
        std::fs::write(root.path().join("note.txt"), b"x").unwrap();

        let target = root.path().join("data");
        let peers = sibling_perms(target.to_str().unwrap());
        assert_eq!(peers.len(), 2, "media + photos, not data and not the file");
        assert!(peers.iter().all(|(_, i)| i.mode == 0o777));
        assert!(!peers.iter().any(|(p, _)| p.ends_with("/data")));
        assert!(!peers.iter().any(|(p, _)| p.ends_with("note.txt")));

        // End to end: 0o777 is the winning candidate for the 0o775 target.
        let ranked = rank_candidates(peers, Some(0o775));
        assert_eq!(ranked[0].mode, 0o777);
        assert_eq!(ranked[0].count, 2);
    }
}
