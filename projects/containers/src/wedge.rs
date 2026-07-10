//! Wedged-container detection + auto-recovery state machine.
//!
//! ## Problem
//!
//! `ContainerState::Running` only says "the runtime thinks it's up." When
//! PID 1 inside the container hangs (the originating incident: Jellyfin LXC
//! 113 with a runaway `ffprobe`), `pct status` still reports `running`, the
//! reconciler is happy, and nothing pages — but the service is dead. The
//! [`crate::Liveness`] enum produced by [`crate::RuntimeAdapter::probe_liveness`]
//! exposes the distinction; this module owns the cross-tick state that
//! turns "we saw it wedged once" into "we have permission to act."
//!
//! ## State machine
//!
//! Per `(host, runtime, container_id)`:
//!
//! 1. First [`crate::Liveness::Wedged`] observation → upsert a record,
//!    emit `containers.wedged_detected` once.
//! 2. After `WEDGE_ARM_THRESHOLD` consecutive `Wedged` observations →
//!    attempt recovery via the adapter's [`crate::WedgeRecoverer`].
//!    Backoff between attempts is [`MIN_BACKOFF_BETWEEN_ATTEMPTS_SECS`].
//!    Each attempt increments `recovery_attempts`.
//! 3. After [`MAX_RECOVERY_ATTEMPTS`] failures → escalate to
//!    `containers.wedged_unrecoverable` (ERROR severity). Stop trying.
//! 4. Label `orca.unwedge=manual` short-circuits step 2 — the record
//!    still tracks the wedge but auto-recovery is skipped and the
//!    escalation event fires at the arm threshold for operator paging.
//! 5. Any [`crate::Liveness::Live`] observation deletes the record; if
//!    recovery attempts were made, `containers.wedge_recovered` fires
//!    with the total wedged duration.
//!
//! ## Single code path
//!
//! [`attempt_unwedge`] is the one place recovery runs. The
//! `containers.unwedge` tool body wraps it for manual operator
//! triggering; the reconciler calls it directly for auto-recovery.
//! Same handler, three skins (CLI, REST, MCP all dispatch to the tool;
//! reconciler shares the underlying free fn). Mirrors the
//! `containers.unhold` shape — see [[feedback-cli-api-mcp-one-path]].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use utils::time::Timestamp;

use crate::{AdapterError, Container, Liveness, RuntimeAdapter, RuntimeKind};

// ── Tunables ──────────────────────────────────────────────────────────────

/// Consecutive `Wedged` observations required before the reconciler
/// attempts auto-recovery. One blip won't act; two in a row will.
pub const WEDGE_ARM_THRESHOLD: u32 = 2;

/// Maximum recovery attempts before escalating to
/// `containers.wedged_unrecoverable` and stopping.
pub const MAX_RECOVERY_ATTEMPTS: u32 = 3;

/// Minimum wall-clock seconds between recovery attempts on the same
/// container. Prevents the reconciler from hammering a wedged
/// container on every tick.
pub const MIN_BACKOFF_BETWEEN_ATTEMPTS_SECS: i64 = 60;

// ── Record ────────────────────────────────────────────────────────────────

/// Outcome of one recovery attempt. `Failed` carries the message
/// surface stamped into `containers.wedge_recovery_failed`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryOutcome {
    /// Adapter returned Ok AND post-probe was `Live`.
    Succeeded,
    /// Adapter returned Err, OR Ok but post-probe was still `Wedged`.
    Failed { error: String },
}

/// Per-container wedge state. Persisted by [`WedgeStore`] across
/// reconciler ticks (and across daemon restarts when the file-backed
/// store is in use).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WedgeRecord {
    pub host: String,
    pub runtime: RuntimeKind,
    pub container_id: String,

    /// Run of `Wedged` observations including the most recent. Resets
    /// on any non-`Wedged` observation.
    pub consecutive_wedged_ticks: u32,

    /// First time we saw this container wedged in the current streak.
    pub first_wedged_at: Option<Timestamp>,

    pub recovery_attempts: u32,
    pub last_attempt_at: Option<Timestamp>,
    pub last_attempt_outcome: Option<RecoveryOutcome>,

    /// True once we've emitted `containers.wedged_unrecoverable` for
    /// this streak. Prevents repeated paging.
    pub escalated: bool,

    /// True once we've emitted `containers.wedged_detected` for this
    /// streak. Prevents flooding the dispatcher every tick.
    pub notified_detected: bool,
}

impl WedgeRecord {
    /// Fresh record for the first wedged tick of a streak. Timestamps
    /// the streak start with `now`.
    pub fn new(host: &str, runtime: RuntimeKind, container_id: &str, now: Timestamp) -> Self {
        Self {
            host: host.to_string(),
            runtime,
            container_id: container_id.to_string(),
            consecutive_wedged_ticks: 1,
            first_wedged_at: Some(now),
            recovery_attempts: 0,
            last_attempt_at: None,
            last_attempt_outcome: None,
            escalated: false,
            notified_detected: false,
        }
    }

    /// Whether enough time has passed since the last attempt that we're
    /// allowed to try again. `None` for `last_attempt_at` means we've
    /// never tried, so always Ok.
    pub fn backoff_satisfied(&self, now: Timestamp) -> bool {
        match self.last_attempt_at {
            None => true,
            Some(t) => (now.unix_seconds() - t.unix_seconds()) >= MIN_BACKOFF_BETWEEN_ATTEMPTS_SECS,
        }
    }

    /// Duration the container has been wedged in the current streak,
    /// in fractional seconds. Used in the `recovered` / `succeeded`
    /// event payloads.
    pub fn wedged_duration_secs(&self, now: Timestamp) -> f64 {
        match self.first_wedged_at {
            Some(t) => (now.unix_millis() - t.unix_millis()) as f64 / 1_000.0,
            None => 0.0,
        }
    }
}

