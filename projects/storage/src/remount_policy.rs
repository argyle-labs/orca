//! Typed remount policy for managed network mounts.
//!
//! A mount's remount behaviour used to be an opaque `remount_policy` string that
//! [`RemountAggression::from_policy`] parsed at the last moment. This module
//! replaces that with a fully-typed policy the whole engine shares: how
//! aggressively a re-election may disrupt a busy mount ([`RemountAggression`]),
//! whether and how to fail (back) between ordered sources ([`Failover`]), which
//! transport-liveness probe a source election uses ([`SourceProbe`]), and how a
//! coordinated drain releases a source ([`Drain`]).
//!
//! It lives in the `storage` domain (the dependency leaf both `system` and the
//! backend plugins reach through `plugin_toolkit::storage`) so the policy shape
//! and [`RemountAggression`] have a single home, imported rather than
//! re-declared. All plain serde + `JsonSchema` so it crosses the plugin boundary
//! and persists as one typed column.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How aggressively a re-election is allowed to disrupt an *actively-held* mount
/// when failing (back) to the elected source.
///
/// Remounting under live container I/O can interrupt Plex/Jellyfin mid-stream,
/// so the aggressiveness is a policy, and the default is [`Safe`](Self::Safe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemountAggression {
    /// Never disturb a busy mount. If the elected source differs from the live
    /// one and the mount is busy, re-render and log a *pending* failback; the
    /// swap happens on the next idle re-trigger. A not-busy mount is remounted
    /// immediately. This is the default.
    #[default]
    Safe,
    /// Prefer a clean remount, but if the mount is busy escalate to a lazy
    /// force-unmount + retrigger (and, only as a clearly-logged last resort,
    /// killing holders). Opt-in per mount — it can disrupt live I/O.
    Force,
}

/// Which transport-liveness probe a source election runs against a candidate
/// source. [`Auto`](Self::Auto) resolves to [`Nfs`](Self::Nfs) for NFS
/// filesystem types (an RPC NULL that a *hung* nfsd will not answer even when
/// TCP is up) and [`Tcp`](Self::Tcp) otherwise (a bare TCP connect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceProbe {
    /// A bare TCP connect to the source's transport port.
    Tcp,
    /// An RPC NULL to the NFS program — returns down when TCP is up but nfsd
    /// does not answer, so election will not fail back onto a hung primary.
    Nfs,
    /// Resolve per filesystem type: [`Nfs`](Self::Nfs) for `nfs*`, else
    /// [`Tcp`](Self::Tcp).
    #[default]
    Auto,
}

impl SourceProbe {
    /// Resolve [`Auto`](Self::Auto) against a concrete `fstype`: NFS filesystem
    /// types (`nfs`, `nfs4`, …) get the RPC-NULL [`Nfs`](Self::Nfs) probe; every
    /// other type gets the bare-TCP [`Tcp`](Self::Tcp) probe. An explicit
    /// [`Tcp`](Self::Tcp)/[`Nfs`](Self::Nfs) is returned unchanged.
    pub fn resolve(self, fstype: &str) -> SourceProbe {
        match self {
            SourceProbe::Auto => {
                if fstype.starts_with("nfs") {
                    SourceProbe::Nfs
                } else {
                    SourceProbe::Tcp
                }
            }
            other => other,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_confirm_ticks() -> u32 {
    2
}

/// Fail-over / fail-back policy between a mount's ordered sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct Failover {
    /// Whether ordered-source fail-over is performed at all. When `false` the
    /// mount stays pinned to its primary source and is never re-elected.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether to fail *back* to a higher-priority source once it is live again
    /// (index 0 = primary wins the next election). When `false`, a mount that
    /// degraded to a secondary stays there until it too fails.
    #[serde(default = "default_true")]
    pub return_to_primary: bool,
    /// Consecutive confirming ticks a transition must persist before it is acted
    /// on — the blip filter that rides out a briefly-slow server.
    #[serde(default = "default_confirm_ticks")]
    pub confirm_ticks: u32,
    /// Which transport-liveness probe the election uses.
    #[serde(default)]
    pub probe: SourceProbe,
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            enabled: true,
            return_to_primary: true,
            confirm_ticks: default_confirm_ticks(),
            probe: SourceProbe::default(),
        }
    }
}

/// How a coordinated drain releases every client placement of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DrainMode {
    /// `umount -l` (lazy): detach the mount, letting in-flight I/O finish.
    #[default]
    Lazy,
    /// `umount -l -f` (lazy + force): detach even under a wedged/held handle.
    Force,
}

fn default_settle_secs() -> u32 {
    15
}

