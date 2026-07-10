//! Crashloop circuit breaker — C4 implementation.
//!
//! ## Policy summary
//!
//! The reconciler asks the breaker for a [`BreakerDecision`] right before it
//! starts a container that previously exited non-zero (or, for lxc, that the
//! reconciler observes flapping). The breaker:
//!
//! 1. Loads the persisted [`BreakerRecord`] for `(host, runtime, container_id)`
//!    from [`BreakerStore`] (or creates a fresh `Watching` record on first
//!    contact).
//! 2. Updates the sliding 5-minute observation window with the current
//!    observation, runtime-aware:
//!    - **docker** — folds `Container.restart_count` deltas against
//!      `restart_count_snapshot`; also folds the current non-zero exit
//!      timestamp into `recent_starts`.
//!    - **lxc** — folds observed state transitions (running ↔ stopped)
//!      into `recent_starts`; reads the journalctl tail in
//!      [`HostObservation`] for the `pve-container@<vmid>.service` unit.
//! 3. Runs the pure [`classify`] function against the updated record.
//! 4. Returns [`BreakerDecision::Hold { reason }`] if classify trips,
//!    otherwise [`BreakerDecision::Proceed`].
//!
//! ## Recovery (operator-only, deliberate)
//!
//! There is **no automatic timeout-based recovery**. The breaker clears on
//! operator action — `containers.unhold` — so that a real crashloop stays
//! surfaced in `containers.pending` until a human looks at it. An automatic
//! timer that re-armed the start would mask the very class of failure the
//! breaker exists to catch (intermittent flap that lands inside the auto-
//! clear window forever).
//!
//! ## Storage
//!
//! C4 ships a JSON-file [`FileStore`] at
//! `<orca_home>/containers/breaker_state.json` with tmp-file-and-rename
//! atomic writes. The plugin-namespaced db primitive
//! (`project_sdk_plugin_namespaced_db`) is still a CONCEPT — once that
//! lands, callers swap [`BreakerStore`] for a db-backed impl; the
//! [`BreakerRecord`] surface does not change.
//!
//! TODO(c4-followup): migrate to plugin-namespaced db once
//! project_sdk_plugin_namespaced_db lands.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use utils::time::Timestamp;

use crate::{Container, ContainerState, RuntimeKind};

// ── Constants ──────────────────────────────────────────────────────────────

/// Sliding window the breaker uses for restart-count / transition / journal
/// classification. 5 minutes.
pub const OBSERVATION_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Restart-count threshold for docker `RestartStormIn5Min`.
pub const DOCKER_RESTART_THRESHOLD: u64 = 3;

/// Window for "fast re-exit after orca-issued start" classification.
pub const FAST_REEXIT_WITHIN: Duration = Duration::from_secs(60);

/// LXC transition threshold for `LxcFlappingIn5Min`.
pub const LXC_TRANSITION_THRESHOLD: u32 = 3;

/// LXC journalctl failure-line threshold for `LxcJournalFailuresIn5Min`.
pub const LXC_JOURNAL_FAILURE_THRESHOLD: u32 = 3;

// ── Public types ───────────────────────────────────────────────────────────

/// Persisted breaker state keyed by `(host, runtime, container_id)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakerRecord {
    pub host: String,
    pub runtime: RuntimeKind,
    pub container_id: String,
    /// Timestamp of the last orca-initiated start. Used by the
    /// "fast re-exit" docker classifier.
    pub last_orca_start_at: Option<Timestamp>,
    /// docker `RestartCount` baseline captured at last reconcile. The
    /// classifier compares the current `Container.restart_count` against
    /// this to detect a runaway restart loop driven by docker itself
    /// (not by orca).
    pub restart_count_snapshot: Option<u64>,
    /// Sliding-window timestamps. For docker: starts the reconciler took
    /// + observed non-zero exits. For lxc: state-transition timestamps.
    ///
    /// Entries older than [`OBSERVATION_WINDOW`] are dropped on every touch.
    pub recent_starts: Vec<Timestamp>,
    pub status: BreakerStatus,
    pub held_reason: Option<HoldReason>,
    pub held_since: Option<Timestamp>,
    /// Suppress repeat `containers.held` notifications while a single
    /// hold is active. Reset by [`unhold`].
    pub notified_at: Option<Timestamp>,
    /// Container state observed at the end of the previous arm() call.
    /// Used by [`fold_lxc`] to detect stopped→running transitions
    /// without requiring the caller to thread cross-tick state. Set
    /// inside [`arm`] just before save. `None` on a fresh record (no
    /// prior observation to compare against).
    #[serde(default)]
    pub last_observed_state: Option<ContainerState>,
}

impl BreakerRecord {
    /// Fresh record for a never-seen container. Status = Watching, no
    /// hold history.
    pub fn fresh(host: &str, runtime: RuntimeKind, container_id: &str) -> Self {
        Self {
            host: host.to_string(),
            runtime,
            container_id: container_id.to_string(),
            last_orca_start_at: None,
            restart_count_snapshot: None,
            recent_starts: Vec::new(),
            status: BreakerStatus::Watching,
            held_reason: None,
            held_since: None,
            notified_at: None,
            last_observed_state: None,
        }
    }

    /// Drop entries from `recent_starts` older than the observation
    /// window relative to `now`.
    pub fn prune_window(&mut self, now: Timestamp) {
        let cutoff = now.minus(OBSERVATION_WINDOW);
        self.recent_starts.retain(|t| *t >= cutoff);
    }
}

/// Closed lifecycle of a breaker record. No `Other(String)` escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerStatus {
    /// Default — observing, not blocking starts.
    Watching,
    /// Crossed an early signal but not the hard threshold. C4 does not
    /// surface a separate trip for this; it's a forward-compatible slot
    /// for §2.1 escalation policy and is currently only set externally
    /// by callers that want to mark a container under heightened
    /// scrutiny without blocking the start.
    TentativeHold,
    /// Crashloop tripped. Reconciler short-circuits to no-op on every
    /// subsequent tick until `containers.unhold` clears the record.
    Held,
}

/// Closed enum of trip reasons. Each variant carries the numeric context
/// the operator needs to understand the trip without re-running the
/// observation. No `Other(String)` escape hatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HoldReason {
    /// docker `RestartCount` jumped by >3 inside the 5-minute window —
    /// the runtime itself is throwing the container into a loop.
    RestartStormIn5Min { count: u32, window_start: Timestamp },
    /// docker container exited non-zero within 60s of an orca-initiated
    /// start — we started it, it died fast, we're not starting it again
    /// until an operator looks.
    FastReexitAfterOrcaStart { within_secs: u32, exit_code: i32 },
    /// LXC state flapped between stopped↔running more than 3 times in
    /// the 5-minute window.
    LxcFlappingIn5Min {
        transitions: u32,
        window_start: Timestamp,
    },
    /// `pve-container@<vmid>.service` logged >3 failure lines in 5min.
    LxcJournalFailuresIn5Min { count: u32, window_start: Timestamp },
}