// ── Storage trait ─────────────────────────────────────────────────────────

/// Persistence boundary. Tests inject [`MemoryStore`]; production uses
/// [`FileStore`]. Mirrors [`crate::breaker::BreakerStore`] shape.
pub trait WedgeStore: Send + Sync {
    fn load(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<Option<WedgeRecord>, WedgeError>;

    fn save(&self, record: &WedgeRecord) -> Result<(), WedgeError>;

    fn delete(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<(), WedgeError>;

    /// Return every record currently in the store. Used by future
    /// `containers.wedged` listing tool.
    fn list(&self) -> Result<Vec<WedgeRecord>, WedgeError>;

    /// Garbage-collect records whose container is no longer live, bounding
    /// the store's cardinality by the live fleet.
    ///
    /// `live_keys` is the set of `(host, runtime, container_id)` the
    /// reconciler observed this pass. A record is evicted only when its key
    /// is absent from `live_keys` AND it is not operator-actionable.
    ///
    /// An `escalated` record is the `containers.wedged_unrecoverable`
    /// (page-an-operator) state — it must survive even after the container
    /// disappears so the unresolved escalation is not silently lost.
    /// Non-escalated records are in-flight observation / recovery state and
    /// are safe to drop once the container is gone (the live-observation
    /// path already deletes them on recovery).
    fn retain_active(
        &self,
        live_keys: &std::collections::HashSet<(String, RuntimeKind, String)>,
    ) -> Result<(), WedgeError>;
}

/// Whether a wedge record must survive eviction regardless of whether its
/// container is still live. An `escalated` record corresponds to an
/// unresolved `containers.wedged_unrecoverable` page; dropping it because the
/// container vanished would erase an escalation the operator never cleared.
fn wedge_record_is_actionable(record: &WedgeRecord) -> bool {
    record.escalated
}

/// Errors the wedge surface returns. Closed.
#[derive(Debug, thiserror::Error)]
pub enum WedgeError {
    #[error("wedge store I/O: {0}")]
    Io(String),
    #[error("wedge store decode: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RecordKey {
    host: String,
    runtime: RuntimeKind,
    container_id: String,
}

impl RecordKey {
    fn from_record(r: &WedgeRecord) -> Self {
        Self {
            host: r.host.clone(),
            runtime: r.runtime,
            container_id: r.container_id.clone(),
        }
    }

    fn make(host: &str, runtime: RuntimeKind, container_id: &str) -> Self {
        Self {
            host: host.to_string(),
            runtime,
            container_id: container_id.to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct FileLayout {
    records: Vec<WedgeRecord>,
}

// ── File-backed store ─────────────────────────────────────────────────────

/// JSON-file store at `<root>/wedge_state.json`. Atomic writes via
/// tmpfile + rename, identical pattern to
/// [`crate::breaker::FileStore`].
pub struct FileStore {
    path: PathBuf,
    inner: Mutex<HashMap<RecordKey, WedgeRecord>>,
    loaded: Mutex<bool>,
}

impl FileStore {
    /// Construct a store rooted at `dir`. Directory is created on first
    /// save; load is lazy.
    pub fn new(dir: PathBuf) -> Self {
        let path = dir.join("wedge_state.json");
        Self {
            path,
            inner: Mutex::new(HashMap::new()),
            loaded: Mutex::new(false),
        }
    }

    /// Default location: `<orca_home>/containers/wedge_state.json`.
    /// Same resolution as the breaker store so wedge + breaker state
    /// land in the same directory.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("ORCA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".orca")))?;
        Some(home.join("containers"))
    }

    fn ensure_loaded(
        &self,
        guard: &mut MutexGuard<'_, HashMap<RecordKey, WedgeRecord>>,
    ) -> Result<(), WedgeError> {
        let mut loaded_g = self
            .loaded
            .lock()
            .map_err(|e| WedgeError::Io(format!("loaded mutex poisoned: {e}")))?;
        if *loaded_g {
            return Ok(());
        }
        if self.path.exists() {
            let bytes = fs::read(&self.path)
                .map_err(|e| WedgeError::Io(format!("read {}: {e}", self.path.display())))?;
            if !bytes.is_empty() {
                let layout: FileLayout = serde_json::from_slice(&bytes)
                    .map_err(|e| WedgeError::Decode(format!("{}: {e}", self.path.display())))?;
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
        guard: &MutexGuard<'_, HashMap<RecordKey, WedgeRecord>>,
    ) -> Result<(), WedgeError> {
        let layout = FileLayout {
            records: guard.values().cloned().collect(),
        };
        let body = serde_json::to_vec_pretty(&layout)
            .map_err(|e| WedgeError::Decode(format!("encode: {e}")))?;
        utils::atomic::write_mkdir(&self.path, &body)
            .map_err(|e| WedgeError::Io(format!("persist {}: {e}", self.path.display())))?;
        Ok(())
    }
}

impl WedgeStore for FileStore {
    fn load(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<Option<WedgeRecord>, WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        Ok(guard
            .get(&RecordKey::make(host, runtime, container_id))
            .cloned())
    }

    fn save(&self, record: &WedgeRecord) -> Result<(), WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        guard.insert(RecordKey::from_record(record), record.clone());
        self.flush(&guard)
    }

    fn delete(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<(), WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        if guard
            .remove(&RecordKey::make(host, runtime, container_id))
            .is_some()
        {
            self.flush(&guard)?;
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<WedgeRecord>, WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        Ok(guard.values().cloned().collect())
    }

    fn retain_active(
        &self,
        live_keys: &std::collections::HashSet<(String, RuntimeKind, String)>,
    ) -> Result<(), WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("inner mutex poisoned: {e}")))?;
        self.ensure_loaded(&mut guard)?;
        let before = guard.len();
        guard.retain(|key, record| {
            let live =
                live_keys.contains(&(key.host.clone(), key.runtime, key.container_id.clone()));
            live || wedge_record_is_actionable(record)
        });
        if guard.len() != before {
            self.flush(&guard)?;
        }
        Ok(())
    }
}

// ── In-memory store ───────────────────────────────────────────────────────

/// Thread-safe in-memory store. Used in tests and as a fallback when
/// no on-disk state dir resolves.
pub struct MemoryStore {
    inner: Mutex<HashMap<RecordKey, WedgeRecord>>,
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

impl WedgeStore for MemoryStore {
    fn load(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<Option<WedgeRecord>, WedgeError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("memory mutex poisoned: {e}")))?;
        Ok(guard
            .get(&RecordKey::make(host, runtime, container_id))
            .cloned())
    }

    fn save(&self, record: &WedgeRecord) -> Result<(), WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("memory mutex poisoned: {e}")))?;
        guard.insert(RecordKey::from_record(record), record.clone());
        Ok(())
    }

    fn delete(
        &self,
        host: &str,
        runtime: RuntimeKind,
        container_id: &str,
    ) -> Result<(), WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("memory mutex poisoned: {e}")))?;
        guard.remove(&RecordKey::make(host, runtime, container_id));
        Ok(())
    }

    fn list(&self) -> Result<Vec<WedgeRecord>, WedgeError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("memory mutex poisoned: {e}")))?;
        Ok(guard.values().cloned().collect())
    }

    fn retain_active(
        &self,
        live_keys: &std::collections::HashSet<(String, RuntimeKind, String)>,
    ) -> Result<(), WedgeError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| WedgeError::Io(format!("memory mutex poisoned: {e}")))?;
        guard.retain(|key, record| {
            let live =
                live_keys.contains(&(key.host.clone(), key.runtime, key.container_id.clone()));
            live || wedge_record_is_actionable(record)
        });
        Ok(())
    }
}