/// Drain policy — how a source is released from every client before a
/// coordinated operation (a source reboot) that will take it offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct Drain {
    /// Whether a coordinated drain is performed at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Lazy vs lazy-force unmount.
    #[serde(default)]
    pub mode: DrainMode,
    /// Seconds to wait after the lazy unmount before the forced one, letting
    /// in-flight I/O settle.
    #[serde(default = "default_settle_secs")]
    pub settle_secs: u32,
}

impl Default for Drain {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: DrainMode::default(),
            settle_secs: default_settle_secs(),
        }
    }
}

/// The typed per-mount remount policy — the whole engine's behaviour axis in one
/// serde object, replacing the opaque `remount_policy` string.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", default)]
pub struct RemountPolicy {
    /// How aggressively a re-election may disrupt a busy mount.
    pub aggression: RemountAggression,
    /// Ordered-source fail-over / fail-back policy.
    pub failover: Failover,
    /// Coordinated-drain policy.
    pub drain: Drain,
}

impl RemountPolicy {
    /// Parse a persisted policy from an optional JSON string column, falling back
    /// to [`RemountPolicy::default`] on `None` or a malformed value. The seam a
    /// legacy TEXT `remount_policy` column (the retiring `managed_mounts` table)
    /// is read through so the engine always works with the typed policy even
    /// where the on-disk column is still a string.
    pub fn from_json_opt(s: Option<&str>) -> RemountPolicy {
        s.map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_json_opt_defaults_on_none_or_garbage() {
        assert_eq!(RemountPolicy::from_json_opt(None), RemountPolicy::default());
        assert_eq!(
            RemountPolicy::from_json_opt(Some("   ")),
            RemountPolicy::default()
        );
        assert_eq!(
            RemountPolicy::from_json_opt(Some("not json")),
            RemountPolicy::default()
        );
        let p = RemountPolicy::from_json_opt(Some(r#"{"aggression":"force"}"#));
        assert_eq!(p.aggression, RemountAggression::Force);
    }

    #[test]
    fn default_policy_has_safe_aggression_and_enabled_failover() {
        let p = RemountPolicy::default();
        assert_eq!(p.aggression, RemountAggression::Safe);
        assert!(p.failover.enabled);
        assert!(p.failover.return_to_primary);
        assert_eq!(p.failover.confirm_ticks, 2);
        assert_eq!(p.failover.probe, SourceProbe::Auto);
        assert!(p.drain.enabled);
        assert_eq!(p.drain.mode, DrainMode::Lazy);
        assert_eq!(p.drain.settle_secs, 15);
    }

    #[test]
    fn empty_object_deserializes_to_defaults() {
        let p: RemountPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p, RemountPolicy::default());
    }

    #[test]
    fn policy_round_trips_through_serde() {
        let p = RemountPolicy {
            aggression: RemountAggression::Force,
            failover: Failover {
                enabled: false,
                return_to_primary: false,
                confirm_ticks: 5,
                probe: SourceProbe::Nfs,
            },
            drain: Drain {
                enabled: true,
                mode: DrainMode::Force,
                settle_secs: 30,
            },
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<RemountPolicy>(&s).unwrap(), p);
    }

    #[test]
    fn partial_object_fills_missing_with_defaults() {
        // Only aggression set; failover/drain default in, and inside failover
        // only probe set with the rest defaulted.
        let p: RemountPolicy =
            serde_json::from_str(r#"{"aggression":"force","failover":{"probe":"tcp"}}"#).unwrap();
        assert_eq!(p.aggression, RemountAggression::Force);
        assert_eq!(p.failover.probe, SourceProbe::Tcp);
        assert!(p.failover.enabled); // defaulted
        assert_eq!(p.failover.confirm_ticks, 2); // defaulted
        assert_eq!(p.drain, Drain::default());
    }

    #[test]
    fn source_probe_auto_resolves_nfs_for_nfs_fstypes_else_tcp() {
        assert_eq!(SourceProbe::Auto.resolve("nfs"), SourceProbe::Nfs);
        assert_eq!(SourceProbe::Auto.resolve("nfs4"), SourceProbe::Nfs);
        assert_eq!(SourceProbe::Auto.resolve("cifs"), SourceProbe::Tcp);
        assert_eq!(SourceProbe::Auto.resolve("smbfs"), SourceProbe::Tcp);
        // An explicit choice is never re-resolved.
        assert_eq!(SourceProbe::Tcp.resolve("nfs4"), SourceProbe::Tcp);
        assert_eq!(SourceProbe::Nfs.resolve("cifs"), SourceProbe::Nfs);
    }
}
