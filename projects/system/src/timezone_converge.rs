//! Plans guest timezone convergence — the durable half of orca#570.
//!
//! The incident: thor ran `America/Adak`, three hours off, while NTP-synced and
//! reporting healthy, and four different zones were live across twelve LXCs. Guest
//! timezone does **not** inherit from the host, and nothing in orca owned it, so
//! the drift was invisible until timestamps were compared by hand. The fleet is
//! converged today, but that was a manual sweep: a guest provisioned tomorrow can
//! still come up wrong and nothing would notice. That is what this closes.
//!
//! Pure planner over (desired, observed), matching [`crate::mount_converge::plan`]
//! — the observe and apply sides are IO, the decision is not, and the decision is
//! where the bugs that matter live.
//!
//! ## Why this ships without an applier
//!
//! Observing a guest's zone works today: `cat /etc/timezone` is on the
//! [`crate::lxc_exec::ALLOWED_COMMANDS`] allowlist. **Applying** one does not.
//! Writing `/etc/timezone` alone does not change a Debian guest's effective zone
//! — that needs the `/etc/localtime` symlink repointed (`ln`), or
//! `timedatectl set-timezone`, or `dpkg-reconfigure tzdata`. None are
//! allowlisted, and the allowlist's own comment is explicit that each entry
//! widens what can run as root inside a container.
//!
//! So the planner emits the intent and stops. Turning [`TzAction`] into a real
//! change is a deliberate allowlist decision, not something to smuggle in behind
//! a convergence loop.

/// A guest's observed timezone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestTz {
    /// Guest identifier (an LXC vmid, as a string).
    pub id: String,
    /// Zone name read from the guest, or `None` when it could not be read.
    pub timezone: Option<String>,
}

/// What convergence wants done to one guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TzAction {
    /// The guest's zone differs from desired; set it.
    Set {
        id: String,
        /// What it currently reads, for the audit trail. An operator seeing
        /// `Adak -> Denver` learns something that `-> Denver` alone does not.
        from: String,
        to: String,
    },
    /// The zone could not be read. Deliberately NOT a `Set`: orca does not know
    /// what it would be changing, and a guest whose `/etc/timezone` is
    /// unreadable is usually a broken or still-provisioning guest, where writing
    /// system files blind is the wrong move. Surface it; let an operator look.
    Unknown { id: String },
}

