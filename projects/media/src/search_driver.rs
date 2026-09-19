//! Rate-limit-aware search driver (#494 epic, #500).
//!
//! Hand-driving LazyLibrarian meant per-title search storms that tripped
//! provider rate limits — Prowlarr's `/4/api` auto-blocks for ~30s after a
//! burst. This capability owns the pacing centrally: fire ONE global search
//! across all Wanted items rather than N per-title searches, honor a minimum
//! interval between bursts, and back off when a provider returns 429.
//!
//! Pure + generic: the planner is a function of `(now, wanted, rate_state,
//! policy)` — time and rate state are passed in, so the decision logic is fully
//! unit-testable and the caller owns the clock and the persisted state. No
//! tight loops: [`RateState`] carries the last-search time and a 429 cooldown,
//! and the scheduled reconcile (#503) drives this on a throttle-safe cadence.

use crate::MediaIdentity;
use serde::{Deserialize, Serialize};

/// Central pacing policy — owned by orca, not hand-managed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPolicy {
    /// Minimum seconds between search bursts (global pacing between runs).
    pub min_interval_secs: u64,
    /// Cooldown to wait after a provider 429 before searching again
    /// (Prowlarr `/4/api` blocks ~30s → default a hair above that).
    pub cooldown_secs: u64,
    /// Prefer ONE global search over per-title storms when the backend can.
    pub prefer_global: bool,
}

impl Default for SearchPolicy {
    fn default() -> Self {
        SearchPolicy {
            min_interval_secs: 300, // 5 min between bursts
            cooldown_secs: 45,      // > Prowlarr's ~30s auto-block
            prefer_global: true,
        }
    }
}

/// Persisted pacing state for one requester/provider. Epoch seconds; the caller
/// stamps `now` and stores this between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateState {
    /// When the last search burst fired.
    #[serde(default)]
    pub last_search_at: Option<u64>,
    /// When a 429-triggered cooldown expires; searching is blocked until then.
    #[serde(default)]
    pub cooldown_until: Option<u64>,
}

impl RateState {
    /// Record that a provider returned 429 at `now`: set a cooldown window.
    /// Returns the updated state (never mutates in place, so it composes).
    pub fn after_429(&self, now: u64, policy: &SearchPolicy) -> RateState {
        RateState {
            last_search_at: self.last_search_at,
            cooldown_until: Some(now + policy.cooldown_secs),
        }
    }

    /// Record that a search burst fired at `now`.
    pub fn after_search(&self, now: u64) -> RateState {
        RateState {
            last_search_at: Some(now),
            cooldown_until: self.cooldown_until,
        }
    }
}

/// Why a search must wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// Inside a 429 cooldown window.
    Throttled,
    /// Inside the minimum inter-burst interval.
    MinInterval,
}

/// The planner's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum SearchDecision {
    /// Nothing is wanted — no search needed.
    Idle,
    /// Search now. `global` = one batched search vs per-title; `items` are the
    /// Wanted identities to cover.
    Search {
        global: bool,
        items: Vec<MediaIdentity>,
    },
    /// Hold off; retry in `wait_secs`.
    Wait { wait_secs: u64, reason: WaitReason },
}

/// Decide whether to search now, given the current time, the Wanted set, the
/// persisted rate state, and the policy. Cooldown (429) takes precedence over
/// the min-interval gate; both take precedence over searching.
pub fn plan_search(
    now: u64,
    wanted: &[MediaIdentity],
    state: &RateState,
    policy: &SearchPolicy,
) -> SearchDecision {
    if wanted.is_empty() {
        return SearchDecision::Idle;
    }
    // 429 cooldown wins.
    if let Some(until) = state.cooldown_until
        && now < until
    {
        return SearchDecision::Wait {
            wait_secs: until - now,
            reason: WaitReason::Throttled,
        };
    }
    // Minimum inter-burst interval.
    if let Some(last) = state.last_search_at {
        let elapsed = now.saturating_sub(last);
        if elapsed < policy.min_interval_secs {
            return SearchDecision::Wait {
                wait_secs: policy.min_interval_secs - elapsed,
                reason: WaitReason::MinInterval,
            };
        }
    }
    SearchDecision::Search {
        global: policy.prefer_global,
        items: wanted.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(t: &str) -> MediaIdentity {
        MediaIdentity {
            title: t.into(),
            year: None,
            external_ids: vec![],
            series: None,
        }
    }

    #[test]
    fn empty_wanted_is_idle() {
        assert_eq!(
            plan_search(1000, &[], &RateState::default(), &SearchPolicy::default()),
            SearchDecision::Idle
        );
    }

    #[test]
    fn first_run_searches_globally() {
        let d = plan_search(
            1000,
            &[id("A"), id("B")],
            &RateState::default(),
            &SearchPolicy::default(),
        );
        match d {
            SearchDecision::Search { global, items } => {
                assert!(global);
                assert_eq!(items.len(), 2);
            }
            other => panic!("expected Search, got {other:?}"),
        }
    }

    #[test]
    fn cooldown_after_429_blocks_and_reports_remaining() {
        let policy = SearchPolicy::default();
        let state = RateState::default().after_429(1000, &policy);
        // 10s into a 45s cooldown → wait 35s, throttled.
        assert_eq!(
            plan_search(1010, &[id("A")], &state, &policy),
            SearchDecision::Wait {
                wait_secs: 35,
                reason: WaitReason::Throttled
            }
        );
    }

    #[test]
    fn min_interval_gate_between_bursts() {
        let policy = SearchPolicy::default(); // 300s
        let state = RateState::default().after_search(1000);
        // 100s later → still gated, wait 200s.
        assert_eq!(
            plan_search(1100, &[id("A")], &state, &policy),
            SearchDecision::Wait {
                wait_secs: 200,
                reason: WaitReason::MinInterval
            }
        );
        // 300s later → allowed.
        assert!(matches!(
            plan_search(1300, &[id("A")], &state, &policy),
            SearchDecision::Search { .. }
        ));
    }

    #[test]
    fn cooldown_takes_precedence_over_min_interval() {
        let policy = SearchPolicy::default();
        // Both a recent search AND an active cooldown → cooldown reason wins.
        let state = RateState {
            last_search_at: Some(1000),
            cooldown_until: Some(1030),
        };
        assert_eq!(
            plan_search(1010, &[id("A")], &state, &policy),
            SearchDecision::Wait {
                wait_secs: 20,
                reason: WaitReason::Throttled
            }
        );
    }
}
