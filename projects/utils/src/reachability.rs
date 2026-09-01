//! Unified per-peer reachability source of truth.
//!
//! The single authority the three dial loops consult before contacting a peer —
//! the liveness refresher (`pod::server_pod`), `replicate.pull`
//! (`db::replicate_engine`), and roster-sync (`pod::roster_sync`). They share one
//! per-peer backoff/dormant state so a down peer is dialed at most once per its
//! shared window, never re-dialed every tick by a loop that hasn't backed off.
//!
//! ## Model
//! Per peer we track a [`PeerClass`] (from config) and a [`ReachState`] that
//! moves through:
//!
//! ```text
//!   Reachable ──fail──▶ Backoff{until,streak} ──streak saturates──▶ Dormant
//!       ▲                     │                                        │
//!       └──────────── ok ─────┴──────────────── ok ────────────────────┘
//!                        (down→up: fires the catch-up hook)
//! ```
//!
//! * **Backoff** — recently failed; not dialed on the fast path until `until`.
//!   The window widens exponentially [`BACKOFF_BASE`]→[`BACKOFF_MAX`].
//! * **Dormant** — persistently unreachable (streak saturated, ~15 min). Dropped
//!   from the fast loops entirely. An `AlwaysOn` peer still gets a slow heartbeat
//!   probe every [`BACKOFF_MAX`] so we notice it return; a `Wakeable` peer is not
//!   probed at all (Phase 3 wires the wake path — until then every peer is
//!   `AlwaysOn`).
//! * **Waking** — a wake (WoL) is in flight (Phase 3). Not dialed by the loops;
//!   the wake path drives it.
//!
//! Every state read is a cheap map lookup — this module never dials. The dial
//! loops call [`should_dial`] before a probe and [`record_probe`] after; a
//! `false→true` (down→up) transition returns `became_reachable = true`, and the
//! caller invokes [`notify_reachable`] to run the registered catch-up hook.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

/// Base backoff after a peer first fails. Matches the old roster-sync /
/// replicate.pull unreachable base so behaviour is unchanged for a briefly-down
/// peer; the win is that all three loops now share one streak.
pub const BACKOFF_BASE: Duration = Duration::from_secs(60);
/// Ceiling on the backoff window. Also the slow-heartbeat cadence for a Dormant
/// `AlwaysOn` peer — one dial every 15 min is enough to notice recovery.
pub const BACKOFF_MAX: Duration = Duration::from_secs(900);
/// Fail streak at which a peer graduates from Backoff to Dormant. With a 60s
/// base doubling to the 900s cap, streak 5 is ~15 min of continuous
/// unreachability (60+120+240+480+900 ≈ 30 min of windows, saturating the cap),
/// matching the design's "~15 min unreachable → Dormant".
const DORMANT_STREAK: u32 = 5;

/// How a peer is expected to be reachable. Set from config; drives whether a
/// Dormant peer is slow-probed (`AlwaysOn`) or left entirely alone (`Wakeable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PeerClass {
    /// Always expected up. A Dormant `AlwaysOn` peer is a fault worth a slow
    /// heartbeat probe (and, in Phase 3, a health penalty).
    #[default]
    AlwaysOn,
    /// Deliberately sleeps and is network-woken on demand (Phase 3). Dormant is
    /// its healthy steady state — never probed, never penalised.
    Wakeable,
}

/// The reachability lifecycle state for one peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReachState {
    /// Last contact succeeded (or we've never failed) — dial freely.
    Reachable,
    /// Failing; suppressed until `until`. `streak` drives the exponential widen.
    Backoff { until: Instant, streak: u32 },
    /// Persistently unreachable; dropped from the fast loops.
    Dormant,
    /// A wake is in flight (Phase 3); the wake path owns the transition out.
    Waking,
}

/// A peer's reachability record.
#[derive(Clone, Copy, Debug)]
struct PeerReach {
    class: PeerClass,
    state: ReachState,
    consecutive_failures: u32,
    last_ok: Option<Instant>,
    last_change: Instant,
}

impl PeerReach {
    fn new(now: Instant) -> Self {
        Self {
            class: PeerClass::default(),
            state: ReachState::Reachable,
            consecutive_failures: 0,
            last_ok: None,
            last_change: now,
        }
    }
}