/// What the reconciler should do with a flagged-tentative start request.
///
/// Closed enum. C4 changes `Hold` to carry the trip reason so the call
/// site can stamp the same reason into the persisted record without
/// re-classifying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerDecision {
    Proceed,
    Hold {
        reason: HoldReason,
        /// `Some(_)` when a prior tick already notified for this sticky
        /// hold; `None` on a fresh trip. Lets callers suppress repeat
        /// notifications without a second `store.load()` after arm.
        notified_at: Option<Timestamp>,
    },
}

/// Per-tick observation captured by the caller. Runtime-specific signals
/// the classifier can't synthesize from a single `Container` row land
/// here. Optional fields stay `None` when the runtime doesn't have a
/// corresponding signal cheap enough to gather every tick.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HostObservation {
    /// Raw output of `journalctl -u pve-container@<vmid>.service
    /// --since "5 min ago" --no-pager -o cat`. Populated by the LXC
    /// arming path only.
    ///
    /// Filter (case-insensitive, matched per line):
    /// - contains `failed to start`
    /// - OR contains `exited with status`
    pub lxc_journal_tail: Option<String>,
    /// Previous observed state of the LXC, if known. The reconciler
    /// passes this in so the classifier can detect a stopped→running
    /// transition without owning a separate persistent state field.
    pub lxc_previous_state: Option<ContainerState>,
}

// ── Storage trait ──────────────────────────────────────────────────────────

/// Persistence boundary. Lets tests inject a [`MemoryStore`] without
/// touching the filesystem.
pub trait BreakerStore: Send + Sync {
    fn load(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<Option<BreakerRecord>, BreakerError>;

    fn save(&self, record: &BreakerRecord) -> Result<(), BreakerError>;

    /// Return every record currently in the store. Used by
    /// `containers.pending`.
    fn list(&self) -> Result<Vec<BreakerRecord>, BreakerError>;

    /// Garbage-collect records whose container is no longer live, so the
    /// store's cardinality stays bounded by the fleet rather than by every
    /// container that ever flapped.
    ///
    /// `live_keys` is the set of `(host, runtime, container_id)` the
    /// reconciler observed this pass. A record is evicted only when BOTH:
    ///   * its key is absent from `live_keys` (container gone), AND
    ///   * it is NOT operator-actionable.
    ///
    /// `Held` records are operator-actionable — they surface in
    /// `containers.pending` and clear only via `containers.unhold` — so they
    /// are retained even when the container has disappeared (a crashlooped
    /// container that the runtime finally removed must still show its hold
    /// until an operator acknowledges it). Everything else (`Watching`,
    /// `TentativeHold`) is pure observation state and is safe to drop.
    fn retain_active(
        &self,
        live_keys: &std::collections::HashSet<(String, RuntimeKind, String)>,
    ) -> Result<(), BreakerError>;
}

/// Whether a breaker record must survive eviction regardless of whether its
/// container is still live. `Held` records are operator-actionable (listed in
/// `containers.pending`, cleared only by `containers.unhold`); dropping one
/// because the runtime GC'd the crashlooped container would silently erase a
/// hold the operator never acknowledged.
fn breaker_record_is_actionable(record: &BreakerRecord) -> bool {
    record.status == BreakerStatus::Held
}

/// Errors the breaker surface returns. Closed.
#[derive(Debug, thiserror::Error)]
pub enum BreakerError {
    #[error("breaker store I/O: {0}")]
    Io(String),
    #[error("breaker store decode: {0}")]
    Decode(String),
    #[error("breaker record not found: host={host} runtime={runtime:?} id={container_id}")]
    NotFound {
        host: String,
        runtime: RuntimeKind,
        container_id: String,
    },
    #[error("container is not in Held status (current={current:?}); cannot unhold")]
    NotHeld { current: BreakerStatus },
}

// ── File-backed store ──────────────────────────────────────────────────────

/// JSON-file store at `<root>/breaker_state.json`. Writes go to a sibling
/// `.tmp` file and rename atomically.
pub struct FileStore {
    path: PathBuf,
    inner: Mutex<HashMap<RecordKey, BreakerRecord>>,
    loaded: Mutex<bool>,
}

/// Composite key used by [`FileStore`] internally. Kept private because
/// it's an implementation detail of the JSON-file layout.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RecordKey {
    host: String,
    runtime: RuntimeKind,
    container_id: String,
}

impl RecordKey {
    fn from_record(r: &BreakerRecord) -> Self {
        Self {
            host: r.host.clone(),
            runtime: r.runtime,
            container_id: r.container_id.clone(),
        }
    }
}

/// On-disk JSON layout. Stored as a list rather than a map because the
/// composite key doesn't round-trip through JSON object keys cleanly.
#[derive(Debug, Serialize, Deserialize, Default)]
struct FileLayout {
    records: Vec<BreakerRecord>,
}

impl FileStore {
    /// Construct a store rooted at `dir`. The directory is created on
    /// first save if it doesn't exist; load is lazy.
    pub fn new(dir: PathBuf) -> Self {
        let path = dir.join("breaker_state.json");
        Self {
            path,
            inner: Mutex::new(HashMap::new()),
            loaded: Mutex::new(false),
        }
    }

    /// Default location: `<orca_home>/containers/breaker_state.json`.
    /// Resolution mirrors `files::ops::orca_home` (inlined to avoid
    /// pulling the `files` crate into containers).
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("ORCA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".orca")))?;
        Some(home.join("containers"))
    }

    fn ensure_loaded(
        &self,
        guard: &mut MutexGuard<'_, HashMap<RecordKey, BreakerRecord>>,
    ) -> Result<(), BreakerError> {
        let mut loaded_g = self
            .loaded
            .lock()
            .map_err(|e| BreakerError::Io(format!("loaded mutex poisoned: {e}")))?;
        if *loaded_g {
            return Ok(());
        }
        if self.path.exists() {
            let bytes = fs::read(&self.path)
                .map_err(|e| BreakerError::Io(format!("read {}: {e}", self.path.display())))?;
            if !bytes.is_empty() {
                let layout: FileLayout = serde_json::from_slice(&bytes)
                    .map_err(|e| BreakerError::Decode(format!("{}: {e}", self.path.display())))?;
                for r in layout.records {
                    guard.insert(RecordKey::from_record(&r), r);
                }
            }
        }
        *loaded_g = true;
        Ok(())
    }

