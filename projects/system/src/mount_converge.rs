//! Per-host mount convergence loop — the native-mount lifecycle owner that
//! replaces autofs.
//!
//! Each tick this host reads the replicated desired state (its `mounts`
//! placements joined to their `shares`) and makes local reality match: a missing
//! mount is mounted, a stale/unreachable one is remounted onto the next live
//! ordered source, a removed placement is unmounted. Source election, health
//! probing, and the mount itself are all orca's — there is no automounter.
//!
//! The mount/unmount execution goes through the existing root/`sudo` privilege
//! boundary as [`PrivilegedOp::Mount`] / [`PrivilegedOp::Unmount`] — the
//! unprivileged daemon plans, the root helper acts.
//!
//! The decision core ([`plan`]) is pure and unit-tested; the async wrapper only
//! supplies it observed health and executes the actions it returns.

use crate::mount_exec::MountReq;
use crate::{mounts, shares};
use std::collections::{HashMap, HashSet};

/// A desired mount for THIS host: a placement joined to its share, with the
/// share's ordered sources and pre-rendered options resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredMount {
    pub target: String,
    pub fstype: String,
    /// Ordered `host:/export` sources, primary first — election picks the first
    /// live one at mount time.
    pub sources: Vec<String>,
    /// The backend-rendered `mount -o` option string (opaque to core).
    pub options: String,
}

/// One convergence action for the privileged applier to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Mount `target` — the async layer elects a live source from `sources`.
    Mount { target: String },
    /// Release `target` (`umount -lf`): stale-and-remounting, or a removed
    /// placement.
    Unmount { target: String },
}

/// Resolve the mount placements assigned to `this_host`, each joined to its
/// share. A placement whose share is missing or is disabled is skipped (it can't
/// be materialized). Shares are keyed by their uuidv7 `id`, which the placement
/// references via `share_id`.
pub fn desired_for_host(this_host: &str) -> anyhow::Result<Vec<DesiredMount>> {
    let by_id: HashMap<String, shares::Share> = shares::endpoint_db::list()?
        .into_iter()
        .filter(|s| s.enabled)
        .map(|s| (s.id.clone(), s))
        .collect();

    let mut out = Vec::new();
    for m in mounts::endpoint_db::list()? {
        if !m.enabled || m.host != this_host {
            continue;
        }
        let Some(share) = by_id.get(&m.share_id) else {
            continue;
        };
        let sources: Vec<String> = serde_json::from_str(&share.sources).unwrap_or_default();
        if sources.is_empty() {
            continue;
        }
        out.push(DesiredMount {
            target: m.target,
            fstype: share.fstype.clone(),
            sources,
            options: share.options_rendered.clone(),
        });
    }
    Ok(out)
}

/// Decide the convergence actions. Pure: given the desired mounts for this host,
/// the set of targets currently mounted at all, and the subset that are mounted
/// **and healthy**, return the mount/unmount actions that make reality match.
///
/// - desired, not mounted        → Mount
/// - desired, mounted but stale   → Unmount then Mount (remount, fail forward)
/// - desired, mounted and healthy → nothing
/// - mounted but no longer desired → Unmount (removed placement)
///
/// Ordering matters: the remount Unmount precedes its Mount, and stray-target
/// Unmounts come last, so the applier can run the vector top-to-bottom.
pub fn plan(
    desired: &[DesiredMount],
    mounted_any: &HashSet<String>,
    mounted_healthy: &HashSet<String>,
) -> Vec<Action> {
    let desired_targets: HashSet<&str> = desired.iter().map(|d| d.target.as_str()).collect();
    let mut actions = Vec::new();

    for d in desired {
        let mounted = mounted_any.contains(&d.target);
        let healthy = mounted_healthy.contains(&d.target);
        if !mounted {
            actions.push(Action::Mount {
                target: d.target.clone(),
            });
        } else if !healthy {
            // Stale: release then remount so election can fail forward onto a
            // live source instead of leaving the wedged superblock.
            actions.push(Action::Unmount {
                target: d.target.clone(),
            });
            actions.push(Action::Mount {
                target: d.target.clone(),
            });
        }
    }

    // Anything mounted that is no longer a desired placement is released.
    for t in mounted_any {
        if !desired_targets.contains(t.as_str()) {
            actions.push(Action::Unmount { target: t.clone() });
        }
    }
    actions
}

/// Build the [`MountReq`] for a desired target from an elected live `source`.
pub fn mount_req(d: &DesiredMount, source: &str) -> MountReq {
    MountReq {
        source: source.to_string(),
        target: d.target.clone(),
        fstype: d.fstype.clone(),
        options: d.options.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(target: &str) -> DesiredMount {
        DesiredMount {
            target: target.to_string(),
            fstype: "nfs4".to_string(),
            sources: vec!["10.0.0.1:/e".to_string(), "10.0.0.2:/e".to_string()],
            options: "vers=4.2,soft".to_string(),
        }
    }
    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_desired_is_mounted() {
        let out = plan(&[d("/mnt/data")], &set(&[]), &set(&[]));
        assert_eq!(
            out,
            vec![Action::Mount {
                target: "/mnt/data".into()
            }]
        );
    }

    #[test]
    fn healthy_desired_is_left_alone() {
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn stale_desired_is_unmounted_then_remounted_in_order() {
        let out = plan(&[d("/mnt/data")], &set(&["/mnt/data"]), &set(&[]));
        assert_eq!(
            out,
            vec![
                Action::Unmount {
                    target: "/mnt/data".into()
                },
                Action::Mount {
                    target: "/mnt/data".into()
                },
            ]
        );
    }

    #[test]
    fn undesired_mount_is_released() {
        // /mnt/old is mounted but no longer a placement → unmount.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data", "/mnt/old"]),
            &set(&["/mnt/data"]),
        );
        assert_eq!(
            out,
            vec![Action::Unmount {
                target: "/mnt/old".into()
            }]
        );
    }

    #[test]
    fn mount_req_uses_elected_source_and_rendered_options() {
        let req = mount_req(&d("/mnt/data"), "10.0.0.2:/e");
        assert_eq!(req.source, "10.0.0.2:/e");
        assert_eq!(req.target, "/mnt/data");
        assert_eq!(req.fstype, "nfs4");
        assert_eq!(req.options, "vers=4.2,soft");
    }
}