fn table() -> &'static RwLock<HashMap<String, PeerReach>> {
    static TABLE: OnceLock<RwLock<HashMap<String, PeerReach>>> = OnceLock::new();
    TABLE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Public snapshot of a peer's reachability for read-only consumers (roster
/// render, health) that must never dial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub class: PeerClass,
    pub state: ReachState,
    pub consecutive_failures: u32,
}

impl Snapshot {
    /// Whether the fast loops treat this peer as suppressed (not Reachable).
    pub fn suppressed(&self) -> bool {
        !matches!(self.state, ReachState::Reachable)
    }
    /// Whether this peer is Dormant.
    pub fn dormant(&self) -> bool {
        matches!(self.state, ReachState::Dormant)
    }
}

/// The exponential backoff window for a given fail streak (1-based), capped.
fn backoff_delay(streak: u32) -> Duration {
    let shift = streak.saturating_sub(1).min(4);
    BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_MAX)
}

/// Set a peer's class (from config). Creates the record if absent. Phase 3 calls
/// this as it loads `power: always_on | wakeable`.
pub fn set_class(peer_id: &str, class: PeerClass, now: Instant) {
    if let Ok(mut g) = table().write() {
        g.entry(peer_id.to_string())
            .or_insert_with(|| PeerReach::new(now))
            .class = class;
    }
}

/// Whether a dial loop should probe/contact this peer on the current pass.
///
/// * `Reachable` / never-seen → yes.
/// * `Backoff` → only once the window has elapsed (the retry probe).
/// * `Dormant` `AlwaysOn` → only on the slow heartbeat ([`BACKOFF_MAX`] cadence).
/// * `Dormant` `Wakeable` → never (Phase 3 wakes it explicitly).
/// * `Waking` → never (the wake path drives it).
pub fn should_dial(peer_id: &str, now: Instant) -> bool {
    let g = match table().read() {
        Ok(g) => g,
        Err(_) => return true,
    };
    match g.get(peer_id) {
        None => true,
        Some(p) => match p.state {
            ReachState::Reachable => true,
            ReachState::Backoff { until, .. } => now >= until,
            ReachState::Dormant => {
                p.class == PeerClass::AlwaysOn
                    && now.saturating_duration_since(p.last_change) >= BACKOFF_MAX
            }
            ReachState::Waking => false,
        },
    }
}

/// The outcome of recording a probe result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ProbeOutcome {
    /// This probe took the peer from a not-Reachable state back to Reachable —
    /// the caller must fire the catch-up hook ([`notify_reachable`]).
    pub became_reachable: bool,
    /// This probe pushed the peer into Dormant for the first time.
    pub became_dormant: bool,
}

/// Record a probe/contact result and advance the state machine.
///
/// Success resets the streak and returns to `Reachable`, flagging a down→up
/// transition. Failure widens the backoff and, once the streak saturates,
/// transitions to `Dormant`. A `Wakeable` peer's own down transition goes
/// straight toward Dormant without being treated as a fault (Phase 3 reads the
/// class for health).
pub fn record_probe(peer_id: &str, ok: bool, now: Instant) -> ProbeOutcome {
    let mut g = match table().write() {
        Ok(g) => g,
        Err(_) => return ProbeOutcome::default(),
    };
    let p = g
        .entry(peer_id.to_string())
        .or_insert_with(|| PeerReach::new(now));

    if ok {
        let became_reachable = !matches!(p.state, ReachState::Reachable);
        p.state = ReachState::Reachable;
        p.consecutive_failures = 0;
        p.last_ok = Some(now);
        if became_reachable {
            p.last_change = now;
        }
        return ProbeOutcome {
            became_reachable,
            became_dormant: false,
        };
    }

    p.consecutive_failures = p.consecutive_failures.saturating_add(1);
    let was_dormant = matches!(p.state, ReachState::Dormant);
    if p.consecutive_failures >= DORMANT_STREAK {
        p.state = ReachState::Dormant;
    } else {
        p.state = ReachState::Backoff {
            until: now + backoff_delay(p.consecutive_failures),
            streak: p.consecutive_failures,
        };
    }
    let became_dormant = !was_dormant && matches!(p.state, ReachState::Dormant);
    if became_dormant {
        p.last_change = now;
    }
    ProbeOutcome {
        became_reachable: false,
        became_dormant,
    }
}

