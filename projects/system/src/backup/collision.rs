//! Fleet-wide backup-collision detection.
//!
//! Two backups CONFLICT when they are written to the same folder on the same
//! underlying storage — even from DIFFERENT machines. Example: two hosts both
//! pointed at the same NFS export and both writing `…/hosts/proxmox/thor`
//! interleave their payloads and corrupt each other's retention.
//!
//! Detection is over the FLEET, not one host: each node self-reports its resolved
//! backup destinations into a `backup`/`destinations` config row, which
//! replicates across the mesh ([[mesh-data-is-eventually-consistent]]); the check
//! unions every owner's destinations and looks for overlaps.
//!
//! A collision is keyed on **(backing identity, sub-path)**. The backing identity
//! is globally stable
//! ([`TargetLocation::backing_key`](super::target::TargetLocation::backing_key)):
//! per-host local disks carry `local://<host>`, shared storage carries its shared
//! address (`nfs://server/export`), so two hosts collide only when they write the
//! same shared backing. Sub-paths overlap when they are equal or one nests under
//! the other. This module computes collisions from a plain list of destinations;
//! the `tools` layer gathers them and raises notifications.

use serde::{Deserialize, Serialize};

/// One resolved backup destination: WHERE a given `(kind, instance)` writes.
/// Stored (per owner) in the `backup`/`destinations` config row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    /// Backup kind (`host`, `service`, …).
    pub kind: String,
    /// Instance within the kind.
    pub instance: String,
    /// Globally stable backing identity (`local://thor`, `nfs://nas/export`).
    pub backing_key: String,
    /// Sub-path beneath the backing root (the layout, e.g. `hosts/proxmox/thor`).
    pub subpath: String,
    /// The target ref this resolved from (`<kind>/<name>`), for messaging.
    pub target: String,
}

/// A destination tagged with the host that owns/writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedDestination {
    pub owner: String,
    pub dest: Destination,
}

impl OwnedDestination {
    /// A stable identity tuple: two entries with the same tuple are the SAME
    /// logical backup, not a collision.
    fn identity(&self) -> (&str, &str, &str) {
        (&self.owner, &self.dest.kind, &self.dest.instance)
    }

    /// Human party label for a collision message.
    fn party(&self) -> String {
        format!(
            "{}:{}/{} ({})",
            self.owner, self.dest.kind, self.dest.instance, self.dest.target
        )
    }
}

/// A detected collision between two destinations sharing a backing + overlapping
/// path. `nested` is true when one path is a parent of the other (vs identical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub backing_key: String,
    pub party_a: String,
    pub party_b: String,
    pub path_a: String,
    pub path_b: String,
    pub nested: bool,
}

impl Collision {
    /// Stable notification key, independent of pair order.
    pub fn key(&self) -> String {
        let mut ends = [
            format!("{}|{}", self.party_a, self.path_a),
            format!("{}|{}", self.party_b, self.path_b),
        ];
        ends.sort();
        format!(
            "backup-collision:{}:{}::{}",
            self.backing_key, ends[0], ends[1]
        )
    }

    /// One-line human description for the notification body.
    pub fn describe(&self) -> String {
        let rel = if self.nested {
            "nests under"
        } else {
            "collides with"
        };
        format!(
            "{} {} {} on backing {} ({} / {}). Point one at a distinct path.",
            self.party_a, rel, self.party_b, self.backing_key, self.path_a, self.path_b
        )
    }
}

