//! Config-standard guards — declared, per-kind provisioning invariants validated
//! before a mutating action runs (MINIMAL-BACKUP.md §4.4).
//!
//! A [`UnitGuard`] is the typed, orca-owned replacement for the ad-hoc
//! "may cause data loss / this container is under-provisioned" prompts a guest's
//! in-place updater emits. Core owns *what* a well-formed unit of a kind must
//! look like (minimum CPU/memory, a reachable root console, a working update
//! command); a plugin declares the concrete minimums for its kinds and supplies
//! the observed [`UnitFacts`]. On a violation the caller first takes the unit's
//! minimal backup (§4.3) and then either auto-remediates the
//! [auto-remediable](GuardViolation::is_auto_remediable) shortfalls (e.g. raise
//! memory to the minimum) or refuses with a precise, typed reason.
//!
//! Everything here is pure declaration + a total check function — no I/O, no
//! provider concepts. The plugin decides how to gather [`UnitFacts`] and how to
//! remediate; core decides whether the facts satisfy the guard.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A kind's declared provisioning invariants. A provider returns one per kind it
/// wants guarded (proxmox: one for `vm`, one for `lxc`); kinds without a guard
/// are simply never checked. Every field is optional/opt-in so a guard tightens
/// only what it names — an unset minimum imposes no floor.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct UnitGuard {
    /// The kind this guard governs (`vm` / `lxc` / `stack` / …). Free string,
    /// matched against [`crate::unit::UnitId::kind`]; core never enumerates it.
    pub kind: String,
    /// Minimum CPU cores. `None` = no floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_cpu: Option<u32>,
    /// Minimum memory in MiB. `None` = no floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_mem_mb: Option<u64>,
    /// Require a reachable root console (serial getty / `pct enter`) so recovery
    /// is possible if the guest loses network. Not auto-remediable.
    #[serde(default)]
    pub require_root_console: bool,
    /// Require a working in-guest update command (the unit can update itself).
    /// Not auto-remediable.
    #[serde(default)]
    pub require_update_command: bool,
}

/// Observed facts about a concrete unit, gathered by the provider and checked
/// against its kind's [`UnitGuard`]. `None` means "not observed" — an unknown
/// value is treated as *not* satisfying a floor (fail closed) so a probe failure
/// never silently passes a guard.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct UnitFacts {
    /// Configured CPU cores, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    /// Configured memory in MiB, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_mb: Option<u64>,
    /// Whether a reachable root console was confirmed.
    #[serde(default)]
    pub has_root_console: bool,
    /// Whether a working in-guest update command was confirmed.
    #[serde(default)]
    pub has_update_command: bool,
}

/// One way a unit fails its kind's [`UnitGuard`]. Carries the required and
/// observed values so a caller can render a precise reason or drive remediation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "violation")]
pub enum GuardViolation {
    /// Fewer CPU cores than the declared minimum (or cores unknown).
    UnderCpu { min: u32, actual: Option<u32> },
    /// Less memory than the declared minimum (or memory unknown).
    UnderMem { min: u64, actual: Option<u64> },
    /// No reachable root console.
    NoRootConsole,
    /// No working in-guest update command.
    NoUpdateCommand,
}

impl GuardViolation {
    /// Whether this shortfall can be fixed by orca adjusting the unit's config
    /// (raise CPU/memory) versus requiring out-of-band operator action (a missing
    /// console or update command can't be conjured by editing the unit spec).
    /// Guides §4.4: auto-remediate the remediable, refuse on the rest.
    pub fn is_auto_remediable(&self) -> bool {
        matches!(
            self,
            GuardViolation::UnderCpu { .. } | GuardViolation::UnderMem { .. }
        )
    }

    /// A one-line human reason, for prompts and refusal messages.
    pub fn reason(&self) -> String {
        match self {
            GuardViolation::UnderCpu { min, actual } => {
                format!("cpu {} below minimum {min}", fmt_opt(actual))
            }
            GuardViolation::UnderMem { min, actual } => {
                format!("memory {}MiB below minimum {min}MiB", fmt_opt(actual))
            }
            GuardViolation::NoRootConsole => "no reachable root console".to_string(),
            GuardViolation::NoUpdateCommand => "no working update command".to_string(),
        }
    }
}

fn fmt_opt<T: std::fmt::Display>(v: &Option<T>) -> String {
    match v {
        Some(v) => v.to_string(),
        None => "unknown".to_string(),
    }
}

