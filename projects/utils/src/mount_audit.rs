//! Classifies provisioned mounts that are set up to cause an outage later.
//!
//! Two conditions, both invisible to every liveness and level check on the
//! fleet today, because at rest they look like nothing at all:
//!
//! 1. **Unbounded scratch.** A bind mount has no size cap — the guest writes
//!    straight into the host's filesystem. When that host path lives on the
//!    hypervisor's own root volume, one long transcode does not fill a
//!    container, it fills the hypervisor and takes down *every guest on the
//!    node*. frigg CT113 has exactly this shape: `/srv/jellyfin-transcode` ->
//!    `/transcode`, no cap, backed by `pve-root`. It measures 0 bytes at rest,
//!    which is precisely why nothing has ever flagged it.
//! 2. **Unused provisioned mount.** A mount that is declared, mounted, and
//!    consuming capacity while nothing reads or writes it. Harmless in itself,
//!    and that is the problem — it is indistinguishable from a live mount, so
//!    it survives every cleanup and quietly holds a share or a volume hostage.
//!
//! Pure classification: the caller supplies facts it has already gathered
//! (from `pct config`, a storage plugin, `findmnt`), and gets back ranked
//! findings. Nothing here reads a filesystem or shells out, so the whole
//! decision table is testable — which matters because the interesting cases are
//! the ones that look healthy.

/// How a mount gets its storage, which decides whether a size cap is even
/// possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    /// Host directory passed through to a guest. **Cannot** be quota'd by the
    /// hypervisor — it is the host's filesystem, so its only bound is that
    /// filesystem's free space.
    Bind,
    /// A dedicated volume on managed storage. Has its own size, so it fills
    /// itself and stops rather than taking the host with it.
    Volume,
    /// A network share (NFS/SMB/object). Bounded remotely, and its capacity is
    /// the remote server's problem, not this node's.
    Network,
}

/// One provisioned mount, as the caller already knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    /// Stable identifier for reporting, e.g. `"113:mp2"`.
    pub id: String,
    /// Host-side source: a path for [`MountKind::Bind`], a volume or share
    /// reference otherwise.
    pub source: String,
    /// Path inside the guest.
    pub target: String,
    pub kind: MountKind,
    /// Explicit size cap, when the mount has one.
    pub size_limit_bytes: Option<u64>,
    /// A read-only mount cannot grow, so it is never unbounded-scratch however
    /// it is backed.
    pub read_only: bool,
    /// True when this mount's source shares a filesystem with the host's own
    /// root. This is what separates "a guest can fill its scratch" from "a guest
    /// can kill the hypervisor", and it is the caller's job to determine because
    /// only the caller can compare device ids.
    pub on_host_root_fs: bool,
    /// Whether anything is known to consume this mount. `None` = the caller
    /// could not tell, which must NOT be reported as unused — see
    /// [`Risk::UnusedProvisioned`].
    pub consumed: Option<bool>,
}

/// What is wrong with a mount, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Writable, uncapped, and sharing a filesystem with the host root: a guest
    /// can exhaust the hypervisor's own storage.
    UnboundedOnHostRootFs,
    /// Writable and uncapped, but on a filesystem of its own. Bad, bounded.
    UnboundedScratch,
    /// Declared and consuming capacity with nothing using it.
    UnusedProvisioned,
}

/// One classified problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountFinding {
    pub id: String,
    pub risk: Risk,
    /// Why, in terms an operator can act on.
    pub detail: String,
}

/// Classify one mount, or `None` when it is fine.
///
/// Order matters: a mount can be both uncapped and unused, and the uncapped
/// finding is the one that causes an outage, so it wins. Reporting both would
/// double-count a single mount.
fn classify(m: &MountSpec) -> Option<MountFinding> {
    let unbounded = m.kind == MountKind::Bind && m.size_limit_bytes.is_none() && !m.read_only;
    if unbounded {
        let (risk, detail) = if m.on_host_root_fs {
            (
                Risk::UnboundedOnHostRootFs,
                format!(
                    "`{}` is a writable bind mount with no size cap, and its source shares \
                     a filesystem with the host root. A guest writing to `{}` consumes the \
                     hypervisor's own storage, so filling it takes down every guest on this \
                     node, not just this one. Bind mounts cannot be quota'd — move the \
                     source to a dedicated volume, or cap the writer.",
                    m.source, m.target
                ),
            )
        } else {
            (
                Risk::UnboundedScratch,
                format!(
                    "`{}` is a writable bind mount with no size cap. A guest writing to \
                     `{}` is bounded only by that filesystem's free space. Bind mounts \
                     cannot be quota'd — use a dedicated volume if the writer is unbounded.",
                    m.source, m.target
                ),
            )
        };
        return Some(MountFinding {
            id: m.id.clone(),
            risk,
            detail,
        });
    }
    // Only `Some(false)` is evidence of disuse. `None` means the caller could not
    // tell, and guessing would delete a live mount.
    if m.consumed == Some(false) {
        return Some(MountFinding {
            id: m.id.clone(),
            risk: Risk::UnusedProvisioned,
            detail: format!(
                "`{}` is mounted at `{}` and consuming capacity, but nothing is known to \
                 use it. Confirm before removing — an idle mount and an unused one look \
                 identical from here.",
                m.source, m.target
            ),
        });
    }
    None
}