// ── Recovery action ───────────────────────────────────────────────────────

/// Outcome of an unwedge attempt — flat fields for tool output.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwedgeOutcome {
    /// True iff post-attempt liveness probe returned [`Liveness::Live`].
    pub recovered: bool,
    /// Wall-clock time from start of attempt to post-probe result.
    pub attempt_duration_secs: f64,
    /// Liveness observed immediately after the recovery call returned.
    pub post_probe: Liveness,
    /// Error message from the adapter if the recovery call itself
    /// failed. `None` when the call returned Ok (regardless of
    /// `recovered`).
    pub error: Option<String>,
}

/// Attempt to unwedge `container` via `adapter`'s [`crate::WedgeRecoverer`].
///
/// Errors with `AdapterError::Refused` when the adapter doesn't
/// implement [`crate::RuntimeAdapter::wedge_recoverer`]. Otherwise
/// always returns Ok with an outcome — failures of the recovery call
/// itself are reported via `recovered=false` + `error=Some(_)` so the
/// caller (tool body OR reconciler) can render a uniform result.
///
/// This is the single shared handler. The `containers.unwedge` tool
/// body wraps it; the reconciler's auto-recovery loop calls it
/// directly. Same logic, three skins (CLI/REST/MCP) plus the
/// in-process call site.
pub async fn attempt_unwedge(
    adapter: &dyn RuntimeAdapter,
    container: &Container,
) -> Result<UnwedgeOutcome, AdapterError> {
    let recoverer = adapter.wedge_recoverer().ok_or_else(|| {
        AdapterError::Refused(format!(
            "runtime {} does not support unwedge",
            adapter.kind().as_str()
        ))
    })?;

    let started = std::time::Instant::now();
    let call_result = recoverer.attempt_unwedge(container).await;
    let post_probe = adapter.probe_liveness(container).await;
    let elapsed = started.elapsed().as_secs_f64();

    let (recovered, error) = match call_result {
        Ok(()) => (matches!(post_probe, Liveness::Live), None),
        Err(e) => (false, Some(e.to_string())),
    };

    Ok(UnwedgeOutcome {
        recovered,
        attempt_duration_secs: elapsed,
        post_probe,
        error,
    })
}

// ── State-machine events ──────────────────────────────────────────────────

/// Transport-agnostic event the state machine asks the caller to
/// emit. The reconciler maps each variant to a `notifications::Event`;
/// keeping the enum here means `wedge.rs` doesn't depend on the
/// notifications crate and the state machine stays unit-testable in
/// isolation.
#[derive(Debug, Clone, PartialEq)]
pub enum WedgeEvent {
    /// First `Wedged` observation for a streak. Emitted exactly once
    /// per streak via the `notified_detected` guard.
    Detected {
        host: String,
        runtime: RuntimeKind,
        container_id: String,
        container_name: String,
        first_wedged_at: Timestamp,
    },
    /// About to invoke recovery. Emitted right before the
    /// `attempt_unwedge` call so an operator sees we're acting.
    RecoveryAttempted {
        host: String,
        runtime: RuntimeKind,
        container_id: String,
        container_name: String,
        attempt_number: u32,
    },
    /// Recovery call returned Ok AND post-probe was `Live`.
    RecoverySucceeded {
        host: String,
        runtime: RuntimeKind,
        container_id: String,
        container_name: String,
        attempts_taken: u32,
        total_wedged_duration_secs: f64,
    },
    /// Recovery call failed, OR returned Ok but post-probe was still
    /// wedged. `error` carries the adapter message when the call
    /// itself errored; `"post-attempt still wedged"` otherwise.
    RecoveryFailed {
        host: String,
        runtime: RuntimeKind,
        container_id: String,
        container_name: String,
        attempt_number: u32,
        error: String,
    },
    /// All `MAX_RECOVERY_ATTEMPTS` attempts failed (or
    /// `orca.unwedge=manual` is set and the arm threshold was hit).
    /// ERROR severity — this is the page-an-operator event.
    Unrecoverable {
        host: String,
        runtime: RuntimeKind,
        container_id: String,
        container_name: String,
        attempts: u32,
        first_wedged_at: Timestamp,
    },
    /// Container returned to `Live` after at least one recovery
    /// attempt was made. Emitted on the spontaneous-recovery path —
    /// the post-attempt success path uses `RecoverySucceeded`
    /// instead.
    Recovered {
        host: String,
        runtime: RuntimeKind,
        container_id: String,
        container_name: String,
        attempts: u32,
        total_wedged_duration_secs: f64,
    },
}