/// Compare two zone names.
///
/// Trimmed and case-insensitive: `/etc/timezone` is conventionally exact
/// (`America/Denver`), but a hand-edited file may carry trailing whitespace or
/// odd case, and remediating that as real drift would rewrite system files on
/// every tick forever without ever converging.
fn same_zone(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Plan convergence toward `desired`.
///
/// A blank `desired` yields no actions at all: "no policy configured" must never
/// be read as "set every guest to the empty zone". That is the difference between
/// an unconfigured fleet and a wrecked one.
///
/// Output follows input order so a plan over a stable fleet is itself stable and
/// an operator diffing two runs sees real change.
pub fn plan(desired: &str, observed: &[GuestTz]) -> Vec<TzAction> {
    if desired.trim().is_empty() {
        return Vec::new();
    }
    observed
        .iter()
        .filter_map(|g| match g.timezone.as_deref() {
            // An empty-but-present zone file is drift, not unknown: something
            // truncated it, and the guest is running on an undefined zone.
            Some(tz) if tz.trim().is_empty() => Some(TzAction::Set {
                id: g.id.clone(),
                from: "(empty)".to_string(),
                to: desired.trim().to_string(),
            }),
            Some(tz) if same_zone(tz, desired) => None,
            Some(tz) => Some(TzAction::Set {
                id: g.id.clone(),
                from: tz.trim().to_string(),
                to: desired.trim().to_string(),
            }),
            None => Some(TzAction::Unknown { id: g.id.clone() }),
        })
        .collect()
}

/// Guests that need changing, ignoring the unreadable ones. For a caller that
/// wants a count of real drift without the noise of unreachable guests.
pub fn drifted(actions: &[TzAction]) -> Vec<&TzAction> {
    actions
        .iter()
        .filter(|a| matches!(a, TzAction::Set { .. }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz(id: &str, zone: Option<&str>) -> GuestTz {
        GuestTz {
            id: id.to_string(),
            timezone: zone.map(str::to_string),
        }
    }

    /// The incident, in its exact shape: thor's guest three hours off while
    /// everything reported healthy.
    #[test]
    fn the_thor_adak_drift_is_planned_with_its_origin_recorded() {
        let got = plan("America/Denver", &[tz("110", Some("America/Adak"))]);
        assert_eq!(
            got,
            vec![TzAction::Set {
                id: "110".into(),
                from: "America/Adak".into(),
                to: "America/Denver".into(),
            }],
            "the plan must record what it is changing FROM"
        );
    }

    #[test]
    fn a_converged_fleet_plans_nothing() {
        let fleet: Vec<GuestTz> = ["100", "107", "113", "117"]
            .iter()
            .map(|id| tz(id, Some("America/Denver")))
            .collect();
        assert_eq!(plan("America/Denver", &fleet), vec![]);
    }

    /// The guard that stops an unconfigured fleet from being wrecked.
    #[test]
    fn no_desired_zone_plans_nothing_rather_than_clearing_every_guest() {
        let fleet = vec![tz("100", Some("America/Adak")), tz("101", None)];
        assert_eq!(plan("", &fleet), vec![], "empty desired = no policy");
        assert_eq!(plan("   ", &fleet), vec![], "whitespace is not a policy");
    }

    /// Unreadable must not become a blind write to a guest's system files.
    #[test]
    fn an_unreadable_zone_is_unknown_not_a_set() {
        let got = plan("America/Denver", &[tz("999", None)]);
        assert_eq!(got, vec![TzAction::Unknown { id: "999".into() }]);
    }

    /// A truncated zone file IS drift — the guest is running on an undefined
    /// zone, which is different from us being unable to look.
    #[test]
    fn an_empty_zone_file_is_drift_not_unknown() {
        let got = plan("America/Denver", &[tz("113", Some(""))]);
        assert_eq!(
            got,
            vec![TzAction::Set {
                id: "113".into(),
                from: "(empty)".into(),
                to: "America/Denver".into(),
            }]
        );
    }

    /// Without this the planner would rewrite system files every tick forever and
    /// never converge.
    #[test]
    fn whitespace_and_case_differences_are_not_drift() {
        for zone in ["America/Denver\n", " America/Denver ", "america/denver"] {
            assert_eq!(
                plan("America/Denver", &[tz("1", Some(zone))]),
                vec![],
                "{zone:?} must not be treated as drift"
            );
        }
    }

    /// A real zone change must still be planned even though comparison is loose.
    #[test]
    fn a_genuinely_different_zone_is_still_caught() {
        assert_eq!(plan("America/Denver", &[tz("1", Some("UTC"))]).len(), 1);
        assert_eq!(
            plan("America/Denver", &[tz("1", Some("America/New_York"))]).len(),
            1
        );
    }

    #[test]
    fn a_mixed_fleet_plans_only_what_needs_changing_in_input_order() {
        let fleet = vec![
            tz("100", Some("America/Denver")),
            tz("107", Some("UTC")),
            tz("108", None),
            tz("110", Some("America/Adak")),
        ];
        let got = plan("America/Denver", &fleet);
        assert_eq!(got.len(), 3);
        assert!(matches!(&got[0], TzAction::Set { id, .. } if id == "107"));
        assert!(matches!(&got[1], TzAction::Unknown { id } if id == "108"));
        assert!(matches!(&got[2], TzAction::Set { id, .. } if id == "110"));
        // And the drift count excludes the guest we simply could not read.
        assert_eq!(drifted(&got).len(), 2);
    }

    #[test]
    fn an_empty_fleet_plans_nothing() {
        assert_eq!(plan("America/Denver", &[]), vec![]);
    }

    /// The desired zone is trimmed into the action, so a config value with stray
    /// whitespace does not propagate into every guest.
    #[test]
    fn the_desired_zone_is_normalized_into_the_action() {
        let got = plan(" America/Denver \n", &[tz("1", Some("UTC"))]);
        assert_eq!(
            got,
            vec![TzAction::Set {
                id: "1".into(),
                from: "UTC".into(),
                to: "America/Denver".into(),
            }]
        );
    }
}