    fn flush(
        &self,
        guard: &MutexGuard<'_, HashMap<RecordKey, BreakerRecord>>,
    ) -> Result<(), BreakerError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| BreakerError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        let layout = FileLayout {
            records: guard.values().cloned().collect(),
        };
        let body = serde_json::to_vec_pretty(&layout)
            .map_err(|e| BreakerError::Decode(format!("encode: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        // Scoped so the file handle is dropped (and its buffer flushed)
        // before the rename — required on macOS/BSD where renaming an
        // open file can race the fsync.
        {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| BreakerError::Io(format!("create {}: {e}", tmp.display())))?;
            f.write_all(&body)
                .map_err(|e| BreakerError::Io(format!("write {}: {e}", tmp.display())))?;
            f.sync_all()
                .map_err(|e| BreakerError::Io(format!("fsync {}: {e}", tmp.display())))?;
        }
        fs::rename(&tmp, &self.path).map_err(|e| {
            BreakerError::Io(format!(
                "rename {} → {}: {e}",
                tmp.display(),
                self.path.display()
            ))
        })?;
        Ok(())
    }
}

impl BreakerStore for FileStore {
    fn load(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<Option<BreakerRecord>, BreakerError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        let key = RecordKey {
            host: host.to_string(),
            runtime,
            container_id: container_id.to_string(),
        };
        Ok(guard.get(&key).cloned())
    }

    fn save(&self, record: &BreakerRecord) -> Result<(), BreakerError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        guard.insert(RecordKey::from_record(record), record.clone());
        self.flush(&guard)
    }

    fn list(&self) -> Result<Vec<BreakerRecord>, BreakerError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        Ok(guard.values().cloned().collect())
    }

    fn retain_active(
        &self,
        live_keys: &std::collections::HashSet<(String, RuntimeKind, String)>,
    ) -> Result<(), BreakerError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        let before = guard.len();
        guard.retain(|key, record| {
            let live =
                live_keys.contains(&(key.host.clone(), key.runtime, key.container_id.clone()));
            live || breaker_record_is_actionable(record)
        });
        if guard.len() != before {
            self.flush(&guard)?;
        }
        Ok(())
    }
}

// ── In-memory store (tests / first-boot before any persistence path) ──────

/// Thread-safe in-memory store. Used in tests and as a safe default
/// when no on-disk state dir is resolvable.
pub struct MemoryStore {
    inner: Mutex<HashMap<RecordKey, BreakerRecord>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl BreakerStore for MemoryStore {
    fn load(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<Option<BreakerRecord>, BreakerError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("memory mutex poisoned: {e}")))?;
        let key = RecordKey {
            host: host.to_string(),
            runtime,
            container_id: container_id.to_string(),
        };
        Ok(guard.get(&key).cloned())
    }

    fn save(&self, record: &BreakerRecord) -> Result<(), BreakerError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("memory mutex poisoned: {e}")))?;
        guard.insert(RecordKey::from_record(record), record.clone());
        Ok(())
    }

    fn list(&self) -> Result<Vec<BreakerRecord>, BreakerError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("memory mutex poisoned: {e}")))?;
        Ok(guard.values().cloned().collect())
    }

    fn retain_active(
        &self,
        live_keys: &std::collections::HashSet<(String, RuntimeKind, String)>,
    ) -> Result<(), BreakerError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| BreakerError::Io(format!("memory mutex poisoned: {e}")))?;
        guard.retain(|key, record| {
            let live =
                live_keys.contains(&(key.host.clone(), key.runtime, key.container_id.clone()));
            live || breaker_record_is_actionable(record)
        });
        Ok(())
    }
}

// ── Arming entry point ────────────────────────────────────────────────────

/// Inputs to [`arm`]. Carries everything the breaker needs to make a
/// decision plus persist the result.
pub struct ArmRequest<'a> {
    pub container: &'a Container,
    pub observation: &'a HostObservation,
    pub now: Timestamp,
    pub store: &'a dyn BreakerStore,
    /// `true` when the reconciler is about to call `adapter.start()` on a
    /// `Proceed` decision — the breaker stamps `last_orca_start_at` and,
    /// for runtimes where the fold doesn't own start counting (see
    /// `fold_owns_start_count`), pushes `recent_starts`. `false` means
    /// observe-only: fold the observation, classify, persist
    /// `last_observed_state`, but don't mint a start event. LXC arms
    /// every tick in observe mode (see `arm_on_every_start`); its
    /// `recent_starts` count comes from `fold_lxc`'s next-tick
    /// transition detection, not from arm itself.
    pub initiating_start: bool,
}