/// What the state machine wants the caller to do after a liveness
/// observation. Pure result — caller applies via `store.save` /
/// `store.delete` and `attempt_unwedge`.
#[derive(Debug, Clone, PartialEq)]
pub enum NextAction {
    /// Persist `record`, do nothing else this tick.
    Save(WedgeRecord),
    /// Delete the existing record this tick.
    Delete,
    /// Persist the record AND call `attempt_unwedge`. The caller
    /// chains [`process_recovery_outcome`] with the result to derive
    /// the post-attempt record and follow-up events.
    AttemptRecovery(WedgeRecord),
    /// No record exists and nothing needs to change.
    Noop,
}

/// Result of `process_liveness_observation` / `process_recovery_outcome`.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationResult {
    pub next_action: NextAction,
    pub events: Vec<WedgeEvent>,
}

/// True iff `container` carries the opt-out label
/// `orca.unwedge=manual`.
pub fn is_unwedge_manual(container: &Container) -> bool {
    container.has_label("orca.unwedge", "manual")
}

/// Drive the wedge state machine forward by one liveness observation.
///
/// Pure function — no I/O, no notifications. Caller persists the
/// returned record (or deletes per [`NextAction::Delete`]) and emits
/// the `events`. If `next_action == AttemptRecovery`, caller invokes
/// [`attempt_unwedge`] and chains [`process_recovery_outcome`].
///
/// `prior`: existing record from the store, if any.
/// `observed`: the liveness this tick. `NotApplicable` / `Unknown`
///   are treated as "no signal" — the record (if any) is left alone.
/// `has_recoverer`: result of `adapter.wedge_recoverer().is_some()`.
///   When false, recovery is skipped and we escalate at the arm
///   threshold (matches the `orca.unwedge=manual` path).
/// `now`: timestamp source — pass `utils::time::now()` in production.
pub fn process_liveness_observation(
    prior: Option<WedgeRecord>,
    observed: Liveness,
    container: &Container,
    has_recoverer: bool,
    now: Timestamp,
) -> ObservationResult {
    match observed {
        Liveness::Live => observation_live(prior, container, now),
        Liveness::Wedged => observation_wedged(prior, container, has_recoverer, now),
        // No signal — leave the record alone if one exists.
        Liveness::NotApplicable | Liveness::Unknown => match prior {
            Some(r) => ObservationResult {
                next_action: NextAction::Save(r),
                events: Vec::new(),
            },
            None => ObservationResult {
                next_action: NextAction::Noop,
                events: Vec::new(),
            },
        },
    }
}

fn observation_live(
    prior: Option<WedgeRecord>,
    container: &Container,
    now: Timestamp,
) -> ObservationResult {
    match prior {
        None => ObservationResult {
            next_action: NextAction::Noop,
            events: Vec::new(),
        },
        Some(r) if r.recovery_attempts == 0 => ObservationResult {
            next_action: NextAction::Delete,
            events: Vec::new(),
        },
        Some(r) => {
            let duration = r.wedged_duration_secs(now);
            ObservationResult {
                next_action: NextAction::Delete,
                events: vec![WedgeEvent::Recovered {
                    host: r.host.clone(),
                    runtime: r.runtime,
                    container_id: r.container_id.clone(),
                    container_name: container.name.clone(),
                    attempts: r.recovery_attempts,
                    total_wedged_duration_secs: duration,
                }],
            }
        }
    }
}