/// Normalize a sub-path into comparable components (trim slashes/empties).
fn components(subpath: &str) -> Vec<&str> {
    subpath
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// Whether two sub-paths overlap on the same backing: `Some(nested)` when they
/// are equal (`nested=false`) or one is a prefix of the other (`nested=true`);
/// `None` when they are disjoint.
fn overlap(a: &str, b: &str) -> Option<bool> {
    let (ca, cb) = (components(a), components(b));
    let shorter = ca.len().min(cb.len());
    if ca[..shorter] != cb[..shorter] {
        return None;
    }
    Some(ca.len() != cb.len())
}

/// Every collision among `dests` (fleet-wide). A collision is any two DISTINCT
/// destination entries sharing a `backing_key` whose sub-paths overlap. Distinct
/// entries with the same identity (e.g. one workload configured to write the same
/// place twice) still collide — writing a folder twice is itself the bug.
pub fn detect_collisions(dests: &[OwnedDestination]) -> Vec<Collision> {
    let mut out = Vec::new();
    for i in 0..dests.len() {
        for j in (i + 1)..dests.len() {
            let (a, b) = (&dests[i], &dests[j]);
            if a.dest.backing_key != b.dest.backing_key {
                continue;
            }
            // Identical identity AND identical path is the same row echoed twice
            // (e.g. a host re-reporting) — not a real conflict.
            if a.identity() == b.identity() && a.dest.subpath == b.dest.subpath {
                continue;
            }
            if let Some(nested) = overlap(&a.dest.subpath, &b.dest.subpath) {
                out.push(Collision {
                    backing_key: a.dest.backing_key.clone(),
                    party_a: a.party(),
                    party_b: b.party(),
                    path_a: a.dest.subpath.clone(),
                    path_b: b.dest.subpath.clone(),
                    nested,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(owner: &str, kind: &str, inst: &str, backing: &str, subpath: &str) -> OwnedDestination {
        OwnedDestination {
            owner: owner.to_string(),
            dest: Destination {
                kind: kind.to_string(),
                instance: inst.to_string(),
                backing_key: backing.to_string(),
                subpath: subpath.to_string(),
                target: format!("{kind}/default"),
            },
        }
    }

    #[test]
    fn different_hosts_same_nfs_folder_collide() {
        let d = vec![
            dest(
                "thor",
                "host",
                "thor",
                "nfs://nas/backups",
                "hosts/proxmox/thor",
            ),
            dest(
                "mimir",
                "host",
                "mimir",
                "nfs://nas/backups",
                "hosts/proxmox/thor",
            ),
        ];
        let c = detect_collisions(&d);
        assert_eq!(c.len(), 1);
        assert!(!c[0].nested);
        assert_eq!(c[0].backing_key, "nfs://nas/backups");
    }

    #[test]
    fn same_local_path_on_different_hosts_does_not_collide() {
        // Per-host local backing keys differ, so identical paths are independent.
        let d = vec![
            dest("thor", "host", "thor", "local://thor", "hosts/bare/thor"),
            dest("mimir", "host", "mimir", "local://mimir", "hosts/bare/thor"),
        ];
        assert!(detect_collisions(&d).is_empty());
    }

    #[test]
    fn nested_paths_on_same_backing_collide() {
        let d = vec![
            dest("a", "host", "x", "nfs://nas/b", "hosts"),
            dest("b", "service", "y", "nfs://nas/b", "hosts/proxmox/thor"),
        ];
        let c = detect_collisions(&d);
        assert_eq!(c.len(), 1);
        assert!(c[0].nested, "one path nests under the other");
    }

    #[test]
    fn disjoint_paths_same_backing_do_not_collide() {
        let d = vec![
            dest("a", "host", "thor", "nfs://nas/b", "hosts/proxmox/thor"),
            dest("b", "host", "mimir", "nfs://nas/b", "hosts/proxmox/mimir"),
        ];
        assert!(detect_collisions(&d).is_empty());
    }

    #[test]
    fn echoed_identical_row_is_not_a_collision() {
        let d = vec![
            dest("thor", "host", "thor", "nfs://nas/b", "hosts/proxmox/thor"),
            dest("thor", "host", "thor", "nfs://nas/b", "hosts/proxmox/thor"),
        ];
        assert!(detect_collisions(&d).is_empty());
    }

    #[test]
    fn collision_key_is_order_independent() {
        let a = dest("thor", "host", "thor", "nfs://nas/b", "p");
        let b = dest("mimir", "host", "mimir", "nfs://nas/b", "p");
        let c1 = detect_collisions(&[a.clone(), b.clone()]);
        let c2 = detect_collisions(&[b, a]);
        assert_eq!(c1[0].key(), c2[0].key());
    }
}