/// Ask the breaker whether the reconciler should proceed with a start
/// (initiating) or just record an observation. Loads-or-fresh, folds
/// the current observation into the persisted sliding window,
/// classifies, persists, returns the decision.
///
/// On `Hold`: stamps `status = Held`, `held_reason`, `held_since`.
/// `notified_at` is left `None` here — the caller stamps it via
/// [`mark_notified`] after a successful notification dispatch, so a
/// dropped alert can be retried on the next tick.
///
/// On `Proceed`:
/// - `recent_starts` gets a fresh `now` only when `initiating_start`
///   is true AND the runtime relies on the reconciler to inject the
///   start intent (`!fold_owns_start_count(runtime)`). LXC's fold
///   counts observed transitions on its own; pushing here would
///   double-count.
/// - `last_orca_start_at` is stamped only when `initiating_start` is
///   true — observation-only ticks must not move the anchor used by
///   the docker fast-reexit classifier.
/// - `restart_count_snapshot` is refreshed every Proceed; it matters
///   only for `fold_docker` (LXC's `restart_count` is always 0) but
///   keeping the refresh unconditional keeps the bookkeeping uniform.
pub fn arm(req: ArmRequest<'_>) -> Result<BreakerDecision, BreakerError> {
    let container = req.container;
    let mut record = match req
        .store
        .load(&container.host, container.runtime, &container.id)?
    {
        Some(r) => r,
        None => BreakerRecord::fresh(&container.host, container.runtime, &container.id),
    };

    // If we already tripped on a previous tick, short-circuit: the
    // reconciler's job is to no-op until unhold clears us. The hold is
    // sticky on purpose.
    if record.status == BreakerStatus::Held
        && let Some(reason) = record.held_reason.clone()
    {
        // Keep `last_observed_state` fresh even on the Held short-circuit
        // so that after `unhold()` clears, the next tick's prev-state
        // lookup reflects reality instead of whatever was observed at
        // trip-time. Persist before returning.
        let notified_at = record.notified_at;
        record.last_observed_state = Some(container.state);
        req.store.save(&record)?;
        return Ok(BreakerDecision::Hold {
            reason,
            notified_at,
        });
    }

    record.prune_window(req.now);

    // Overlay the caller's observation with the cross-tick state we
    // own. `lxc_previous_state` is authoritatively the state observed
    // at the end of the *previous* arm() call — callers cannot supply
    // it accurately (they don't see prior ticks). When the caller
    // passes Some(...), it's an explicit override (tests, planned
    // probe-driven recovery paths); otherwise we inject from the record.
    let observation_owned;
    let observation: &HostObservation = if req.observation.lxc_previous_state.is_some() {
        req.observation
    } else {
        observation_owned = HostObservation {
            lxc_journal_tail: req.observation.lxc_journal_tail.clone(),
            lxc_previous_state: record.last_observed_state,
        };
        &observation_owned
    };

    // Fold the current observation into the sliding window, runtime-
    // aware.
    fold_observation(&mut record, container, observation, req.now);

    // Classify against the updated record.
    let decision = match classify(&record, container, observation, req.now) {
        Some(reason) => {
            record.status = BreakerStatus::Held;
            record.held_reason = Some(reason.clone());
            record.held_since = Some(req.now);
            // notified_at left None; the caller stamps it after the
            // first successful notification dispatch.
            // Fresh trip — caller stamps `notified_at` via `mark_notified`
            // after the first successful dispatch, so it's `None` here.
            BreakerDecision::Hold {
                reason,
                notified_at: None,
            }
        }
        None => {
            // No trip. Three things may need to happen, on different
            // conditions:
            //
            // 1. `recent_starts.push` — captures "we minted a start." Only
            //    on `initiating_start`, and only for runtimes whose fold
            //    *doesn't* already derive starts from observed state. LXC
            //    is excluded because `fold_lxc` will detect the next-tick
            //    `Exited → Running` transition and push then; pushing here
            //    would double-count. Docker keeps the push: `fold_docker`
            //    relies on `restart_count` delta which lags an orca-issued
            //    start by a tick.
            // 2. `last_orca_start_at` — only on `initiating_start`. This
            //    timestamp is the anchor for the fast-reexit classifier;
            //    observe-only ticks must not move it.
            // 3. `restart_count_snapshot` — refreshed every tick so docker
            //    deltas are computed against the freshest baseline.
            if req.initiating_start && !fold_owns_start_count(container.runtime) {
                record.recent_starts.push(req.now);
            }
            if req.initiating_start {
                record.last_orca_start_at = Some(req.now);
            }
            record.restart_count_snapshot = Some(u64::from(container.restart_count));
            BreakerDecision::Proceed
        }
    };

    // Stamp the current state for the next tick's prev-state lookup.
    // Done unconditionally — both Hold and Proceed branches need it, and
    // the Held short-circuit at the top of this function reads only
    // `status`/`held_reason`, so we never overwrite a sticky state.
    record.last_observed_state = Some(container.state);

    req.store.save(&record)?;
    Ok(decision)
}

/// Mark that the `containers.held` notification has been dispatched for
/// the current hold so the next tick can suppress a repeat. Idempotent.
pub fn mark_notified(
    store: &dyn BreakerStore,
    host: &str,
    runtime: RuntimeKind,
    container_id: &str,
    when: Timestamp,
) -> Result<(), BreakerError> {
    let Some(mut record) = store.load(host, runtime, container_id)? else {
        return Err(BreakerError::NotFound {
            host: host.to_string(),
            runtime,
            container_id: container_id.to_string(),
        });
    };
    record.notified_at = Some(when);
    store.save(&record)
}

/// Operator-driven recovery. Loads the record, requires `Held`, clears
/// the hold state, persists, and returns the cleared record.
pub fn unhold(
    store: &dyn BreakerStore,
    host: &str,
    runtime: RuntimeKind,
    container_id: &str,
) -> Result<BreakerRecord, BreakerError> {
    let Some(mut record) = store.load(host, runtime, container_id)? else {
        return Err(BreakerError::NotFound {
            host: host.to_string(),
            runtime,
            container_id: container_id.to_string(),
        });
    };
    if record.status != BreakerStatus::Held {
        return Err(BreakerError::NotHeld {
            current: record.status,
        });
    }
    record.status = BreakerStatus::Watching;
    record.held_reason = None;
    record.held_since = None;
    record.notified_at = None;
    record.recent_starts.clear();
    // Leave `restart_count_snapshot` and `last_orca_start_at` as
    // historical breadcrumbs. The next `arm()` will refresh them on the
    // first start.
    store.save(&record)?;
    Ok(record)
}

// ── Per-runtime arming policy ─────────────────────────────────────────────
//
// Two questions collapse into one predicate each:
//
// * `arm_on_every_start` — should `run_start_pipeline` arm regardless of
//   the docker-style `Exited && exit_code != 0` tentative gate? True
//   for runtimes whose adapters don't surface `exit_code` (LXC). Also
//   used in the reconcile dispatch loop's NoOp branch to enable per-
//   tick observation while a container is running — the only way the
//   cross-tick `last_observed_state` and journalctl tail stay current.
//
// * `fold_owns_start_count` — does the runtime's `fold_*` derive
//   `recent_starts` from observed state (so `arm()` must NOT push on
//   `initiating_start`), or does it rely on the reconciler to inject
//   the start intent (so `arm()` must push)? True for LXC (`fold_lxc`
//   counts observed Exited→Running transitions); false for docker
//   (`fold_docker` only catches docker-initiated restarts via the
//   restart_count delta, so orca-initiated starts must be stamped by
//   `arm()` directly).
//
// Adding a new runtime classifier — say podman with its own
// restart_count semantics, or nspawn with a different observation
// signal — means editing these two predicates, plus adding the
// `fold_*` and `classify_*` arms below. The adapter trait stays
// breaker-agnostic.

/// Whether `arm()` should fire on every tick this runtime presents
/// for arming, not just on the docker-style tentative gate. See module
/// comment above this function.
pub fn arm_on_every_start(runtime: RuntimeKind) -> bool {
    matches!(runtime, RuntimeKind::Lxc)
}

/// Whether the runtime's fold derives `recent_starts` from observed
/// state. If true, `arm()` must not push on `initiating_start` (the
/// next tick's fold will catch the transition). See module comment.
fn fold_owns_start_count(runtime: RuntimeKind) -> bool {
    matches!(runtime, RuntimeKind::Lxc)
}

// ── Window-folding ────────────────────────────────────────────────────────

/// Update sliding-window state with the current observation. Runtime-
/// specific signals (docker restart-count deltas, lxc transition
/// detection) fold here, before classification.
fn fold_observation(
    record: &mut BreakerRecord,
    container: &Container,
    observation: &HostObservation,
    now: Timestamp,
) {
    match container.runtime {
        RuntimeKind::Docker => fold_docker(record, container, now),
        RuntimeKind::Lxc => fold_lxc(record, container, observation, now),
        RuntimeKind::Podman | RuntimeKind::Nspawn => {
            // No runtime-specific fold yet — the docker model is a
            // reasonable approximation for podman; nspawn has no
            // restart-count signal. Both fall through to the
            // exit-code path only.
        }
    }
}