fn observation_wedged(
    prior: Option<WedgeRecord>,
    container: &Container,
    has_recoverer: bool,
    now: Timestamp,
) -> ObservationResult {
    let manual = is_unwedge_manual(container);
    let mut events = Vec::new();

    // Upsert the record. `notified_detected=true` after this block
    // means we emit `Detected` exactly once per streak.
    let mut record = match prior {
        Some(mut r) => {
            r.consecutive_wedged_ticks = r.consecutive_wedged_ticks.saturating_add(1);
            r
        }
        None => WedgeRecord::new(&container.host, container.runtime, &container.id, now),
    };

    if !record.notified_detected {
        events.push(WedgeEvent::Detected {
            host: record.host.clone(),
            runtime: record.runtime,
            container_id: record.container_id.clone(),
            container_name: container.name.clone(),
            first_wedged_at: record.first_wedged_at.unwrap_or(now),
        });
        record.notified_detected = true;
    }

    // Already escalated → nothing more to do (no attempts, no
    // duplicate Unrecoverable).
    if record.escalated {
        return ObservationResult {
            next_action: NextAction::Save(record),
            events,
        };
    }

    let arm_reached = record.consecutive_wedged_ticks >= WEDGE_ARM_THRESHOLD;

    // Manual-only or no recoverer available: escalate directly at the
    // arm threshold. No auto-recovery attempts.
    if arm_reached && (manual || !has_recoverer) {
        record.escalated = true;
        events.push(WedgeEvent::Unrecoverable {
            host: record.host.clone(),
            runtime: record.runtime,
            container_id: record.container_id.clone(),
            container_name: container.name.clone(),
            attempts: record.recovery_attempts,
            first_wedged_at: record.first_wedged_at.unwrap_or(now),
        });
        return ObservationResult {
            next_action: NextAction::Save(record),
            events,
        };
    }

    // Hit the cap: escalate, stop trying.
    if record.recovery_attempts >= MAX_RECOVERY_ATTEMPTS {
        record.escalated = true;
        events.push(WedgeEvent::Unrecoverable {
            host: record.host.clone(),
            runtime: record.runtime,
            container_id: record.container_id.clone(),
            container_name: container.name.clone(),
            attempts: record.recovery_attempts,
            first_wedged_at: record.first_wedged_at.unwrap_or(now),
        });
        return ObservationResult {
            next_action: NextAction::Save(record),
            events,
        };
    }

    // Should we try this tick?
    if arm_reached && record.backoff_satisfied(now) {
        events.push(WedgeEvent::RecoveryAttempted {
            host: record.host.clone(),
            runtime: record.runtime,
            container_id: record.container_id.clone(),
            container_name: container.name.clone(),
            attempt_number: record.recovery_attempts + 1,
        });
        return ObservationResult {
            next_action: NextAction::AttemptRecovery(record),
            events,
        };
    }

    // Wedged but not yet at the arm threshold, or backoff still in
    // effect — just track it.
    ObservationResult {
        next_action: NextAction::Save(record),
        events,
    }
}