/// Classify every mount, most severe first.
///
/// Ties keep input order, so a report over a stable config is itself stable —
/// an operator diffing two runs should see real changes, not reshuffling.
pub fn audit(mounts: &[MountSpec]) -> Vec<MountFinding> {
    let mut out: Vec<MountFinding> = mounts.iter().filter_map(classify).collect();
    out.sort_by_key(|f| f.risk);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(id: &str, source: &str, target: &str) -> MountSpec {
        MountSpec {
            id: id.to_string(),
            source: source.to_string(),
            target: target.to_string(),
            kind: MountKind::Bind,
            size_limit_bytes: None,
            read_only: false,
            on_host_root_fs: false,
            consumed: None,
        }
    }

    /// The live frigg defect, in its exact shape. 0 bytes at rest, on pve-root.
    #[test]
    fn the_frigg_jellyfin_transcode_shape_is_the_top_risk() {
        let m = MountSpec {
            on_host_root_fs: true,
            ..bind("113:mp2", "/srv/jellyfin-transcode", "/transcode")
        };
        let f = classify(&m).expect("must be reported");
        assert_eq!(f.risk, Risk::UnboundedOnHostRootFs);
        assert_eq!(f.id, "113:mp2");
        // The detail must say the blast radius is the node, not the container.
        assert!(f.detail.contains("every guest on this node"));
        assert!(
            f.detail.contains("cannot be quota'd"),
            "names the constraint"
        );
    }

    /// Same uncapped bind mount, but off the host root: still wrong, less bad.
    #[test]
    fn uncapped_bind_on_its_own_filesystem_is_lesser_but_still_reported() {
        let f = classify(&bind("113:mp2", "/srv/scratch", "/transcode")).expect("reported");
        assert_eq!(f.risk, Risk::UnboundedScratch);
        assert!(
            !f.detail.contains("every guest"),
            "must not overstate blast radius"
        );
    }

    #[test]
    fn a_read_only_bind_mount_cannot_grow_and_is_not_a_finding() {
        let m = MountSpec {
            read_only: true,
            on_host_root_fs: true,
            ..bind("115:mp0", "/mnt/data", "/mnt/data")
        };
        assert_eq!(classify(&m), None, "ro=1 cannot fill anything");
    }

    #[test]
    fn a_capped_bind_mount_is_not_a_finding() {
        let m = MountSpec {
            size_limit_bytes: Some(32 * 1024 * 1024 * 1024),
            on_host_root_fs: true,
            ..bind("113:mp2", "/srv/jellyfin-transcode", "/transcode")
        };
        assert_eq!(classify(&m), None);
    }

    /// Volumes and network shares bound themselves; flagging them would be noise
    /// on nearly every mount the fleet has.
    #[test]
    fn volume_and_network_mounts_are_bounded_elsewhere() {
        for kind in [MountKind::Volume, MountKind::Network] {
            let m = MountSpec {
                kind,
                ..bind("x:mp0", "//willow/data", "/mnt/data")
            };
            assert_eq!(classify(&m), None, "{kind:?} must not be flagged");
        }
    }

    #[test]
    fn a_mount_nothing_consumes_is_reported_as_unused() {
        let m = MountSpec {
            kind: MountKind::Network,
            consumed: Some(false),
            ..bind("109:mp0", "//willow/old-backups", "/mnt/backups")
        };
        let f = classify(&m).expect("reported");
        assert_eq!(f.risk, Risk::UnusedProvisioned);
        assert!(
            f.detail.contains("Confirm before removing"),
            "must not imply safe to delete"
        );
    }

    /// The distinction that keeps this check from deleting live mounts: not
    /// knowing is not the same as knowing it is unused.
    #[test]
    fn unknown_consumption_is_never_reported_as_unused() {
        let m = MountSpec {
            kind: MountKind::Network,
            consumed: None,
            ..bind("109:mp0", "//willow/data", "/mnt/backups")
        };
        assert_eq!(classify(&m), None, "None must not become a finding");
    }

    #[test]
    fn a_consumed_mount_is_not_a_finding() {
        let m = MountSpec {
            kind: MountKind::Network,
            consumed: Some(true),
            ..bind("109:mp0", "//willow/data", "/mnt/backups")
        };
        assert_eq!(classify(&m), None);
    }

    /// One mount, one finding — the outage-causing one.
    #[test]
    fn an_uncapped_and_unused_mount_reports_only_the_uncapped_risk() {
        let m = MountSpec {
            consumed: Some(false),
            on_host_root_fs: true,
            ..bind("113:mp2", "/srv/jellyfin-transcode", "/transcode")
        };
        let f = classify(&m).expect("reported");
        assert_eq!(f.risk, Risk::UnboundedOnHostRootFs, "severity must win");
        assert_eq!(audit(&[m]).len(), 1, "must not double-count one mount");
    }

    #[test]
    fn audit_ranks_most_severe_first_and_is_stable_within_a_rank() {
        let mounts = vec![
            MountSpec {
                kind: MountKind::Network,
                consumed: Some(false),
                ..bind("a", "//willow/x", "/x")
            },
            bind("b", "/srv/scratch", "/scratch"),
            MountSpec {
                on_host_root_fs: true,
                ..bind("c", "/srv/transcode", "/transcode")
            },
            bind("d", "/srv/other", "/other"),
        ];
        let got = audit(&mounts);
        let ids: Vec<&str> = got.iter().map(|f| f.id.as_str()).collect();
        // c (host-root) first, then the two plain unbounded in input order, then unused.
        assert_eq!(ids, vec!["c", "b", "d", "a"]);
    }

    #[test]
    fn a_healthy_fleet_produces_an_empty_report() {
        let mounts = vec![
            MountSpec {
                kind: MountKind::Network,
                consumed: Some(true),
                ..bind("a", "//willow/data", "/mnt/data")
            },
            MountSpec {
                read_only: true,
                ..bind("b", "/mnt/data", "/mnt/data")
            },
        ];
        assert_eq!(audit(&mounts), vec![]);
    }
}