fn fold_docker(record: &mut BreakerRecord, container: &Container, now: Timestamp) {
    let current = u64::from(container.restart_count);
    // First contact — establish the baseline without minting fake
    // history.
    if record.restart_count_snapshot.is_none() {
        record.restart_count_snapshot = Some(current);
        return;
    }
    let baseline = record.restart_count_snapshot.unwrap_or(0);
    if current > baseline {
        // Each unit of increase is one "start" the runtime performed
        // without our knowledge. We don't know the precise timestamps,
        // so we stamp them all at `now` — same observation window,
        // identical answer.
        let delta = (current - baseline).min(1024); // sanity clamp
        for _ in 0..delta {
            record.recent_starts.push(now);
        }
        record.restart_count_snapshot = Some(current);
    }
}

fn fold_lxc(
    record: &mut BreakerRecord,
    container: &Container,
    observation: &HostObservation,
    now: Timestamp,
) {
    // Stopped → running transition = one "start" event.
    if let Some(prev) = observation.lxc_previous_state {
        let started_now = matches!(container.state, ContainerState::Running);
        let was_stopped = matches!(
            prev,
            ContainerState::Exited | ContainerState::Dead | ContainerState::Stopping
        );
        if started_now && was_stopped {
            record.recent_starts.push(now);
        }
    }
}

// ── Classifier ────────────────────────────────────────────────────────────

/// Pure classifier. Returns the trip reason if the breaker should open,
/// or `None` if the start is safe to proceed.
///
/// Trip conditions:
///
/// **docker**
/// - `Container.restart_count` increased by more than
///   [`DOCKER_RESTART_THRESHOLD`] inside [`OBSERVATION_WINDOW`].
/// - OR `Container.state == Exited` with `exit_code != 0` observed
///   within [`FAST_REEXIT_WITHIN`] of `last_orca_start_at`.
///
/// **lxc**
/// - More than [`LXC_TRANSITION_THRESHOLD`] state transitions tracked in
///   `recent_starts` inside [`OBSERVATION_WINDOW`].
/// - OR more than [`LXC_JOURNAL_FAILURE_THRESHOLD`] lines in
///   `observation.lxc_journal_tail` matching (case-insensitive)
///   `failed to start` or `exited with status`. The exact filter is the
///   five-minute tail of `journalctl -u
///   pve-container@<vmid>.service --since "5 min ago" --no-pager -o
///   cat`.
pub fn classify(
    record: &BreakerRecord,
    container: &Container,
    observation: &HostObservation,
    now: Timestamp,
) -> Option<HoldReason> {
    match container.runtime {
        RuntimeKind::Docker => classify_docker(record, container, now),
        RuntimeKind::Lxc => classify_lxc(record, observation, now),
        RuntimeKind::Podman | RuntimeKind::Nspawn => None,
    }
}

fn classify_docker(
    record: &BreakerRecord,
    container: &Container,
    now: Timestamp,
) -> Option<HoldReason> {
    // Restart-storm path.
    if record.recent_starts.len() as u64 > DOCKER_RESTART_THRESHOLD {
        let window_start = record
            .recent_starts
            .iter()
            .min()
            .copied()
            .unwrap_or(now.minus(OBSERVATION_WINDOW));
        return Some(HoldReason::RestartStormIn5Min {
            count: record.recent_starts.len() as u32,
            window_start,
        });
    }

    // Fast-reexit path. Requires (a) we issued a start, (b) container
    // currently exited non-zero, (c) the gap is under FAST_REEXIT_WITHIN.
    if let Some(last_start) = record.last_orca_start_at
        && matches!(container.state, ContainerState::Exited)
        && let Some(code) = container.exit_code
        && code != 0
    {
        let elapsed_secs = now.unix_seconds() - last_start.unix_seconds();
        if elapsed_secs >= 0 && Duration::from_secs(elapsed_secs as u64) <= FAST_REEXIT_WITHIN {
            return Some(HoldReason::FastReexitAfterOrcaStart {
                within_secs: elapsed_secs.max(0) as u32,
                exit_code: code,
            });
        }
    }

    None
}

