//! Source-liveness election for managed network mounts.
//!
//! autofs (see [`crate::autofs`]) cannot do liveness-ordered source selection
//! *or* fail-back: given a map line with several replicated locations it picks
//! one by response time and never returns to a higher-priority source once it is
//! live again. So a host that fell over to the secondary while the primary was
//! briefly down stays on the secondary indefinitely — the exact defect this
//! module cures.
//!
//! orca instead **elects a single live source** per mount and renders only that
//! one into the map. Election is deterministic: from the mount's ordered sources
//! (primary first — see [`crate::managed_mounts::ordered_sources`]) it picks the
//! **first live** one. Because the primary is index 0, a recovered primary always
//! wins the next election, which *is* fail-back. When no source is live the
//! election is empty and the caller logs a non-silent degraded/empty-target
//! warning rather than churning the mount.
//!
//! This module is backend-agnostic: liveness is a caller-supplied predicate
//! (in production a TCP probe from [`plugin_toolkit::storage::probe_source`],
//! which already speaks NFS `:2049` and SMB `:445`), so NFS today and SMB next
//! ride the same election with no change here. Everything is pure and
//! synchronous so it unit-tests without a network.

/// How aggressively a re-election is allowed to disrupt an *actively-held* mount
/// when failing (back) to the elected source.
///
/// Remounting under live container I/O can interrupt Plex/Jellyfin mid-stream,
/// so the aggressiveness is a policy, and the default is [`Safe`](Self::Safe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemountAggression {
    /// Never disturb a busy mount. If the elected source differs from the live
    /// one and the mount is busy, re-render the map and log a *pending* failback;
    /// the swap happens on the next idle re-trigger. A not-busy mount is
    /// remounted immediately. This is the default.
    #[default]
    Safe,
    /// Prefer a clean remount, but if the mount is busy escalate to a lazy
    /// force-unmount + retrigger (and, only as a clearly-logged last resort,
    /// killing holders). Opt-in per mount — it can disrupt live I/O.
    Force,
}

impl RemountAggression {
    /// Parse a mount's `remount_policy` string into an aggression. Anything other
    /// than an explicit force opt-in (`force` / `force_remount`) is [`Safe`] —
    /// the conservative default, so an unset or unknown policy never disrupts a
    /// live mount.
    pub fn from_policy(policy: Option<&str>) -> Self {
        match policy
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("force") | Some("force_remount") => Self::Force,
            _ => Self::Safe,
        }
    }
}

/// The result of electing a source for one mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Election {
    /// A live source was elected. `index` is its position in the ordered-source
    /// list (0 = primary), carried so callers can log degrade vs. fail-back.
    Elected { source: String, index: usize },
    /// Every ordered source probed as down — nothing to mount. Non-silent:
    /// the caller must log an empty-target warning.
    Empty,
}

/// Elect the first live source from a priority-ordered list.
///
/// `is_live` is the liveness predicate (a TCP probe in production, a stub in
/// tests). Sources are tried in order and the **first** that answers live wins,
/// so index 0 (the primary) is always preferred whenever it is live — that
/// preference *is* the fail-back guarantee. Returns [`Election::Empty`] for an
/// empty input or when no source is live.
pub fn elect(sources: &[String], is_live: impl Fn(&str) -> bool) -> Election {
    for (index, source) in sources.iter().enumerate() {
        if is_live(source) {
            return Election::Elected {
                source: source.clone(),
                index,
            };
        }
    }
    Election::Empty
}

/// Classify what an election means relative to the source that is *currently*
/// mounted, so the caller can log the right non-silent line and decide whether a
/// remount is needed at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// No live source — degraded to nothing. Log an empty-target warning.
    EmptyTarget,
    /// Elected source already matches what is mounted; nothing to do.
    Unchanged,
    /// Elected a higher-priority source than the one mounted (e.g. primary came
    /// back while on the secondary): a **fail-back**. Remount to the elected.
    FailBack { to: String },
    /// Elected a lower-priority source than the one mounted (primary went down):
    /// a **degrade** to a secondary. Remount to the elected.
    Degrade { to: String },
    /// Elected a different same-nothing source (no source currently mounted, or
    /// the current source isn't in the ordered list): mount the elected.
    Mount { to: String },
}