/// Fold the outcome of an `attempt_unwedge` call into the record and
/// derive the follow-up events. `record` is the
/// `NextAction::AttemptRecovery(_)` payload from
/// `process_liveness_observation`; `outcome` is the return of
/// `attempt_unwedge`. Caller applies the resulting `NextAction`.
pub fn process_recovery_outcome(
    mut record: WedgeRecord,
    outcome: &UnwedgeOutcome,
    container: &Container,
    now: Timestamp,
) -> ObservationResult {
    record.recovery_attempts = record.recovery_attempts.saturating_add(1);
    record.last_attempt_at = Some(now);

    if outcome.recovered {
        record.last_attempt_outcome = Some(RecoveryOutcome::Succeeded);
        let duration = record.wedged_duration_secs(now);
        return ObservationResult {
            next_action: NextAction::Delete,
            events: vec![WedgeEvent::RecoverySucceeded {
                host: record.host.clone(),
                runtime: record.runtime,
                container_id: record.container_id.clone(),
                container_name: container.name.clone(),
                attempts_taken: record.recovery_attempts,
                total_wedged_duration_secs: duration,
            }],
        };
    }

    // Not recovered — either the call errored or post-probe was still
    // wedged. Stamp Failed with whichever message we have.
    let error = outcome
        .error
        .clone()
        .unwrap_or_else(|| "post-attempt still wedged".to_string());
    record.last_attempt_outcome = Some(RecoveryOutcome::Failed {
        error: error.clone(),
    });

    let mut events = vec![WedgeEvent::RecoveryFailed {
        host: record.host.clone(),
        runtime: record.runtime,
        container_id: record.container_id.clone(),
        container_name: container.name.clone(),
        attempt_number: record.recovery_attempts,
        error,
    }];

    // Did this attempt push us over the cap? Escalate now so the
    // operator gets paged on the same tick.
    if record.recovery_attempts >= MAX_RECOVERY_ATTEMPTS && !record.escalated {
        record.escalated = true;
        events.push(WedgeEvent::Unrecoverable {
            host: record.host.clone(),
            runtime: record.runtime,
            container_id: record.container_id.clone(),
            container_name: container.name.clone(),
            attempts: record.recovery_attempts,
            first_wedged_at: record.first_wedged_at.unwrap_or(now),
        });
    }

    ObservationResult {
        next_action: NextAction::Save(record),
        events,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContainerState, HostObservation, ListFilter, LogTail, RestartPolicy, RuntimeKind,
        WedgeRecoverer,
    };
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    fn sample_container() -> Container {
        Container {
            id: "113".to_string(),
            name: "jellyfin".to_string(),
            runtime: RuntimeKind::Lxc,
            host: "operator-pve-a".to_string(),
            state: ContainerState::Running,
            restart_policy: RestartPolicy::UnlessStopped,
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

    /// Mock adapter whose recovery + post-probe outcomes are scripted.
    struct MockAdapter {
        kind: RuntimeKind,
        recover_result: StdMutex<Result<(), AdapterError>>,
        post_probe: StdMutex<Liveness>,
        recover_calls: StdMutex<u32>,
        recoverer_enabled: bool,
    }

    impl MockAdapter {
        fn new(recover_result: Result<(), AdapterError>, post_probe: Liveness) -> Self {
            Self {
                kind: RuntimeKind::Lxc,
                recover_result: StdMutex::new(recover_result),
                post_probe: StdMutex::new(post_probe),
                recover_calls: StdMutex::new(0),
                recoverer_enabled: true,
            }
        }

        fn without_recoverer() -> Self {
            Self {
                kind: RuntimeKind::Lxc,
                recover_result: StdMutex::new(Ok(())),
                post_probe: StdMutex::new(Liveness::Wedged),
                recover_calls: StdMutex::new(0),
                recoverer_enabled: false,
            }
        }
    }

    #[async_trait]
    impl RuntimeAdapter for MockAdapter {
        fn kind(&self) -> RuntimeKind {
            self.kind
        }

        async fn list(&self, _filter: &ListFilter) -> Result<Vec<Container>, AdapterError> {
            Ok(Vec::new())
        }

        async fn inspect(&self, _id: &str) -> Result<Container, AdapterError> {
            Ok(sample_container())
        }

        async fn start(&self, _id: &str) -> Result<(), AdapterError> {
            Ok(())
        }

        async fn stop(&self, _id: &str) -> Result<(), AdapterError> {
            Ok(())
        }

        async fn restart(&self, _id: &str) -> Result<(), AdapterError> {
            Ok(())
        }

        async fn logs(&self, _id: &str, _tail: LogTail) -> Result<String, AdapterError> {
            Ok(String::new())
        }

        async fn observe(&self, _c: &Container) -> HostObservation {
            HostObservation::default()
        }

        async fn probe_liveness(&self, _c: &Container) -> Liveness {
            *self.post_probe.lock().unwrap()
        }

        fn wedge_recoverer(&self) -> Option<&dyn WedgeRecoverer> {
            if self.recoverer_enabled {
                Some(self)
            } else {
                None
            }
        }
    }

    #[async_trait]
    impl WedgeRecoverer for MockAdapter {
        async fn attempt_unwedge(&self, _c: &Container) -> Result<(), AdapterError> {
            *self.recover_calls.lock().unwrap() += 1;
            self.recover_result
                .lock()
                .unwrap()
                .as_ref()
                .map(|_| ())
                .map_err(|e| match e {
                    AdapterError::Refused(s) => AdapterError::Refused(s.clone()),
                    AdapterError::NotFound(s) => AdapterError::NotFound(s.clone()),
                    AdapterError::Unavailable(s) => AdapterError::Unavailable(s.clone()),
                    AdapterError::Malformed(s) => AdapterError::Malformed(s.clone()),
                    AdapterError::Transport(s) => AdapterError::Transport(s.clone()),
                })
        }
    }

    #[tokio::test]
    async fn attempt_unwedge_succeeds_when_recovery_and_probe_agree() {
        let adapter = MockAdapter::new(Ok(()), Liveness::Live);
        let outcome = attempt_unwedge(&adapter, &sample_container())
            .await
            .unwrap();
        assert!(outcome.recovered);
        assert_eq!(outcome.post_probe, Liveness::Live);
        assert!(outcome.error.is_none());
        assert_eq!(*adapter.recover_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn attempt_unwedge_returns_not_recovered_when_probe_still_wedged() {
        let adapter = MockAdapter::new(Ok(()), Liveness::Wedged);
        let outcome = attempt_unwedge(&adapter, &sample_container())
            .await
            .unwrap();
        assert!(!outcome.recovered);
        assert_eq!(outcome.post_probe, Liveness::Wedged);
        assert!(outcome.error.is_none());
    }

    #[tokio::test]
    async fn attempt_unwedge_reports_adapter_error() {
        let adapter = MockAdapter::new(
            Err(AdapterError::Refused("forceStop timed out".to_string())),
            Liveness::Wedged,
        );
        let outcome = attempt_unwedge(&adapter, &sample_container())
            .await
            .unwrap();
        assert!(!outcome.recovered);
        assert!(outcome.error.as_deref().unwrap().contains("forceStop"));
    }

    #[tokio::test]
    async fn attempt_unwedge_errors_when_adapter_has_no_recoverer() {
        let adapter = MockAdapter::without_recoverer();
        let err = attempt_unwedge(&adapter, &sample_container())
            .await
            .unwrap_err();
        match err {
            AdapterError::Refused(msg) => assert!(msg.contains("does not support unwedge")),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn wedge_record_backoff_respects_min_gap() {
        let now = utils::time::now();
        let mut rec = WedgeRecord::new("h", RuntimeKind::Lxc, "1", now);
        assert!(rec.backoff_satisfied(now));
        rec.last_attempt_at = Some(now);
        assert!(!rec.backoff_satisfied(now.plus(std::time::Duration::from_secs(5))));
        assert!(
            rec.backoff_satisfied(now.plus(std::time::Duration::from_secs(
                (MIN_BACKOFF_BETWEEN_ATTEMPTS_SECS) as u64
            )))
        );
    }

    #[test]
    fn wedge_record_duration_is_now_minus_first_wedged() {
        let now = utils::time::now();
        let mut rec = WedgeRecord::new("h", RuntimeKind::Lxc, "1", now);
        let later = now.plus(std::time::Duration::from_secs(42));
        assert!((rec.wedged_duration_secs(later) - 42.0).abs() < 0.001);
        rec.first_wedged_at = None;
        assert_eq!(rec.wedged_duration_secs(later), 0.0);
    }

    #[test]
    fn memory_store_round_trips_a_record() {
        let store = MemoryStore::new();
        let rec = WedgeRecord::new("h", RuntimeKind::Lxc, "113", utils::time::now());
        store.save(&rec).unwrap();
        let loaded = store.load("h", RuntimeKind::Lxc, "113").unwrap().unwrap();
        assert_eq!(loaded, rec);
        store.delete("h", RuntimeKind::Lxc, "113").unwrap();
        assert!(store.load("h", RuntimeKind::Lxc, "113").unwrap().is_none());
    }

    // ── State machine ────────────────────────────────────────────

    fn manual_container() -> Container {
        let mut c = sample_container();
        c.labels
            .push(("orca.unwedge".to_string(), "manual".to_string()));
        c
    }

    fn assert_record(action: &NextAction) -> &WedgeRecord {
        match action {
            NextAction::Save(r) | NextAction::AttemptRecovery(r) => r,
            other => panic!("expected Save/AttemptRecovery, got {other:?}"),
        }
    }

    #[test]
    fn first_wedged_creates_record_and_detected_event_no_attempt() {
        let now = utils::time::now();
        let c = sample_container();
        let r = process_liveness_observation(None, Liveness::Wedged, &c, true, now);
        assert!(matches!(r.next_action, NextAction::Save(_)));
        assert_eq!(r.events.len(), 1);
        assert!(matches!(r.events[0], WedgeEvent::Detected { .. }));
        let rec = assert_record(&r.next_action);
        assert_eq!(rec.consecutive_wedged_ticks, 1);
        assert!(rec.notified_detected);
    }

    #[test]
    fn second_wedged_tick_emits_recovery_attempted() {
        let now = utils::time::now();
        let c = sample_container();
        let r1 = process_liveness_observation(None, Liveness::Wedged, &c, true, now);
        let rec1 = assert_record(&r1.next_action).clone();
        let r2 = process_liveness_observation(
            Some(rec1),
            Liveness::Wedged,
            &c,
            true,
            now.plus(std::time::Duration::from_secs(1)),
        );
        assert!(matches!(r2.next_action, NextAction::AttemptRecovery(_)));
        assert_eq!(r2.events.len(), 1);
        match &r2.events[0] {
            WedgeEvent::RecoveryAttempted { attempt_number, .. } => assert_eq!(*attempt_number, 1),
            other => panic!("expected RecoveryAttempted, got {other:?}"),
        }
    }

    #[test]
    fn recovery_outcome_success_emits_succeeded_and_deletes() {
        let now = utils::time::now();
        let rec = WedgeRecord::new("h", RuntimeKind::Lxc, "113", now);
        let outcome = UnwedgeOutcome {
            recovered: true,
            attempt_duration_secs: 1.2,
            post_probe: Liveness::Live,
            error: None,
        };
        let r = process_recovery_outcome(
            rec,
            &outcome,
            &sample_container(),
            now.plus(std::time::Duration::from_secs(5)),
        );
        assert_eq!(r.next_action, NextAction::Delete);
        assert_eq!(r.events.len(), 1);
        assert!(matches!(
            r.events[0],
            WedgeEvent::RecoverySucceeded {
                attempts_taken: 1,
                ..
            }
        ));
    }

    #[test]
    fn recovery_outcome_failure_three_times_escalates() {
        let now = utils::time::now();
        let c = sample_container();
        let mut rec = WedgeRecord::new(&c.host, c.runtime, &c.id, now);
        // Mark that we've already armed (two consecutive wedged ticks)
        // so attempt counts grow without going back through the
        // observation path.
        rec.consecutive_wedged_ticks = WEDGE_ARM_THRESHOLD;
        rec.notified_detected = true;

        let outcome = UnwedgeOutcome {
            recovered: false,
            attempt_duration_secs: 0.5,
            post_probe: Liveness::Wedged,
            error: Some("forceStop timed out".to_string()),
        };

        let mut current = rec;
        let mut last_events: Vec<WedgeEvent> = Vec::new();
        for i in 1..=MAX_RECOVERY_ATTEMPTS {
            let r = process_recovery_outcome(
                current,
                &outcome,
                &c,
                now.plus(std::time::Duration::from_secs((i as i64 * 60) as u64)),
            );
            last_events = r.events.clone();
            match r.next_action {
                NextAction::Save(rec) => current = rec,
                other => panic!("expected Save during attempts, got {other:?}"),
            }
        }
        assert!(current.escalated);
        assert_eq!(current.recovery_attempts, MAX_RECOVERY_ATTEMPTS);
        // Last tick should include both Failed and Unrecoverable.
        assert!(
            last_events
                .iter()
                .any(|e| matches!(e, WedgeEvent::RecoveryFailed { .. }))
        );
        assert!(
            last_events
                .iter()
                .any(|e| matches!(e, WedgeEvent::Unrecoverable { .. }))
        );

        // Further wedged observations on an escalated record do nothing
        // beyond persisting — no extra Unrecoverable, no new attempt.
        let later = process_liveness_observation(
            Some(current),
            Liveness::Wedged,
            &c,
            true,
            now.plus(std::time::Duration::from_secs(600)),
        );
        assert!(matches!(later.next_action, NextAction::Save(_)));
        assert!(
            !later
                .events
                .iter()
                .any(|e| matches!(e, WedgeEvent::Unrecoverable { .. }))
        );
    }

    #[test]
    fn manual_label_skips_attempts_and_escalates_at_arm() {
        let now = utils::time::now();
        let c = manual_container();
        let r1 = process_liveness_observation(None, Liveness::Wedged, &c, true, now);
        let rec1 = assert_record(&r1.next_action).clone();
        let r2 = process_liveness_observation(
            Some(rec1),
            Liveness::Wedged,
            &c,
            true,
            now.plus(std::time::Duration::from_secs(1)),
        );
        match r2.next_action {
            NextAction::Save(rec) => assert!(rec.escalated),
            other => panic!("expected Save with escalated=true, got {other:?}"),
        }
        assert!(
            r2.events
                .iter()
                .any(|e| matches!(e, WedgeEvent::Unrecoverable { .. }))
        );
        // No RecoveryAttempted ever.
        assert!(
            !r2.events
                .iter()
                .any(|e| matches!(e, WedgeEvent::RecoveryAttempted { .. }))
        );
    }

    #[test]
    fn live_after_zero_attempts_deletes_silently() {
        let now = utils::time::now();
        let c = sample_container();
        let rec = WedgeRecord::new(&c.host, c.runtime, &c.id, now);
        let r = process_liveness_observation(
            Some(rec),
            Liveness::Live,
            &c,
            true,
            now.plus(std::time::Duration::from_secs(2)),
        );
        assert_eq!(r.next_action, NextAction::Delete);
        assert!(r.events.is_empty());
    }

    #[test]
    fn live_after_attempts_emits_recovered() {
        let now = utils::time::now();
        let c = sample_container();
        let mut rec = WedgeRecord::new(&c.host, c.runtime, &c.id, now);
        rec.recovery_attempts = 1;
        let r = process_liveness_observation(
            Some(rec),
            Liveness::Live,
            &c,
            true,
            now.plus(std::time::Duration::from_secs(30)),
        );
        assert_eq!(r.next_action, NextAction::Delete);
        assert_eq!(r.events.len(), 1);
        match &r.events[0] {
            WedgeEvent::Recovered {
                attempts,
                total_wedged_duration_secs,
                ..
            } => {
                assert_eq!(*attempts, 1);
                assert!((*total_wedged_duration_secs - 30.0).abs() < 0.1);
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
    }

    #[test]
    fn backoff_skips_second_attempt_within_window() {
        let now = utils::time::now();
        let c = sample_container();
        let mut rec = WedgeRecord::new(&c.host, c.runtime, &c.id, now);
        rec.consecutive_wedged_ticks = WEDGE_ARM_THRESHOLD;
        rec.recovery_attempts = 1;
        rec.last_attempt_at = Some(now);
        rec.notified_detected = true;
        // Observation 5s after the last attempt — inside backoff.
        let r = process_liveness_observation(
            Some(rec),
            Liveness::Wedged,
            &c,
            true,
            now.plus(std::time::Duration::from_secs(5)),
        );
        assert!(matches!(r.next_action, NextAction::Save(_)));
        assert!(
            !r.events
                .iter()
                .any(|e| matches!(e, WedgeEvent::RecoveryAttempted { .. }))
        );
    }

    #[test]
    fn detected_only_emits_once_across_repeated_wedged_ticks() {
        let now = utils::time::now();
        let c = sample_container();
        let r1 = process_liveness_observation(None, Liveness::Wedged, &c, false, now);
        assert!(
            r1.events
                .iter()
                .any(|e| matches!(e, WedgeEvent::Detected { .. }))
        );
        let rec = assert_record(&r1.next_action).clone();
        // Second tick: no-recoverer path will escalate (Unrecoverable),
        // but Detected MUST NOT fire again.
        let r2 = process_liveness_observation(
            Some(rec),
            Liveness::Wedged,
            &c,
            false,
            now.plus(std::time::Duration::from_secs(1)),
        );
        assert!(
            !r2.events
                .iter()
                .any(|e| matches!(e, WedgeEvent::Detected { .. }))
        );
    }

    #[test]
    fn unknown_or_not_applicable_leaves_record_intact() {
        let now = utils::time::now();
        let c = sample_container();
        let rec = WedgeRecord::new(&c.host, c.runtime, &c.id, now);
        let r = process_liveness_observation(
            Some(rec.clone()),
            Liveness::Unknown,
            &c,
            true,
            now.plus(std::time::Duration::from_secs(1)),
        );
        match r.next_action {
            NextAction::Save(saved) => assert_eq!(saved, rec),
            other => panic!("expected Save(unchanged), got {other:?}"),
        }
        assert!(r.events.is_empty());
    }

    #[test]
    fn file_store_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        // Anchor to a second-precision timestamp: `Timestamp` serializes at
        // second precision, so a sub-second `now()` would not survive the
        // JSON round-trip. All real persisted timestamps are second-precision.
        let anchor = Timestamp::parse_rfc3339("2026-06-12T18:30:00Z").unwrap();
        let rec = WedgeRecord::new("h", RuntimeKind::Lxc, "113", anchor);
        {
            let s = FileStore::new(dir.path().to_path_buf());
            s.save(&rec).unwrap();
        }
        let s2 = FileStore::new(dir.path().to_path_buf());
        let loaded = s2.load("h", RuntimeKind::Lxc, "113").unwrap().unwrap();
        assert_eq!(loaded, rec);
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
    fn retain_active_evicts_absent_non_escalated_record() {
        let store = MemoryStore::new();
        let now = utils::time::now();
        store
            .save(&WedgeRecord::new("h", RuntimeKind::Lxc, "gone", now))
            .unwrap();
        store
            .save(&WedgeRecord::new("h", RuntimeKind::Lxc, "alive", now))
            .unwrap();

        store
            .retain_active(&live_set(&[("h", RuntimeKind::Lxc, "alive")]))
            .unwrap();

        assert!(
            store
                .load("h", RuntimeKind::Lxc, "alive")
                .unwrap()
                .is_some()
        );
        assert!(store.load("h", RuntimeKind::Lxc, "gone").unwrap().is_none());
    }

    #[test]
    fn retain_active_preserves_escalated_record_even_when_absent() {
        let store = MemoryStore::new();
        let now = utils::time::now();
        let mut rec = WedgeRecord::new("h", RuntimeKind::Lxc, "unrecoverable", now);
        rec.escalated = true;
        store.save(&rec).unwrap();

        store.retain_active(&live_set(&[])).unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 1, "escalated record must survive eviction");
        assert!(all[0].escalated);
    }

    #[test]
    fn retain_active_persists_eviction_in_file_store() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = FileStore::new(dir.path().to_path_buf());
            s.save(&WedgeRecord::new(
                "h",
                RuntimeKind::Lxc,
                "gone",
                utils::time::now(),
            ))
            .unwrap();
            s.retain_active(&live_set(&[])).unwrap();
        }
        let s2 = FileStore::new(dir.path().to_path_buf());
        assert!(s2.list().unwrap().is_empty());
    }
}