fn classify_lxc(
    record: &BreakerRecord,
    observation: &HostObservation,
    now: Timestamp,
) -> Option<HoldReason> {
    // Flapping path: transitions tracked in recent_starts.
    if record.recent_starts.len() as u32 > LXC_TRANSITION_THRESHOLD {
        let window_start = record
            .recent_starts
            .iter()
            .min()
            .copied()
            .unwrap_or(now.minus(OBSERVATION_WINDOW));
        return Some(HoldReason::LxcFlappingIn5Min {
            transitions: record.recent_starts.len() as u32,
            window_start,
        });
    }

    // Journal path. Count case-insensitive matches against the two
    // filter substrings.
    if let Some(tail) = &observation.lxc_journal_tail {
        let mut count: u32 = 0;
        for line in tail.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.contains("failed to start") || lower.contains("exited with status") {
                count += 1;
            }
        }
        if count > LXC_JOURNAL_FAILURE_THRESHOLD {
            return Some(HoldReason::LxcJournalFailuresIn5Min {
                count,
                window_start: now.minus(OBSERVATION_WINDOW),
            });
        }
    }

    None
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContainerMount, ContainerPort, ContainerState, RestartPolicy};
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ── Fixtures ──────────────────────────────────────────────────

    fn mk_docker(state: ContainerState, exit_code: Option<i32>, restart_count: u32) -> Container {
        Container {
            id: "id-docker-1".into(),
            name: "sabnzbd".into(),
            runtime: RuntimeKind::Docker,
            host: "charlie".into(),
            state,
            restart_policy: RestartPolicy::UnlessStopped,
            image: Some("img".into()),
            labels: Vec::new(),
            mounts: Vec::<ContainerMount>::new(),
            ports: Vec::<ContainerPort>::new(),
            started_at: None,
            finished_at: None,
            restart_count,
            exit_code,
            startup: None,
        }
    }

    fn mk_lxc(state: ContainerState) -> Container {
        Container {
            id: "200".into(),
            name: "jellyfin".into(),
            runtime: RuntimeKind::Lxc,
            host: "media-a".into(),
            state,
            restart_policy: RestartPolicy::Always,
            image: None,
            labels: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            exit_code: None,
            startup: None,
        }
    }

    fn now() -> Timestamp {
        // Deterministic anchor; wall-clock `now()` is non-deterministic
        // and we want repeatable window math.
        Timestamp::parse_rfc3339("2026-06-12T18:30:00Z").expect("rfc3339")
    }

    // ── Trip path 1: docker RestartStormIn5Min ───────────────────

    #[test]
    fn classify_docker_restart_storm_trips() {
        let now = now();
        let mut record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        // Simulate 4 starts inside the window (threshold = 3, so 4 trips).
        record.recent_starts = vec![
            now.minus(std::time::Duration::from_secs(120)),
            now.minus(std::time::Duration::from_secs(90)),
            now.minus(std::time::Duration::from_secs(60)),
            now.minus(std::time::Duration::from_secs(30)),
        ];
        let container = mk_docker(ContainerState::Exited, Some(0), 4);
        let obs = HostObservation::default();
        let reason = classify(&record, &container, &obs, now).expect("should trip");
        match reason {
            HoldReason::RestartStormIn5Min { count, .. } => assert_eq!(count, 4),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn classify_docker_under_threshold_no_trip() {
        let now = now();
        let mut record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        // 3 starts == threshold; > is the trip condition, so 3 must not.
        record.recent_starts = vec![
            now.minus(std::time::Duration::from_secs(120)),
            now.minus(std::time::Duration::from_secs(60)),
            now.minus(std::time::Duration::from_secs(10)),
        ];
        let container = mk_docker(ContainerState::Running, None, 3);
        let obs = HostObservation::default();
        assert!(classify(&record, &container, &obs, now).is_none());
    }

    // ── Trip path 2: docker FastReexitAfterOrcaStart ─────────────

    #[test]
    fn classify_docker_fast_reexit_trips() {
        let now = now();
        let mut record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        record.last_orca_start_at = Some(now.minus(std::time::Duration::from_secs(15)));
        let container = mk_docker(ContainerState::Exited, Some(137), 1);
        let obs = HostObservation::default();
        let reason = classify(&record, &container, &obs, now).expect("should trip");
        match reason {
            HoldReason::FastReexitAfterOrcaStart {
                within_secs,
                exit_code,
            } => {
                assert_eq!(exit_code, 137);
                assert!(within_secs <= 60);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn classify_docker_reexit_outside_window_no_trip() {
        let now = now();
        let mut record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        // 90s ago — well outside the 60s fast-reexit window.
        record.last_orca_start_at = Some(now.minus(std::time::Duration::from_secs(90)));
        let container = mk_docker(ContainerState::Exited, Some(137), 1);
        let obs = HostObservation::default();
        assert!(classify(&record, &container, &obs, now).is_none());
    }

    #[test]
    fn classify_docker_clean_exit_after_start_no_trip() {
        let now = now();
        let mut record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        record.last_orca_start_at = Some(now.minus(std::time::Duration::from_secs(10)));
        let container = mk_docker(ContainerState::Exited, Some(0), 1);
        let obs = HostObservation::default();
        assert!(classify(&record, &container, &obs, now).is_none());
    }

    // ── Trip path 3: LxcFlappingIn5Min ───────────────────────────

    #[test]
    fn classify_lxc_flapping_trips() {
        let now = now();
        let mut record = BreakerRecord::fresh("media-a", RuntimeKind::Lxc, "200");
        record.recent_starts = vec![
            now.minus(std::time::Duration::from_secs(240)),
            now.minus(std::time::Duration::from_secs(180)),
            now.minus(std::time::Duration::from_secs(120)),
            now.minus(std::time::Duration::from_secs(60)),
        ];
        let container = mk_lxc(ContainerState::Running);
        let obs = HostObservation::default();
        let reason = classify(&record, &container, &obs, now).expect("should trip");
        match reason {
            HoldReason::LxcFlappingIn5Min { transitions, .. } => assert_eq!(transitions, 4),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── Trip path 4: LxcJournalFailuresIn5Min ────────────────────

    #[test]
    fn classify_lxc_journal_failures_trips() {
        let now = now();
        let record = BreakerRecord::fresh("media-a", RuntimeKind::Lxc, "200");
        let tail = "\
Jun 12 18:25 media-a systemd[1]: Failed to start LXC Container: 200.
Jun 12 18:26 media-a systemd[1]: Failed to start LXC Container: 200.
Jun 12 18:27 media-a pve-container[1234]: command exited with status 1.
Jun 12 18:28 media-a systemd[1]: pve-container@200.service: Main process EXITED WITH STATUS 137
unrelated line
";
        let obs = HostObservation {
            lxc_journal_tail: Some(tail.to_string()),
            lxc_previous_state: None,
        };
        let container = mk_lxc(ContainerState::Exited);
        let reason = classify(&record, &container, &obs, now).expect("should trip");
        match reason {
            HoldReason::LxcJournalFailuresIn5Min { count, .. } => assert_eq!(count, 4),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn classify_lxc_journal_under_threshold_no_trip() {
        let record = BreakerRecord::fresh("media-a", RuntimeKind::Lxc, "200");
        // Threshold is > 3; exactly 3 failure lines must not trip.
        let tail = "\
Failed to start unit
exited with status 1
failed to start again
unrelated chatter
";
        let obs = HostObservation {
            lxc_journal_tail: Some(tail.to_string()),
            lxc_previous_state: None,
        };
        let container = mk_lxc(ContainerState::Exited);
        assert!(classify(&record, &container, &obs, now()).is_none());
    }

    // ── Steady-state no-trip ─────────────────────────────────────

    #[test]
    fn classify_no_signals_no_trip() {
        let record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        let container = mk_docker(ContainerState::Running, None, 0);
        let obs = HostObservation::default();
        assert!(classify(&record, &container, &obs, now()).is_none());
    }

    // ── Window pruning ───────────────────────────────────────────

    #[test]
    fn prune_window_drops_entries_older_than_5_min() {
        let now = now();
        let mut record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        record.recent_starts = vec![
            now.minus(std::time::Duration::from_secs(600)), // out
            now.minus(std::time::Duration::from_secs(400)), // out
            now.minus(std::time::Duration::from_secs(200)), // in
            now.minus(std::time::Duration::from_secs(10)),  // in
        ];
        record.prune_window(now);
        assert_eq!(record.recent_starts.len(), 2);
    }

    // ── arm() integration paths ─────────────────────────────────

    #[test]
    fn arm_first_contact_proceeds_and_persists_baseline() {
        let store = MemoryStore::new();
        let container = mk_docker(ContainerState::Created, None, 0);
        let obs = HostObservation::default();
        let decision = arm(ArmRequest {
            container: &container,
            observation: &obs,
            now: now(),
            store: &store,
            initiating_start: true,
        })
        .expect("arm ok");
        assert_eq!(decision, BreakerDecision::Proceed);
        let r = store
            .load("charlie", RuntimeKind::Docker, "id-docker-1")
            .expect("load")
            .expect("present");
        assert_eq!(r.status, BreakerStatus::Watching);
        assert_eq!(r.restart_count_snapshot, Some(0));
        assert_eq!(r.recent_starts.len(), 1);
    }

    #[test]
    fn arm_docker_restart_storm_holds() {
        let store = MemoryStore::new();
        let obs = HostObservation::default();
        let mut t = now();
        // Seed baseline at restart_count=0.
        arm(ArmRequest {
            container: &mk_docker(ContainerState::Created, None, 0),
            observation: &obs,
            now: t,
            store: &store,
            initiating_start: true,
        })
        .expect("seed");

        // Docker churns: each subsequent tick observes restart_count
        // jump by 1 — but the threshold is > 3 so it takes 4 deltas
        // (delta 4 trips because the seed start also lives in
        // recent_starts).
        for delta in 1..=4u32 {
            t = t.plus(std::time::Duration::from_secs(30));
            let _ = arm(ArmRequest {
                container: &mk_docker(ContainerState::Running, None, delta),
                observation: &obs,
                now: t,
                store: &store,
                initiating_start: true,
            })
            .expect("tick");
        }

        let r = store
            .load("charlie", RuntimeKind::Docker, "id-docker-1")
            .expect("load")
            .expect("present");
        assert_eq!(r.status, BreakerStatus::Held);
        assert!(matches!(
            r.held_reason,
            Some(HoldReason::RestartStormIn5Min { .. })
        ));
    }

    #[test]
    fn arm_held_is_sticky_across_ticks() {
        let store = MemoryStore::new();
        let obs = HostObservation::default();
        // Manually seed a Held record.
        let mut seed = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        seed.status = BreakerStatus::Held;
        seed.held_reason = Some(HoldReason::FastReexitAfterOrcaStart {
            within_secs: 12,
            exit_code: 137,
        });
        seed.held_since = Some(now());
        store.save(&seed).expect("seed save");

        let container = mk_docker(ContainerState::Created, None, 0);
        let decision = arm(ArmRequest {
            container: &container,
            observation: &obs,
            now: now().plus(std::time::Duration::from_secs(30)),
            store: &store,
            initiating_start: true,
        })
        .expect("arm");
        assert!(matches!(decision, BreakerDecision::Hold { .. }));
    }

    #[test]
    fn unhold_clears_held_record_and_rearms_watching() {
        let store = MemoryStore::new();
        let mut seed = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        seed.status = BreakerStatus::Held;
        seed.held_reason = Some(HoldReason::RestartStormIn5Min {
            count: 5,
            window_start: now(),
        });
        seed.held_since = Some(now());
        seed.notified_at = Some(now());
        seed.recent_starts = vec![now(), now()];
        store.save(&seed).expect("save");

        let cleared =
            unhold(&store, "charlie", RuntimeKind::Docker, "id-docker-1").expect("unhold ok");
        assert_eq!(cleared.status, BreakerStatus::Watching);
        assert!(cleared.held_reason.is_none());
        assert!(cleared.held_since.is_none());
        assert!(cleared.notified_at.is_none());
        assert!(cleared.recent_starts.is_empty());
    }

    #[test]
    fn unhold_rejects_when_not_held() {
        let store = MemoryStore::new();
        let seed = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        store.save(&seed).expect("save");
        let err =
            unhold(&store, "charlie", RuntimeKind::Docker, "id-docker-1").expect_err("must error");
        assert!(matches!(err, BreakerError::NotHeld { .. }));
    }

    #[test]
    fn unhold_rejects_when_missing() {
        let store = MemoryStore::new();
        let err = unhold(&store, "charlie", RuntimeKind::Docker, "nope").expect_err("must error");
        assert!(matches!(err, BreakerError::NotFound { .. }));
    }

    #[test]
    fn mark_notified_idempotent() {
        let store = MemoryStore::new();
        let seed = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        store.save(&seed).expect("save");
        mark_notified(&store, "charlie", RuntimeKind::Docker, "id-docker-1", now()).expect("mark");
        mark_notified(
            &store,
            "charlie",
            RuntimeKind::Docker,
            "id-docker-1",
            now().plus(std::time::Duration::from_secs(1)),
        )
        .expect("mark again");
        let r = store
            .load("charlie", RuntimeKind::Docker, "id-docker-1")
            .expect("load")
            .expect("present");
        assert!(r.notified_at.is_some());
    }

    // ── Persistence: FileStore round-trip ───────────────────────

    #[test]
    fn file_store_persists_across_instances() {
        let tmp = TempDir::new().expect("tempdir");
        let path: PathBuf = tmp.path().to_path_buf();
        // Instance A: arm a container, trip the breaker via direct save.
        let a = FileStore::new(path.clone());
        let mut record = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "id-docker-1");
        record.status = BreakerStatus::Held;
        record.held_reason = Some(HoldReason::FastReexitAfterOrcaStart {
            within_secs: 5,
            exit_code: 1,
        });
        record.held_since = Some(now());
        a.save(&record).expect("save");
        drop(a);

        // Instance B: load the same path, expect the held record.
        let b = FileStore::new(path);
        let loaded = b
            .load("charlie", RuntimeKind::Docker, "id-docker-1")
            .expect("load")
            .expect("present");
        assert_eq!(loaded.status, BreakerStatus::Held);
        assert_eq!(loaded.held_reason, record.held_reason);

        // arm() against the persisted Held record short-circuits to Hold.
        let container = mk_docker(ContainerState::Created, None, 0);
        let obs = HostObservation::default();
        let decision = arm(ArmRequest {
            container: &container,
            observation: &obs,
            now: now().plus(std::time::Duration::from_secs(120)),
            store: &b,
            initiating_start: true,
        })
        .expect("arm");
        assert!(matches!(decision, BreakerDecision::Hold { .. }));
    }

    // ── Cross-tick last_observed_state ───────────────────────────

    #[test]
    fn arm_persists_current_state_for_next_tick() {
        let store = MemoryStore::new();
        let container = mk_lxc(ContainerState::Running);
        let obs = HostObservation::default();
        let _ = arm(ArmRequest {
            container: &container,
            observation: &obs,
            now: now(),
            store: &store,
            initiating_start: true,
        })
        .expect("arm");

        let persisted = store
            .load(&container.host, container.runtime, &container.id)
            .expect("load")
            .expect("record present");
        assert_eq!(persisted.last_observed_state, Some(ContainerState::Running));
    }

    #[test]
    fn arm_injects_persisted_prev_state_into_lxc_fold() {
        // Compare two stores side-by-side at the same tick: one with
        // a persisted prev=Exited (should trip the fold's
        // stopped→running detection), one fresh (no prev, no
        // transition detected). The fresh-store row isolates arm()'s
        // unconditional Proceed-time push from the fold's overlay-
        // driven push, so the difference between the two row sizes
        // proves the overlay was applied.
        let running = mk_lxc(ContainerState::Running);

        let fresh_store = MemoryStore::new();
        let _ = arm(ArmRequest {
            container: &running,
            observation: &HostObservation::default(),
            now: now(),
            store: &fresh_store,
            initiating_start: true,
        })
        .expect("fresh arm");

        let primed_store = MemoryStore::new();
        let mut seed = BreakerRecord::fresh(&running.host, running.runtime, &running.id);
        seed.last_observed_state = Some(ContainerState::Exited);
        primed_store.save(&seed).expect("seed");
        let _ = arm(ArmRequest {
            container: &running,
            observation: &HostObservation::default(),
            now: now(),
            store: &primed_store,
            initiating_start: true,
        })
        .expect("primed arm");

        let fresh = fresh_store
            .load(&running.host, running.runtime, &running.id)
            .expect("load")
            .expect("fresh record")
            .recent_starts
            .len();
        let primed = primed_store
            .load(&running.host, running.runtime, &running.id)
            .expect("load")
            .expect("primed record")
            .recent_starts
            .len();
        assert_eq!(
            primed,
            fresh + 1,
            "overlayed prev_state=Exited should produce exactly one extra fold push (fresh={fresh}, primed={primed})"
        );
    }

    #[test]
    fn arm_respects_caller_supplied_prev_state_when_set() {
        // Pre-seed the persisted record with prev=Running so an
        // overlay would produce running→running (no transition). The
        // caller forces prev=Exited; this must drive the fold even
        // though the persisted state disagrees. We sanity-check by
        // running the same scenario without the override and asserting
        // the fold did *not* fire.
        let running = mk_lxc(ContainerState::Running);
        let seed_with = || {
            let s = MemoryStore::new();
            let mut r = BreakerRecord::fresh(&running.host, running.runtime, &running.id);
            r.last_observed_state = Some(ContainerState::Running);
            s.save(&r).expect("seed");
            s
        };

        let no_override = seed_with();
        let _ = arm(ArmRequest {
            container: &running,
            observation: &HostObservation::default(),
            now: now(),
            store: &no_override,
            initiating_start: true,
        })
        .expect("no override arm");

        let with_override = seed_with();
        let obs = HostObservation {
            lxc_journal_tail: None,
            lxc_previous_state: Some(ContainerState::Exited),
        };
        let _ = arm(ArmRequest {
            container: &running,
            observation: &obs,
            now: now(),
            store: &with_override,
            initiating_start: true,
        })
        .expect("override arm");

        let no_count = no_override
            .load(&running.host, running.runtime, &running.id)
            .expect("load")
            .expect("record")
            .recent_starts
            .len();
        let with_count = with_override
            .load(&running.host, running.runtime, &running.id)
            .expect("load")
            .expect("record")
            .recent_starts
            .len();
        assert_eq!(
            with_count,
            no_count + 1,
            "caller's prev_state=Exited should produce exactly one extra fold push over the no-override baseline (no_override={no_count}, with_override={with_count})"
        );
    }

    #[test]
    fn file_store_list_returns_all_records() {
        let tmp = TempDir::new().expect("tempdir");
        let store = FileStore::new(tmp.path().to_path_buf());
        store
            .save(&BreakerRecord::fresh("charlie", RuntimeKind::Docker, "a"))
            .expect("a");
        store
            .save(&BreakerRecord::fresh("charlie", RuntimeKind::Docker, "b"))
            .expect("b");
        store
            .save(&BreakerRecord::fresh("media-a", RuntimeKind::Lxc, "200"))
            .expect("c");
        let mut all = store.list().expect("list");
        all.sort_by(|x, y| x.container_id.cmp(&y.container_id));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].container_id, "200");
        assert_eq!(all[1].container_id, "a");
        assert_eq!(all[2].container_id, "b");
    }

    // ── retain_active GC ─────────────────────────────────────────

    fn live_set(
        keys: &[(&str, RuntimeKind, &str)],
    ) -> std::collections::HashSet<(String, RuntimeKind, String)> {
        keys.iter()
            .map(|(h, rt, id)| (h.to_string(), *rt, id.to_string()))
            .collect()
    }

    #[test]
    fn retain_active_evicts_absent_watching_record() {
        let store = MemoryStore::new();
        // A plain observation record (Watching) for a container that is no
        // longer live.
        store
            .save(&BreakerRecord::fresh(
                "charlie",
                RuntimeKind::Docker,
                "gone",
            ))
            .expect("seed");
        // A live container's record.
        store
            .save(&BreakerRecord::fresh(
                "charlie",
                RuntimeKind::Docker,
                "alive",
            ))
            .expect("seed");

        let live = live_set(&[("charlie", RuntimeKind::Docker, "alive")]);
        store.retain_active(&live).expect("retain");

        let ids: std::collections::HashSet<String> = store
            .list()
            .expect("list")
            .into_iter()
            .map(|r| r.container_id)
            .collect();
        assert!(ids.contains("alive"), "live record must survive");
        assert!(!ids.contains("gone"), "absent watching record must evict");
    }

    #[test]
    fn retain_active_preserves_held_record_even_when_absent() {
        let store = MemoryStore::new();
        let mut held = BreakerRecord::fresh("charlie", RuntimeKind::Docker, "held-gone");
        held.status = BreakerStatus::Held;
        held.held_reason = Some(HoldReason::FastReexitAfterOrcaStart {
            within_secs: 5,
            exit_code: 1,
        });
        held.held_since = Some(now());
        store.save(&held).expect("seed");

        // Container absent from the live set entirely.
        let live = live_set(&[]);
        store.retain_active(&live).expect("retain");

        let all = store.list().expect("list");
        assert_eq!(all.len(), 1, "operator-actionable Held record must survive");
        assert_eq!(all[0].container_id, "held-gone");
        assert_eq!(all[0].status, BreakerStatus::Held);
    }

    #[test]
    fn retain_active_persists_eviction_in_file_store() {
        let tmp = TempDir::new().expect("tempdir");
        let path: PathBuf = tmp.path().to_path_buf();
        {
            let store = FileStore::new(path.clone());
            store
                .save(&BreakerRecord::fresh(
                    "charlie",
                    RuntimeKind::Docker,
                    "gone",
                ))
                .expect("seed");
            store.retain_active(&live_set(&[])).expect("retain");
        }
        // Re-open: the eviction must have been flushed.
        let reopened = FileStore::new(path);
        assert!(reopened.list().expect("list").is_empty());
    }
}