/// Decide the transition from the `current` mounted source (if any) to the
/// `election`, using `sources` to compare priorities by index. Pure — the
/// caller performs the resulting remount.
pub fn transition(sources: &[String], current: Option<&str>, election: &Election) -> Transition {
    let (elected, elected_idx) = match election {
        Election::Empty => return Transition::EmptyTarget,
        Election::Elected { source, index } => (source.as_str(), *index),
    };
    match current {
        Some(cur) if cur == elected => Transition::Unchanged,
        Some(cur) => match sources.iter().position(|s| s == cur) {
            Some(cur_idx) if elected_idx < cur_idx => Transition::FailBack {
                to: elected.to_string(),
            },
            Some(_) => Transition::Degrade {
                to: elected.to_string(),
            },
            None => Transition::Mount {
                to: elected.to_string(),
            },
        },
        None => Transition::Mount {
            to: elected.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srcs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── elect ─────────────────────────────────────────────────────────────

    #[test]
    fn elect_picks_first_live_in_priority_order() {
        let s = srcs(&["primary:/x", "secondary:/x", "tertiary:/x"]);
        // all live → primary wins (deterministic fail-back to index 0)
        assert_eq!(
            elect(&s, |_| true),
            Election::Elected {
                source: "primary:/x".into(),
                index: 0,
            }
        );
    }

    #[test]
    fn elect_degrades_when_primary_down() {
        let s = srcs(&["primary:/x", "secondary:/x"]);
        // primary down, secondary up → secondary elected at index 1
        assert_eq!(
            elect(&s, |src| src != "primary:/x"),
            Election::Elected {
                source: "secondary:/x".into(),
                index: 1,
            }
        );
    }

    #[test]
    fn elect_fails_back_when_primary_returns() {
        let s = srcs(&["primary:/x", "secondary:/x"]);
        // everything live again → primary (index 0) re-wins == fail-back
        assert_eq!(
            elect(&s, |_| true),
            Election::Elected {
                source: "primary:/x".into(),
                index: 0,
            }
        );
    }

    #[test]
    fn elect_empty_when_all_sources_down() {
        let s = srcs(&["primary:/x", "secondary:/x"]);
        assert_eq!(elect(&s, |_| false), Election::Empty);
    }

    #[test]
    fn elect_empty_for_no_sources() {
        assert_eq!(elect(&[], |_| true), Election::Empty);
    }

    #[test]
    fn elect_skips_dead_prefix_to_first_live() {
        let s = srcs(&["a", "b", "c", "d"]);
        assert_eq!(
            elect(&s, |src| src == "c" || src == "d"),
            Election::Elected {
                source: "c".into(),
                index: 2,
            }
        );
    }

    // ── transition ────────────────────────────────────────────────────────

    #[test]
    fn transition_empty_target_when_no_live_source() {
        let s = srcs(&["a", "b"]);
        assert_eq!(
            transition(&s, Some("a"), &Election::Empty),
            Transition::EmptyTarget
        );
    }

    #[test]
    fn transition_unchanged_when_elected_matches_current() {
        let s = srcs(&["a", "b"]);
        let e = Election::Elected {
            source: "a".into(),
            index: 0,
        };
        assert_eq!(transition(&s, Some("a"), &e), Transition::Unchanged);
    }

    #[test]
    fn transition_failback_when_higher_priority_elected() {
        // on secondary (idx 1), primary (idx 0) elected → fail-back
        let s = srcs(&["primary:/x", "secondary:/x"]);
        let e = Election::Elected {
            source: "primary:/x".into(),
            index: 0,
        };
        assert_eq!(
            transition(&s, Some("secondary:/x"), &e),
            Transition::FailBack {
                to: "primary:/x".into()
            }
        );
    }

    #[test]
    fn transition_degrade_when_lower_priority_elected() {
        // on primary (idx 0), secondary (idx 1) elected → degrade
        let s = srcs(&["primary:/x", "secondary:/x"]);
        let e = Election::Elected {
            source: "secondary:/x".into(),
            index: 1,
        };
        assert_eq!(
            transition(&s, Some("primary:/x"), &e),
            Transition::Degrade {
                to: "secondary:/x".into()
            }
        );
    }

    #[test]
    fn transition_mount_when_nothing_currently_mounted() {
        let s = srcs(&["a", "b"]);
        let e = Election::Elected {
            source: "a".into(),
            index: 0,
        };
        assert_eq!(
            transition(&s, None, &e),
            Transition::Mount { to: "a".into() }
        );
    }

    #[test]
    fn transition_mount_when_current_not_in_ordered_list() {
        // mounted from a source no longer declared → treat as a fresh mount
        let s = srcs(&["a", "b"]);
        let e = Election::Elected {
            source: "a".into(),
            index: 0,
        };
        assert_eq!(
            transition(&s, Some("legacy:/x"), &e),
            Transition::Mount { to: "a".into() }
        );
    }

    // ── RemountAggression ─────────────────────────────────────────────────

    #[test]
    fn aggression_defaults_to_safe() {
        assert_eq!(RemountAggression::default(), RemountAggression::Safe);
    }

    #[test]
    fn aggression_from_policy_only_force_opts_in() {
        assert_eq!(
            RemountAggression::from_policy(Some("force")),
            RemountAggression::Force
        );
        assert_eq!(
            RemountAggression::from_policy(Some(" Force_Remount ")),
            RemountAggression::Force
        );
        assert_eq!(
            RemountAggression::from_policy(Some("always")),
            RemountAggression::Safe
        );
        assert_eq!(
            RemountAggression::from_policy(None),
            RemountAggression::Safe
        );
        assert_eq!(
            RemountAggression::from_policy(Some("")),
            RemountAggression::Safe
        );
    }
}