impl UnitGuard {
    /// A guard imposing only CPU/memory floors — the common provisioning-standard
    /// case (no console/update-command requirement).
    pub fn min_resources(kind: impl Into<String>, min_cpu: u32, min_mem_mb: u64) -> Self {
        Self {
            kind: kind.into(),
            min_cpu: Some(min_cpu),
            min_mem_mb: Some(min_mem_mb),
            require_root_console: false,
            require_update_command: false,
        }
    }

    /// Every way `facts` fails this guard, empty when the unit is compliant.
    /// Fails closed: an unset (`None`) fact never satisfies a declared floor, so a
    /// probe that couldn't read a value is reported rather than silently passed.
    pub fn check(&self, facts: &UnitFacts) -> Vec<GuardViolation> {
        let mut out = Vec::new();
        if let Some(min) = self.min_cpu
            && facts.cpu.is_none_or(|c| c < min)
        {
            out.push(GuardViolation::UnderCpu {
                min,
                actual: facts.cpu,
            });
        }
        if let Some(min) = self.min_mem_mb
            && facts.mem_mb.is_none_or(|m| m < min)
        {
            out.push(GuardViolation::UnderMem {
                min,
                actual: facts.mem_mb,
            });
        }
        if self.require_root_console && !facts.has_root_console {
            out.push(GuardViolation::NoRootConsole);
        }
        if self.require_update_command && !facts.has_update_command {
            out.push(GuardViolation::NoUpdateCommand);
        }
        out
    }

    /// True when `facts` satisfies every declared invariant.
    pub fn is_satisfied(&self, facts: &UnitFacts) -> bool {
        self.check(facts).is_empty()
    }
}

/// Partition a set of violations into `(auto_remediable, must_refuse)` — the
/// split §4.4 acts on: fix the first, refuse on the second.
pub fn partition_violations(
    violations: Vec<GuardViolation>,
) -> (Vec<GuardViolation>, Vec<GuardViolation>) {
    violations
        .into_iter()
        .partition(GuardViolation::is_auto_remediable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compliant_unit_has_no_violations() {
        let g = UnitGuard::min_resources("lxc", 2, 1024);
        let facts = UnitFacts {
            cpu: Some(2),
            mem_mb: Some(2048),
            ..Default::default()
        };
        assert!(g.is_satisfied(&facts));
        assert!(g.check(&facts).is_empty());
    }

    #[test]
    fn under_provisioned_reports_each_floor() {
        let g = UnitGuard::min_resources("lxc", 2, 2048);
        let facts = UnitFacts {
            cpu: Some(1),
            mem_mb: Some(512),
            ..Default::default()
        };
        let v = g.check(&facts);
        assert_eq!(v.len(), 2);
        assert!(v.contains(&GuardViolation::UnderCpu {
            min: 2,
            actual: Some(1)
        }));
        assert!(v.contains(&GuardViolation::UnderMem {
            min: 2048,
            actual: Some(512)
        }));
        // Both resource shortfalls are auto-remediable.
        let (fix, refuse) = partition_violations(v);
        assert_eq!(fix.len(), 2);
        assert!(refuse.is_empty());
    }

    #[test]
    fn unknown_facts_fail_closed() {
        let g = UnitGuard::min_resources("vm", 4, 4096);
        // Nothing observed → both floors reported with actual: None.
        let v = g.check(&UnitFacts::default());
        assert!(v.contains(&GuardViolation::UnderCpu {
            min: 4,
            actual: None
        }));
        assert!(v.contains(&GuardViolation::UnderMem {
            min: 4096,
            actual: None
        }));
    }

    #[test]
    fn console_and_update_requirements_are_not_auto_remediable() {
        let g = UnitGuard {
            kind: "lxc".into(),
            min_cpu: None,
            min_mem_mb: None,
            require_root_console: true,
            require_update_command: true,
        };
        let v = g.check(&UnitFacts::default());
        assert_eq!(v.len(), 2);
        let (fix, refuse) = partition_violations(v);
        assert!(fix.is_empty());
        assert_eq!(refuse.len(), 2);
        assert!(refuse.iter().all(|x| !x.is_auto_remediable()));
    }

    #[test]
    fn unset_floors_impose_nothing() {
        let g = UnitGuard {
            kind: "lxc".into(),
            ..Default::default()
        };
        assert!(g.is_satisfied(&UnitFacts::default()));
    }

    #[test]
    fn violation_roundtrips_and_reasons() {
        let v = GuardViolation::UnderMem {
            min: 1024,
            actual: Some(256),
        };
        let j = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<GuardViolation>(&j).unwrap(), v);
        assert!(v.reason().contains("256"));
        assert!(GuardViolation::NoRootConsole.reason().contains("console"));
    }
}