/// Mark a peer as waking (Phase 3 WoL path). No-op record is created if absent.
pub fn mark_waking(peer_id: &str, now: Instant) {
    if let Ok(mut g) = table().write() {
        let p = g
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerReach::new(now));
        p.state = ReachState::Waking;
        p.last_change = now;
    }
}

/// Force a peer to Dormant (e.g. a Wakeable peer we deliberately let sleep).
pub fn mark_dormant(peer_id: &str, now: Instant) {
    if let Ok(mut g) = table().write() {
        let p = g
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerReach::new(now));
        p.state = ReachState::Dormant;
        p.last_change = now;
    }
}

/// Cheap, non-dialing read of a peer's reachability. `None` when we've never
/// recorded a probe for it (caller treats as unknown → dialable).
pub fn reachability(peer_id: &str) -> Option<Snapshot> {
    let g = table().read().ok()?;
    g.get(peer_id).map(|p| Snapshot {
        class: p.class,
        state: p.state,
        consecutive_failures: p.consecutive_failures,
    })
}

/// Drop a peer's record. Called from peer-retirement paths so cardinality stays
/// bounded by live peers (mirrors `peer_info::remove`).
pub fn remove(peer_id: &str) {
    if let Ok(mut g) = table().write() {
        g.remove(peer_id);
    }
}

/// Retain only the supplied peer ids; evict the rest. Called from the periodic
/// peer reconcile (mirrors `peer_info::retain_only`).
pub fn retain_only(active_peer_ids: &HashSet<String>) {
    if let Ok(mut g) = table().write() {
        g.retain(|k, _| active_peer_ids.contains(k));
    }
}

// ── Catch-up hook (down→up) ────────────────────────────────────────────────
// The seam that lets a down→up transition detected in `db` (replicate.pull)
// trigger a forced catch-up sync + roster resync that lives in `pod`/`server`,
// without `db` depending on `pod`. `server` registers the closure at startup;
// any loop that sees `became_reachable` calls `notify_reachable`.

type ReachableHook = Box<dyn Fn(&str) + Send + Sync>;

fn hook() -> &'static RwLock<Option<ReachableHook>> {
    static HOOK: OnceLock<RwLock<Option<ReachableHook>>> = OnceLock::new();
    HOOK.get_or_init(|| RwLock::new(None))
}

/// Register the down→up catch-up action. `server` installs a closure that forces
/// an unthrottled replicate sync + roster resync for the returning peer.
pub fn set_on_peer_reachable<F>(f: F)
where
    F: Fn(&str) + Send + Sync + 'static,
{
    if let Ok(mut g) = hook().write() {
        *g = Some(Box::new(f));
    }
}

/// Fire the registered catch-up hook for a peer that just came back. No-op if no
/// hook is registered (e.g. unit tests, early startup).
pub fn notify_reachable(peer_id: &str) {
    if let Ok(g) = hook().read()
        && let Some(f) = g.as_ref()
    {
        f(peer_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// These tests all mutate the one process-global reachability table, and one
    /// of them (`retain_only`) deliberately wipes every other peer — so they
    /// cannot run in parallel without clobbering each other. Serialize them
    /// behind a shared guard. (Production only ever calls `retain_only` with the
    /// real live-peer set, so this is a test-only concern.)
    fn guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn key(base: &str, line: u32) -> String {
        format!("{base}-{line}")
    }

    #[test]
    fn fresh_peer_is_dialable_and_unknown() {
        let _g = guard();
        let p = key("fresh", line!());
        let now = Instant::now();
        assert!(should_dial(&p, now));
        assert!(reachability(&p).is_none());
    }

    #[test]
    fn failure_arms_backoff_and_suppresses_fast_dial() {
        let _g = guard();
        let p = key("backoff", line!());
        let now = Instant::now();
        let out = record_probe(&p, false, now);
        assert!(!out.became_reachable && !out.became_dormant);
        // Inside the first 60s window the fast loops skip it.
        assert!(!should_dial(&p, now + Duration::from_secs(1)));
        // After the window elapses the retry probe is allowed.
        assert!(should_dial(&p, now + BACKOFF_BASE + Duration::from_secs(1)));
        let snap = reachability(&p).unwrap();
        assert!(snap.suppressed() && !snap.dormant());
    }

    #[test]
    fn backoff_widens_exponentially_capped() {
        let _g = guard();
        assert_eq!(backoff_delay(1), Duration::from_secs(60));
        assert_eq!(backoff_delay(2), Duration::from_secs(120));
        assert_eq!(backoff_delay(3), Duration::from_secs(240));
        assert_eq!(backoff_delay(4), Duration::from_secs(480));
        assert_eq!(backoff_delay(5), BACKOFF_MAX); // 960 capped to 900
        assert_eq!(backoff_delay(50), BACKOFF_MAX);
    }

    #[test]
    fn streak_saturation_transitions_to_dormant() {
        let _g = guard();
        let p = key("dormant", line!());
        let now = Instant::now();
        let mut last = ProbeOutcome::default();
        for _ in 0..DORMANT_STREAK {
            last = record_probe(&p, false, now);
        }
        assert!(
            last.became_dormant,
            "saturated streak should become dormant"
        );
        assert!(reachability(&p).unwrap().dormant());
        // Dormant AlwaysOn: not dialed on the fast pass, but the slow heartbeat
        // fires once BACKOFF_MAX has elapsed since the transition.
        assert!(!should_dial(&p, now + Duration::from_secs(1)));
        assert!(should_dial(&p, now + BACKOFF_MAX + Duration::from_secs(1)));
    }

    #[test]
    fn dormant_wakeable_is_never_dialed() {
        let _g = guard();
        let p = key("wakeable", line!());
        let now = Instant::now();
        set_class(&p, PeerClass::Wakeable, now);
        for _ in 0..DORMANT_STREAK {
            record_probe(&p, false, now);
        }
        assert!(reachability(&p).unwrap().dormant());
        // Even long after BACKOFF_MAX, a Wakeable dormant peer is left alone.
        assert!(!should_dial(&p, now + BACKOFF_MAX * 10));
    }

    #[test]
    fn success_after_failure_flags_down_up_transition() {
        let _g = guard();
        let p = key("recover", line!());
        let now = Instant::now();
        record_probe(&p, false, now);
        let out = record_probe(&p, true, now + Duration::from_secs(5));
        assert!(
            out.became_reachable,
            "recovery must flag the catch-up trigger"
        );
        // A second success is steady-state, not a transition.
        let out2 = record_probe(&p, true, now + Duration::from_secs(10));
        assert!(!out2.became_reachable);
        assert!(should_dial(&p, now + Duration::from_secs(10)));
    }

    #[test]
    fn success_from_dormant_recovers_and_flags_transition() {
        let _g = guard();
        let p = key("dormant-recover", line!());
        let now = Instant::now();
        for _ in 0..DORMANT_STREAK {
            record_probe(&p, false, now);
        }
        assert!(reachability(&p).unwrap().dormant());
        let out = record_probe(&p, true, now + BACKOFF_MAX);
        assert!(out.became_reachable);
        assert!(!reachability(&p).unwrap().suppressed());
    }

    #[test]
    fn remove_and_retain_only_bound_cardinality() {
        let _g = guard();
        let keep = key("keep", line!());
        let drop_it = key("drop", line!());
        let now = Instant::now();
        record_probe(&keep, false, now);
        record_probe(&drop_it, false, now);
        remove(&drop_it);
        assert!(reachability(&drop_it).is_none());
        assert!(reachability(&keep).is_some());

        let mut live = HashSet::new();
        live.insert(keep.clone());
        retain_only(&live);
        assert!(reachability(&keep).is_some());
    }

    #[test]
    fn notify_reachable_invokes_registered_hook() {
        let _g = guard();
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        set_on_peer_reachable(move |_peer| {
            h.fetch_add(1, Ordering::SeqCst);
        });
        notify_reachable("some-peer");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
