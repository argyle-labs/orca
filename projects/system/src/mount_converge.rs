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

use crate::autofs::{self, PrivilegedOp, run_privileged};
use crate::mount_exec::MountReq;
use crate::remediation::{self, RemediationPolicy};
use crate::source_election::{self, Election};
use crate::{host_identity, mounts, periodic, replication, shares};
use db::notifications_store::{Fix, RaiseInput, Severity};
use plugin_toolkit::route::Route;
use plugin_toolkit::storage::{
    Health, RemountAggression, RemountPolicy, SourceProbe, probe_source, probe_source_nfs,
    resolve_replication_status,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// `source` all convergence dismissable notifications carry, for scoping.
const NOTIFY_SOURCE: &str = "remediation:storage.converge";

/// Seconds between convergence ticks.
pub const INTERVAL_SECS: u64 = 30;
/// Per-target liveness-probe timeout — a live NFS `stat` answers in ms.
pub const PROBE_TIMEOUT_SECS: u64 = 5;
/// Consecutive stale probes before a mounted target is remounted. The blip
/// filter: a single stale probe is usually a briefly-slow server, not a dead
/// one. A *missing* mount is not gated — it is mounted immediately.
pub const CONFIRM_TICKS: u32 = 2;

/// A desired mount for THIS host: a placement joined to its share, with the
/// share's ordered sources and pre-rendered options resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredMount {
    pub target: String,
    /// Backend that owns this share's grammar (`nfs`, `smb`) — used to resolve the
    /// generic secret-file through the owning plugin at mount time.
    pub backend: String,
    pub fstype: String,
    /// Ordered `host:/export` sources for the *enabled* (non-held) routes,
    /// primary first — election picks the first live one at mount time. Derived
    /// from `routes` via [`source_of_route`].
    pub sources: Vec<String>,
    /// The share's full ordered route set (including held/disabled routes), so
    /// [`plan`] can honour drained routes and the failover/fail-back policy.
    pub routes: Vec<Route>,
    /// Typed remount policy governing failover / fail-back / drain / aggression.
    pub remount_policy: RemountPolicy,
    /// Optional replication-relationship ref (uuidv7) the share declared. When
    /// set, the failover-safety gate holds an active-route swap unless the
    /// relationship's observed status is healthy — failing over to a member whose
    /// replication is unconfirmed risks serving stale data. `None` ⇒ failover is
    /// ungated (unchanged pre-gate behaviour).
    pub replication: Option<String>,
    /// The backend-rendered `mount -o` option string (opaque to core).
    pub options: String,
    /// Credential reference (a `SecretRef`) the share declared, if any. Core
    /// resolves it and hands the plaintext to the owning backend, which renders the
    /// root-owned secret-file `contents`; core never parses the grammar. `None` for
    /// NFS and file/guest-SMB.
    pub credential: Option<String>,
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
    let by_id: HashMap<String, shares::EndpointRow> = shares::endpoint_db::list()?
        .into_iter()
        .filter(|s| s.enabled)
        .map(|s| (s.id.clone(), s))
        .collect();

    let mut out = Vec::new();
    for m in mounts::endpoint_db::list()? {
        if !m.enabled || m.host != this_host {
            continue;
        }
        // Guest-targeted placements are applied INSIDE a guest by a
        // GuestMountApplier (see `desired_guest_mounts_for_host`), never mounted
        // on the host filesystem — skip them in the host-mount desired set.
        if m.guest.as_deref().is_some_and(|g| !g.trim().is_empty()) {
            continue;
        }
        let Some(share) = by_id.get(&m.share_id) else {
            continue;
        };
        let routes: Vec<Route> = share.routes.iter().cloned().collect();
        // Enabled (non-held) routes rendered to sources, primary first.
        let sources: Vec<String> = routes
            .iter()
            .filter(|r| r.enabled)
            .map(|r| source_of_route(&share.fstype, r))
            .collect();
        if sources.is_empty() {
            continue;
        }
        // The placement carries the per-host remount policy; the share carries
        // the failover routes. `None` ⇒ the engine's default policy.
        let remount_policy = m.remount_policy.clone().unwrap_or_default();
        out.push(DesiredMount {
            target: m.target,
            backend: share.backend.clone(),
            fstype: share.fstype.clone(),
            sources,
            routes,
            remount_policy,
            replication: share.replication.clone(),
            options: share.options_rendered.clone(),
            credential: share.credential.clone(),
        });
    }
    Ok(out)
}

/// Resolve the guest-targeted mount placements for `this_host`, each joined to its
/// share and rendered to a [`GuestMountSpec`](plugin_toolkit::storage::GuestMountSpec)
/// — the counterpart to [`desired_for_host`] for placements whose `guest` is set.
/// The host's convergence loop hands these to the registered
/// [`GuestMountApplier`](plugin_toolkit::storage::GuestMountApplier) instead of
/// mounting them on the host filesystem.
pub fn desired_guest_mounts_for_host(
    this_host: &str,
) -> anyhow::Result<Vec<plugin_toolkit::storage::GuestMountSpec>> {
    let by_id: HashMap<String, shares::EndpointRow> = shares::endpoint_db::list()?
        .into_iter()
        .filter(|s| s.enabled)
        .map(|s| (s.id.clone(), s))
        .collect();

    let mut out = Vec::new();
    for m in mounts::endpoint_db::list()? {
        let Some(guest) = m.guest.as_deref().map(str::trim).filter(|g| !g.is_empty()) else {
            continue;
        };
        if !m.enabled || m.host != this_host {
            continue;
        }
        let Some(share) = by_id.get(&m.share_id) else {
            continue;
        };
        let sources: Vec<String> = share
            .routes
            .iter()
            .filter(|r| r.enabled)
            .map(|r| source_of_route(&share.fstype, r))
            .collect();
        if sources.is_empty() {
            continue;
        }
        out.push(plugin_toolkit::storage::GuestMountSpec {
            guest: guest.to_string(),
            target: m.target,
            backend: share.backend.clone(),
            fstype: share.fstype.clone(),
            sources,
            options: share.options_rendered.clone(),
            credential: share.credential.clone(),
        });
    }
    Ok(out)
}

/// Render one [`Route`] back to the mount source string its `fstype` expects.
/// The fold-in inverse: an NFS `host:/export` source is `value = host`,
/// `path = "/export"` → `host:/export`; an SMB `//server/share` is
/// `value = server`, `path = "/share"` → `//server/share`. A route with no
/// `path` degrades to the bare `value` (a source that is already whole).
pub fn source_of_route(fstype: &str, route: &Route) -> String {
    match route.path.as_deref() {
        Some(path) if fstype.starts_with("nfs") => format!("{}:{}", route.value, path),
        Some(path) => format!("//{}{}", route.value, path),
        None => route.value.clone(),
    }
}

/// How a desired target's two probe signals classify for planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// Nothing mounted at the target → `plan()` mounts it.
    Missing,
    /// A live, healthy filesystem is mounted through the target → leave it.
    Healthy,
    /// Mounted but stale/hung → `plan()` remounts it (fail forward).
    Stale,
}

/// Classify a desired target from its two probe signals. Pure so the exact
/// false-positive that left placements unmounted is unit-tested.
///
/// The kernel mount table is authoritative for *presence*: when the target is
/// absent from it, the target is Missing even if `probe_health` returned
/// `Health::Ok` — a bare mountpoint dir with nothing mounted through it `stat`s
/// clean and reads as `Ok`, the false positive that made convergence a no-op.
fn classify(absent_from_table: bool, health: Health) -> Presence {
    if absent_from_table {
        return Presence::Missing;
    }
    match health {
        Health::Ok => Presence::Healthy,
        Health::Missing => Presence::Missing,
        Health::Stale | Health::Timeout | Health::Error => Presence::Stale,
    }
}

/// The election + placement signals `plan` needs to decide a **failover /
/// fail-back swap** for a mount that is currently healthy but on the wrong
/// source. Built by the tick from the (confirmed) election result and the kernel
/// mount table; empty for a target with no evaluated election.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FailoverSignal {
    /// The election over the *enabled* (non-held) sources for this target.
    pub elected: Election,
    /// The source the target is currently mounted from, when known.
    pub active: Option<String>,
    /// Whether the mount is actively held (busy). Under
    /// [`RemountAggression::Safe`] a busy mount is never force-swapped.
    pub busy: bool,
    /// Observed replication health for the share's relationship, resolved
    /// host-local by the tick. `Some(true)` = healthy (swap permitted by the
    /// gate), `Some(false)` = unhealthy, `None` = unknown / no provider
    /// registered. Only consulted when the desired mount carries a replication
    /// ref; with one present the gate permits a swap ONLY on `Some(true)`.
    pub replication_healthy: Option<bool>,
}

/// Resolve observed replication health for each desired mount that declares a
/// replication ref, keyed by target. A mount with no ref never appears in the
/// map (the gate only consults present entries; its absence reads as `None`).
/// Health is observed host-local + on-demand via the registered status provider
/// ([[on-demand-not-poll-and-cache]]); with no provider registered — the state
/// core is in until the syncthing plugin lands — every ref resolves to `None`
/// (unknown), which the gate treats as "hold". Resolves are cached per
/// relationship id so N shares sharing one folder probe it once.
async fn replication_health_by_target(desired: &[DesiredMount]) -> HashMap<String, Option<bool>> {
    // Any desired mount actually carrying a ref? Skip the relationship list read
    // entirely otherwise (the common case: no share uses replication yet).
    if !desired.iter().any(|d| d.replication.is_some()) {
        return HashMap::new();
    }
    // Index relationships by their uuidv7 id — the value a share's ref carries.
    let by_id: HashMap<String, replication::EndpointRow> = match replication::endpoint_db::list() {
        Ok(rows) => rows.into_iter().map(|r| (r.id.clone(), r)).collect(),
        Err(e) => {
            warn!("replication: relationship list failed; failover gate holds: {e}");
            HashMap::new()
        }
    };
    let mut resolved: HashMap<String, Option<bool>> = HashMap::new();
    let mut out: HashMap<String, Option<bool>> = HashMap::new();
    for d in desired {
        let Some(rep_id) = d.replication.as_deref() else {
            continue;
        };
        let healthy = if let Some(cached) = resolved.get(rep_id) {
            *cached
        } else {
            let h = match by_id.get(rep_id) {
                Some(rel) => {
                    let members: Vec<String> = rel.routes.iter().map(|r| r.value.clone()).collect();
                    resolve_replication_status(&rel.provider, &rel.folder, &members)
                        .await
                        .map(|s| s.healthy)
                }
                None => {
                    // A dangling ref (relationship deleted out from under the
                    // share) is unknown → hold, and surface it.
                    warn!(
                        "replication: share references unknown relationship `{rep_id}`; failover gate holds"
                    );
                    None
                }
            };
            resolved.insert(rep_id.to_string(), h);
            h
        };
        out.insert(d.target.clone(), healthy);
    }
    out
}

/// Whether a healthy mount should be re-pointed at its elected source, honouring
/// the mount's typed [`RemountPolicy`]. Pure so the fail-back / degrade / held /
/// Safe-busy matrix is unit-tested.
///
/// - failover disabled            → never swap (mount pinned).
/// - fail-back but `return_to_primary = false` → stay degraded.
/// - replication ref + not confirmed healthy → hold (the failover-safety gate).
/// - Safe aggression + busy mount → never disrupt (pending; next idle tick).
/// - otherwise (fail-back, degrade, or moving off a held/legacy source) → swap.
fn should_swap(d: &DesiredMount, sig: &FailoverSignal) -> bool {
    let pol = &d.remount_policy;
    if !pol.failover.enabled {
        return false;
    }
    let trans = source_election::transition(&d.sources, sig.active.as_deref(), &sig.elected);
    let candidate = match trans {
        source_election::Transition::FailBack { .. } => pol.failover.return_to_primary,
        source_election::Transition::Degrade { .. } => true,
        // Moving off a source no longer among the enabled routes (a held/drained
        // or legacy source) — only when something is actually mounted.
        source_election::Transition::Mount { .. } => sig.active.is_some(),
        source_election::Transition::Unchanged | source_election::Transition::EmptyTarget => false,
    };
    if !candidate {
        return false;
    }
    // Failover-safety gate: a share bound to a replication relationship may only
    // swap its active route once replication between the members is CONFIRMED
    // healthy. Unknown (`None`, e.g. no provider registered yet) and unhealthy
    // (`Some(false)`) both hold — failing an active mount over to a member whose
    // data may be stale/unreplicated is worse than waiting on the primary. A
    // share with no replication ref is ungated (unchanged pre-gate behaviour).
    if d.replication.is_some() && sig.replication_healthy != Some(true) {
        return false;
    }
    // The Plex/Jellyfin guarantee: never force-swap a busy mount under Safe.
    !(pol.aggression == RemountAggression::Safe && sig.busy)
}

/// Retain predicate for the stale-streak counter prune. The tick's `counters`
/// map is shared by two confirm streaks: the stale streak (bare-target keys) and
/// the failover streak (`failover:`-prefixed keys). When the stale block prunes
/// keys for placements that no longer exist, it must leave the failover streak's
/// keys untouched — otherwise every tick would reset the failover streak to zero,
/// and a healthy mount sitting on a held/drained source could never accrue
/// `confirm_ticks` to swap off it (the failover block owns/prunes those keys via
/// its own retain). Keep a key iff it is failover-namespaced OR names a target
/// still desired.
fn keep_stale_counter_key(key: &str, desired: &[DesiredMount]) -> bool {
    key.starts_with("failover:") || desired.iter().any(|d| d.target == key)
}

/// Whether a live mount's options have drifted from the share's rendered
/// options, comparing ONLY the tokens that appear verbatim in `/proc/mounts` and
/// carry operator intent. Pure so the false-positive-prone comparison is
/// unit-tested against real kernel option strings.
///
/// The kernel does NOT echo the operator's `-o` string back: it reorders tokens,
/// injects its own (`addr=`, `clientaddr=`, `local_lock=`, `sec=`, `rsize=`,
/// `wsize=`, `namlen=`, `proto=`), and EXPANDS/renames some options — notably
/// `actimeo=30` is stored as `acregmin/acregmax/acdirmax=30`, never as
/// `actimeo`. A raw subset/superset check therefore false-positives constantly.
/// So this compares a focused, principled set:
///
/// - the mutually-exclusive `hard`/`soft` pair (the primary case: an operator
///   flips `hard`→`soft` and the live mount must be remounted). The kernel always
///   emits exactly one of the two; drift iff the desired string pins a hardness
///   that differs from the live one.
/// - `softreval` presence (a verbatim boolean flag).
/// - the keyed tunables that appear verbatim and unrenamed: `vers`, `timeo`,
///   `retrans`, `nconnect`. Drift iff desired pins the key and live's value
///   differs (or the key is absent from the live mount).
///
/// Keys the kernel expands/renames (`actimeo`) or injects (`addr`, `sec`,
/// `rsize`, …) are deliberately IGNORED: they can't be compared verbatim and
/// aren't the operator's transport-safety intent that this loop reconciles.
fn options_drifted(desired: &str, live: &[String]) -> bool {
    let desired: Vec<&str> = desired
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();

    // hard/soft: the kernel always emits one; drift only when desired pins a
    // hardness that differs from what is live.
    let soft = |toks: &dyn Fn(&str) -> bool| -> Option<bool> {
        if toks("soft") {
            Some(true)
        } else if toks("hard") {
            Some(false)
        } else {
            None
        }
    };
    let desired_has = |t: &str| desired.contains(&t);
    let live_has = |t: &str| live.iter().any(|x| x == t);
    if let Some(d) = soft(&desired_has)
        && let Some(l) = soft(&live_has)
        && d != l
    {
        return true;
    }

    // softreval: a verbatim boolean flag, compared symmetrically.
    if desired_has("softreval") != live_has("softreval") {
        return true;
    }

    // Keyed tunables that survive verbatim: drift iff desired pins a value the
    // live mount does not carry.
    let keyed = |toks: &[&str], key: &str| -> Option<String> {
        let prefix = format!("{key}=");
        toks.iter()
            .find_map(|t| t.strip_prefix(&prefix).map(str::to_string))
    };
    let live_str: Vec<&str> = live.iter().map(String::as_str).collect();
    for key in ["vers", "timeo", "retrans", "nconnect"] {
        if let Some(want) = keyed(&desired, key)
            && keyed(&live_str, key).as_deref() != Some(want.as_str())
        {
            return true;
        }
    }
    false
}

/// Normalize a mount source string so two spellings of the *same* server+export
/// compare equal. The kernel mount table and a legacy autofs mount can echo a
/// source that differs from converge's route-rendered form only cosmetically: a
/// trailing slash on the export/share path, or ASCII-case in the (DNS
/// case-insensitive) host. Those must NOT read as a different source.
///
/// Hostname-vs-IP is deliberately NOT reconciled — that is a genuine source
/// difference for source election / failover to decide, not a normalization. The
/// export/share path stays case-sensitive (NFS/SMB paths are).
fn norm_source(s: &str) -> String {
    fn trim(p: &str) -> &str {
        p.trim_end_matches('/')
    }
    if let Some(rest) = s.strip_prefix("//") {
        // SMB `//server/share…`
        match rest.split_once('/') {
            Some((host, path)) => format!("//{}/{}", host.to_ascii_lowercase(), trim(path)),
            None => format!("//{}", rest.to_ascii_lowercase()),
        }
    } else if let Some((host, path)) = s.split_once(':') {
        // NFS `host:/export`
        format!("{}:{}", host.to_ascii_lowercase(), trim(path))
    } else {
        trim(&s.to_ascii_lowercase()).to_string()
    }
}

/// Whether two mount source strings name the same server+export, tolerating the
/// cosmetic spelling differences [`norm_source`] normalizes away.
fn same_source(a: &str, b: &str) -> bool {
    norm_source(a) == norm_source(b)
}

/// What to do about a HEALTHY mount whose live options have drifted from the
/// share's rendered options. Pure so the elected-source gate and the Safe/busy
/// defer are unit-tested without a live mount table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriftDecision {
    /// On its elected source and either idle or Force — schedule the lazy
    /// unmount+remount that re-options it.
    Remount,
    /// On its elected source but busy under Safe — defer to the next idle tick
    /// (the Plex/Jellyfin guarantee).
    DeferBusySafe,
    /// Live source is not the elected source — leave the re-option to the
    /// failover/pin path, which owns wrong-source mounts.
    NotOnElected,
    /// No live elected source this tick — nothing to remount onto.
    NoElection,
}

/// Decide the drift-remount action from the elected source, the kernel-active
/// source, the mount's aggression, and whether it is busy. The elected-vs-active
/// comparison goes through [`same_source`] so a cosmetic rendering difference
/// (trailing slash / host case — notably a legacy autofs mount adopted at
/// cutover) no longer makes a genuinely-drifted idle mount silently skip its
/// remount.
fn drift_action(
    elected: Option<&str>,
    active: Option<&str>,
    aggression: RemountAggression,
    busy: bool,
) -> DriftDecision {
    let Some(elected) = elected else {
        return DriftDecision::NoElection;
    };
    if !active.is_some_and(|a| same_source(elected, a)) {
        return DriftDecision::NotOnElected;
    }
    if aggression == RemountAggression::Safe && busy {
        return DriftDecision::DeferBusySafe;
    }
    DriftDecision::Remount
}

/// Decide the convergence actions. Pure: given the desired mounts for this host,
/// the set of targets currently mounted at all, the subset that are mounted
/// **and healthy**, and the per-target failover signals, return the
/// mount/unmount actions that make reality match.
///
/// - desired, not mounted             → Mount
/// - desired, mounted but stale        → Unmount then Mount (remount, fail forward)
/// - desired, mounted, healthy, wrong source (per policy) → Unmount then Mount (swap)
/// - desired, mounted, healthy, right source, options drifted → Unmount then Mount (re-option)
/// - desired, mounted and healthy on the elected source   → nothing
/// - mounted but no longer desired     → Unmount (removed placement)
///
/// The `option_drift` set is the targets the tick already cleared for an
/// option-drift remount (drift detected, on the elected source, and either idle
/// or Force — the Safe/busy defer is applied by the tick before this set is
/// built, so `plan` just materializes the remount).
///
/// Ordering matters: the remount Unmount precedes its Mount, and stray-target
/// Unmounts come last, so the applier can run the vector top-to-bottom.
pub fn plan(
    desired: &[DesiredMount],
    mounted_any: &HashSet<String>,
    mounted_healthy: &HashSet<String>,
    failover: &HashMap<String, FailoverSignal>,
    option_drift: &HashSet<String>,
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
        } else if failover
            .get(&d.target)
            .is_some_and(|sig| should_swap(d, sig))
        {
            // Healthy but on the wrong source and policy allows the swap: release
            // then remount onto the elected source (fail-back / degrade / un-hold).
            actions.push(Action::Unmount {
                target: d.target.clone(),
            });
            actions.push(Action::Mount {
                target: d.target.clone(),
            });
        } else if option_drift.contains(&d.target) {
            // Healthy, on the elected source, but the live options drifted from
            // the share's rendered options: lazily release then remount so the
            // mount comes back with the desired options. The Unmount is the
            // existing `umount -lf` (lazy) — open handles on streaming media keep
            // reading the old superblock, only new opens bind the re-optioned
            // mount.
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

/// How a queued [`Action::Mount`] should execute given what the kernel mount
/// table currently shows at the target. Keeps convergence idempotent + adopting
/// and — above all — non-stacking: a bare mount never lands a second filesystem
/// on top of one already occupying the mountpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MountExec {
    /// Nothing is mounted at the target — mount the elected source.
    Proceed,
    /// A desired source is already mounted here — adopt it as satisfied and do
    /// nothing (no remount, no stack). Re-pointing onto the *elected* source, when
    /// policy allows, is the failover-swap path in [`plan`], which unmounts first.
    Adopt,
    /// A source outside the desired set occupies the target. Never stack: a safe
    /// replace goes through the drain/unmount path, so a bare mount leaves it and
    /// the caller logs a loud WARN.
    Foreign(String),
}

/// Decide how to execute a bare `Mount` at a target from the kernel-observed
/// `active` source, the source about to be mounted (`elected`), and the mount's
/// `desired` ordered sources. Pure so the adopt / proceed / foreign matrix is
/// unit-tested without touching a real mount table.
fn mount_execution(elected: &str, desired: &[String], active: Option<&str>) -> MountExec {
    match active {
        None => MountExec::Proceed,
        // Already mounted from a source we want (the elected one, or any other
        // enabled desired source) — satisfied, do not remount/stack.
        Some(a) if a == elected || desired.iter().any(|s| s == a) => MountExec::Adopt,
        // Occupied by something outside the desired set — refuse to stack.
        Some(a) => MountExec::Foreign(a.to_string()),
    }
}

/// The ledger of targets THIS host has natively mounted, persisted so a
/// placement removed while the daemon was down is still reconciled on the next
/// boot. It is per-host materialized state — never replicated — and holds only
/// targets orca itself mounted, so an autofs/foreign mount can never land in it.
fn ledger_file() -> Option<PathBuf> {
    contract::config::state_dir()
        .ok()
        .map(|d| d.join("managed_mounts.json"))
}

fn load_ledger_at(path: &Path) -> HashSet<String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashSet::new(), // absent/unreadable → empty (first run)
    }
}

/// Atomic ledger write (temp + rename) so a concurrent reader never sees a
/// half-written file. Best-effort: a failure to persist is logged, not fatal —
/// the loop stays correct, it just re-derives on the next tick.
fn save_ledger_at(path: &Path, ledger: &HashSet<String>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(ledger).unwrap_or_else(|_| b"[]".to_vec());
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn load_ledger() -> HashSet<String> {
    ledger_file()
        .map(|p| load_ledger_at(&p))
        .unwrap_or_default()
}

fn save_ledger(ledger: &HashSet<String>) {
    if let Some(p) = ledger_file()
        && let Err(e) = save_ledger_at(&p, ledger)
    {
        warn!("[converge] could not persist managed-mount ledger: {e}");
    }
}

/// Targets orca has mounted that are no longer a desired placement — the removal
/// set. Pure so it is unit-tested independent of the filesystem/probe.
fn orphan_targets(ledger: &HashSet<String>, desired_targets: &HashSet<String>) -> Vec<String> {
    ledger
        .iter()
        .filter(|t| !desired_targets.contains(*t))
        .cloned()
        .collect()
}

/// Build the [`MountReq`] for a desired target from an elected live `source` and
/// an already-resolved optional secret-file (the owning backend produced its
/// `contents`; core writes it 0600 before mounting).
pub fn mount_req(
    d: &DesiredMount,
    source: &str,
    secret_file: Option<crate::mount_exec::SecretFile>,
) -> MountReq {
    MountReq {
        source: source.to_string(),
        target: d.target.clone(),
        fstype: d.fstype.clone(),
        options: d.options.clone(),
        secret_file,
    }
}

/// Resolve the generic secret-file for a desired mount, if it declares a
/// credential: resolve the `SecretRef` to plaintext, then ask the owning backend
/// to render the root-owned secret-file `{path, contents}` from it. Core knows
/// neither the file's grammar nor its path convention beyond validating the path;
/// the backend owns both. Returns `None` when the share declares no credential, or
/// (fail-closed) logs and returns `None` if resolution/rendering fails so a mount
/// never proceeds with a stale or malformed secret-file.
async fn resolve_secret_file(d: &DesiredMount) -> Option<crate::mount_exec::SecretFile> {
    let cred = d.credential.as_deref().filter(|c| !c.is_empty())?;
    let Some(backend) = plugin_toolkit::storage::backend(&d.backend) else {
        warn!(
            "[converge] {} declares a credential but backend `{}` is not registered; \
             mount will fail closed",
            d.target, d.backend
        );
        return None;
    };
    // Re-validate a minimal spec so the backend resolves its own SecretRef and
    // renders the secret-file freshly (plaintext is never persisted). The backend
    // populates `NormalizedSpec::secret_file`; core just carries it to the applier.
    let spec = plugin_toolkit::storage::MountSpec {
        backend: d.backend.clone(),
        target: d.target.clone(),
        fstype: d.fstype.clone(),
        source: d.sources.first().cloned().unwrap_or_default(),
        failover_sources: d.sources.iter().skip(1).cloned().collect(),
        options: Some(d.options.clone()),
        credential: Some(plugin_toolkit::storage::SecretRef(cred.to_string())),
        remount_policy: None,
        enabled: true,
    };
    match backend.validate_spec(&spec).await {
        Ok(normalized) => normalized
            .secret_file
            .map(|sf| crate::mount_exec::SecretFile {
                path: sf.path,
                contents: sf.contents,
            }),
        Err(e) => {
            warn!(
                "[converge] {} secret-file render failed: {e}; mount will fail closed",
                d.target
            );
            None
        }
    }
}

/// Parse the server host out of a mount source by shape alone (`//server/share`
/// → `server`, `host:/export` → `host`). Used to aim the NFS RPC-NULL probe,
/// which needs only the host (it dials port 2049 itself).
fn host_of_source(source: &str) -> Option<String> {
    if let Some(rest) = source.strip_prefix("//") {
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or("").trim();
        (!host.is_empty()).then(|| host.to_string())
    } else if let Some((host, _)) = source.split_once(':') {
        let host = host.trim();
        (!host.is_empty()).then(|| host.to_string())
    } else {
        None
    }
}

/// One transport-liveness probe of a source, resolving [`SourceProbe::Auto`]
/// against `fstype`: NFS filesystem types get the RPC-NULL probe (so a hung nfsd
/// with TCP up still reads down and election won't fail back onto it); everything
/// else gets the bare TCP connect. Sync probes run on the blocking pool.
async fn probe_live(source: &str, fstype: &str, probe: SourceProbe, timeout: Duration) -> bool {
    let resolved = probe.resolve(fstype);
    let (src, fst) = (source.to_string(), fstype.to_string());
    tokio::task::spawn_blocking(move || match resolved {
        SourceProbe::Nfs => match host_of_source(&src) {
            Some(host) => probe_source_nfs(&host, timeout),
            None => false,
        },
        // `Tcp` (and `Auto`, already resolved above to Tcp for non-nfs).
        _ => probe_source(&src, &fst, timeout),
    })
    .await
    .unwrap_or(false)
}

/// Elect the first live source from `sources` (probed in priority order) using
/// the policy's resolved probe. orca owns source election — one live source is
/// chosen per mount attempt, primary-first so a recovered primary always wins the
/// next tick (fail-back for free). Returns the [`Election`] so callers can
/// classify the transition; [`Election::Empty`] when every ordered source is down.
async fn elect(
    sources: &[String],
    fstype: &str,
    probe: SourceProbe,
    timeout: Duration,
) -> Election {
    for (index, s) in sources.iter().enumerate() {
        if probe_live(s, fstype, probe, timeout).await {
            return Election::Elected {
                source: s.clone(),
                index,
            };
        }
    }
    Election::Empty
}

/// The elected source string, or `None` for an empty election.
fn elected_source(election: &Election) -> Option<String> {
    match election {
        Election::Elected { source, .. } => Some(source.clone()),
        Election::Empty => None,
    }
}

/// Spawn the per-host convergence loop. Returns the periodic handle the daemon
/// leaks for the process lifetime (scheduler convention).
pub fn spawn() -> JoinHandle<()> {
    // Per-target consecutive-stale counters (the confirm-ticks blip filter),
    // shared across ticks.
    let counters = Arc::new(Mutex::new(HashMap::<String, u32>::new()));
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "storage.converge.run",
            initial_delay: Duration::from_secs(12),
            interval: Duration::from_secs(INTERVAL_SECS),
        },
        periodic::boxed(move || {
            let counters = counters.clone();
            async move { tick(&counters).await }
        }),
    )
}

/// One convergence pass for this host: resolve desired placements, probe each,
/// apply the confirm-ticks blip filter to staleness, plan, and execute the
/// resulting mounts/remounts through the privileged applier. Only ever touches
/// targets that are desired placements — never an NFS mount orca did not declare
/// (so it is safe to run alongside the legacy autofs path during migration).
async fn tick(counters: &Mutex<HashMap<String, u32>>) -> anyhow::Result<()> {
    let this_host = host_identity::machine_id();
    let desired = desired_for_host(this_host)?;
    let desired_targets: HashSet<String> = desired.iter().map(|d| d.target.clone()).collect();
    let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);

    // Ledger of targets orca has natively mounted here. Drives removal
    // reconciliation: a placement deleted from the replicated `mounts` table
    // leaves its target in the ledger but out of `desired`, so we release it.
    let mut ledger = load_ledger();

    // Removal reconcile: probe each orphan (ledger − desired) first, so we only
    // `umount` one that is still mounted — a target already gone is simply
    // forgotten. Only ledger targets are ever considered, so an autofs/foreign
    // mount is never touched (coexistence stays safe).
    let mut orphan_unmounts: Vec<String> = Vec::new();
    for t in orphan_targets(&ledger, &desired_targets) {
        match autofs::probe(&t, timeout).await {
            Health::Missing => {
                ledger.remove(&t);
            }
            _ => orphan_unmounts.push(t),
        }
    }

    // The kernel mount table, read once: maps each desired target to the source
    // it is currently mounted from (`active`) so election can classify a
    // fail-back / degrade transition without a second probe, and to the live `-o`
    // option tokens the kernel reports so option-drift can be detected without a
    // second read.
    let mount_table = plugin_toolkit::storage::mount_table().unwrap_or_default();
    // Count entries per mountpoint BEFORE collapsing to the by-target maps: >1
    // means the target is stacked (an anomaly the write path blocks but a
    // reconcile must tolerate + surface). Surfaced per-target as `multi_mounted`.
    let mut mount_count_by_target: HashMap<String, usize> = HashMap::new();
    for e in &mount_table {
        *mount_count_by_target
            .entry(e.mountpoint.clone())
            .or_insert(0) += 1;
    }
    let active_by_target: HashMap<String, String> = mount_table
        .iter()
        .map(|e| (e.mountpoint.clone(), e.source.clone()))
        .collect();
    let live_options_by_target: HashMap<String, Vec<String>> = mount_table
        .into_iter()
        .map(|e| (e.mountpoint, e.options))
        .collect();

    // Probe each desired target: mounted+Ok, mounted+stale, or missing. Record
    // the classification as a stored `Health` (written to the row at tick end so
    // `storage.mount.detail` reports it without a live probe).
    let mut mounted_any: HashSet<String> = HashSet::new();
    let mut healthy: HashSet<String> = HashSet::new();
    let mut stale_now: HashSet<String> = HashSet::new();
    let mut health_by_target: HashMap<String, Health> = HashMap::new();
    for d in &desired {
        // Cross-check the kernel mount table FIRST: a bare mountpoint dir that
        // exists with nothing mounted through it `stat`s clean, so `probe_health`
        // returns `Health::Ok` and an unmounted target would be misread as healthy
        // — leaving `plan()` with nothing to do and the placement never mounted.
        // Absence from `/proc/mounts` is the only reliable "not mounted" signal.
        let absent = autofs::target_has_no_mount(&d.target).await;
        let health = autofs::probe(&d.target, timeout).await;
        match classify(absent, health) {
            Presence::Missing => {
                health_by_target.insert(d.target.clone(), Health::Missing);
            }
            Presence::Healthy => {
                mounted_any.insert(d.target.clone());
                healthy.insert(d.target.clone());
                health_by_target.insert(d.target.clone(), Health::Ok);
            }
            Presence::Stale => {
                mounted_any.insert(d.target.clone());
                stale_now.insert(d.target.clone());
                health_by_target.insert(d.target.clone(), Health::Stale);
            }
        }
    }

    // Confirm-ticks: advance per-target stale streaks; only a target stale for
    // CONFIRM_TICKS consecutive ticks is remounted. A stale-but-unconfirmed
    // target is kept in `healthy` so `plan` leaves it alone this tick.
    let confirmed_stale = {
        let mut c = counters.lock().expect("converge counters poisoned");
        // Prune only THIS streak's own (bare-target) keys for gone placements.
        // The failover streak shares this map under a `failover:` prefix and is
        // pruned by its own retain below; evicting those keys here would reset the
        // failover confirm streak every tick, so a healthy mount on a held/drained
        // source could never accrue `confirm_ticks` and would never swap off it.
        c.retain(|t, _| keep_stale_counter_key(t, &desired));
        let mut confirmed = HashSet::new();
        for d in &desired {
            if stale_now.contains(&d.target) {
                let n = c.entry(d.target.clone()).or_insert(0);
                *n += 1;
                if *n >= CONFIRM_TICKS {
                    confirmed.insert(d.target.clone());
                    *n = 0; // reset so a still-down mount re-confirms
                }
            } else {
                c.remove(&d.target);
            }
        }
        confirmed
    };
    for d in &desired {
        if stale_now.contains(&d.target) && !confirmed_stale.contains(&d.target) {
            healthy.insert(d.target.clone()); // ride out the blip
        }
    }

    // Election pass: elect a live source per desired mount using the policy's
    // resolved probe (RPC-NULL for nfs, so a hung primary with TCP up is NOT
    // elected). Reused below both for the failover swap decision and to source
    // the actual mount, so each source is probed once per tick.
    let mut elected_by_target: HashMap<String, Election> = HashMap::new();
    for d in &desired {
        let e = elect(
            &d.sources,
            &d.fstype,
            d.remount_policy.failover.probe,
            timeout,
        )
        .await;
        elected_by_target.insert(d.target.clone(), e);
    }

    // Failover signals for HEALTHY mounts on the wrong source. Confirm-gated with
    // the same counters map (namespaced `failover:`) so a single-tick election
    // blip never force-swaps a live mount; only a target whose swap-worthy
    // transition persists `confirm_ticks` is fed to `plan`.
    let mut failover: HashMap<String, FailoverSignal> = HashMap::new();
    // Observe replication health BEFORE the lock (the resolve is async; the
    // counters lock forbids an await). Feeds both the dry candidacy check and the
    // real signal so a replication-gated share never even enters the confirm
    // streak while its relationship is unconfirmed.
    let repl_health = replication_health_by_target(&desired).await;
    // Phase 1: under the lock (no await), advance the confirm streaks and collect
    // the targets whose swap-worthy transition has persisted `confirm_ticks`.
    let confirmed_swaps: Vec<(String, Election, Option<String>)> = {
        let mut c = counters.lock().expect("converge counters poisoned");
        c.retain(|k, _| {
            k.strip_prefix("failover:")
                .map(|t| desired.iter().any(|d| d.target == t))
                .unwrap_or(true)
        });
        let mut confirmed = Vec::new();
        for d in &desired {
            let key = format!("failover:{}", d.target);
            if !healthy.contains(&d.target) {
                c.remove(&key);
                continue;
            }
            let election = elected_by_target
                .get(&d.target)
                .cloned()
                .unwrap_or(Election::Empty);
            let active = active_by_target.get(&d.target).cloned();
            // A cheap dry-run signal (busy=false) tells us if this transition is
            // even a swap candidate before we pay for the `fuser` busy probe.
            let dry = FailoverSignal {
                elected: election.clone(),
                active: active.clone(),
                busy: false,
                replication_healthy: repl_health.get(&d.target).copied().flatten(),
            };
            if should_swap(d, &dry) {
                let n = c.entry(key.clone()).or_insert(0);
                *n += 1;
                if *n >= d.remount_policy.failover.confirm_ticks.max(1) {
                    *n = 0;
                    confirmed.push((d.target.clone(), election, active));
                }
            } else {
                c.remove(&key);
            }
        }
        confirmed
    };
    // Phase 2: the `fuser` busy probe (async) runs with the lock released.
    for (target, election, active) in confirmed_swaps {
        let busy = autofs::is_busy(&target).await;
        let replication_healthy = repl_health.get(&target).copied().flatten();
        failover.insert(
            target,
            FailoverSignal {
                elected: election,
                active,
                busy,
                replication_healthy,
            },
        );
    }

    // Option-drift detection for HEALTHY mounts already sitting on their elected
    // source. The share's rendered options changed (e.g. `hard`→`soft`) but the
    // live mount still carries the old options. `drift_by_target` records the raw
    // divergence for EVERY genuinely-Ok target (so `storage.mount.list/detail`
    // shows it even when a busy Safe mount is deferred); `option_drift_remount`
    // is the subset actually scheduled to remount this tick.
    let mut drift_by_target: HashMap<String, bool> = HashMap::new();
    let mut option_drift_remount: HashSet<String> = HashSet::new();
    for d in &desired {
        // Only a genuinely-Ok mount is a drift candidate — never a stale-blip
        // rider kept in `healthy` to ride out a single stale probe.
        if health_by_target.get(&d.target) != Some(&Health::Ok) {
            continue;
        }
        let Some(live) = live_options_by_target.get(&d.target) else {
            continue;
        };
        if !options_drifted(&d.options, live) {
            continue;
        }
        drift_by_target.insert(d.target.clone(), true);
        // A target already being failover-swapped this tick remounts with the
        // desired options anyway — don't double-schedule.
        if failover
            .get(&d.target)
            .is_some_and(|sig| should_swap(d, sig))
        {
            continue;
        }
        // Only re-option a mount that is on its ELECTED source (compared through
        // `same_source`, so a cosmetic rendering difference — trailing slash / host
        // case, e.g. a legacy autofs mount adopted at cutover — does NOT skip it).
        // A mount genuinely parked on a non-elected source is the failover/pin
        // path's to decide. The `fuser` busy probe (for the Plex/Jellyfin defer) is
        // paid only for a genuinely-drifted healthy mount, which is rare.
        let elected = elected_by_target.get(&d.target).and_then(elected_source);
        let active = active_by_target.get(&d.target).map(String::as_str);
        let busy = autofs::is_busy(&d.target).await;
        match drift_action(
            elected.as_deref(),
            active,
            d.remount_policy.aggression,
            busy,
        ) {
            DriftDecision::Remount => {
                info!(
                    "[converge] {} option drift (desired `{}`); scheduling lazy re-option remount",
                    d.target, d.options
                );
                option_drift_remount.insert(d.target.clone());
            }
            // The Plex/Jellyfin guarantee: the lazy detach+remount momentarily has
            // no filesystem bound at the path, so a NEW open racing it would fail.
            // Under Safe a busy mount is deferred to the next idle tick.
            DriftDecision::DeferBusySafe => info!(
                "[converge] {} option drift (desired `{}`); deferring remount — busy under \
                 Safe, will re-option when idle",
                d.target, d.options
            ),
            DriftDecision::NotOnElected => info!(
                "[converge] {} option drift (desired `{}`) but live source {active:?} is not the \
                 elected source {elected:?}; leaving re-option to the failover/pin path",
                d.target, d.options
            ),
            DriftDecision::NoElection => warn!(
                "[converge] {} option drift (desired `{}`) but no live elected source; skipping \
                 re-option this tick",
                d.target, d.options
            ),
        }
    }

    // The live options projected for persistence (comma-joined, kernel order) so
    // `storage.mount.list/detail` can show hard-vs-soft per host.
    let active_options_by_target: HashMap<String, String> = live_options_by_target
        .iter()
        .map(|(t, opts)| (t.clone(), opts.join(",")))
        .collect();

    // Plan the desired set (mount / stale-remount / failover swap / re-option),
    // then append the orphan releases (removed placements orca still holds mounted).
    let mut actions = plan(
        &desired,
        &mounted_any,
        &healthy,
        &failover,
        &option_drift_remount,
    );
    for t in &orphan_unmounts {
        actions.push(Action::Unmount { target: t.clone() });
    }
    // No early-return on an empty plan: the execution blocks below already no-op
    // when there is nothing to mount/unmount, and the tail must still run the
    // consumer-stale sweep, adopt the ledger, and persist health. A healthy host
    // with nothing to converge is EXACTLY when a container-pinned stale
    // superblock (ESTALE inside the guest) needs the backend recovery sweep.

    let orphan_set: HashSet<&str> = orphan_unmounts.iter().map(String::as_str).collect();

    // Split: unmounts run first (a stale target's release precedes its remount),
    // then mounts, each with a freshly-elected live source.
    let by_target: HashMap<&str, &DesiredMount> =
        desired.iter().map(|d| (d.target.as_str(), d)).collect();
    let mut unmounts: Vec<String> = Vec::new();
    let mut reqs: Vec<MountReq> = Vec::new();
    for a in &actions {
        match a {
            Action::Unmount { target } => unmounts.push(target.clone()),
            Action::Mount { target } => {
                let Some(d) = by_target.get(target.as_str()) else {
                    continue;
                };
                // Reuse this tick's election (probed with the policy's resolved
                // probe); a target already elected above is not re-probed.
                let elected = match elected_by_target.get(target.as_str()) {
                    Some(e) => elected_source(e),
                    None => elected_source(
                        &elect(
                            &d.sources,
                            &d.fstype,
                            d.remount_policy.failover.probe,
                            timeout,
                        )
                        .await,
                    ),
                };
                match elected {
                    Some(src) => {
                        // Idempotent + non-stacking guard. A Mount paired with an
                        // Unmount this tick (stale-remount / failover swap) has its
                        // occupant released first, so proceed. A BARE Mount (target
                        // classified missing) must never stack: consult the kernel
                        // table captured this tick and adopt an already-desired
                        // source, or refuse a foreign occupant.
                        if !unmounts.contains(target) {
                            match mount_execution(
                                &src,
                                &d.sources,
                                active_by_target.get(target.as_str()).map(String::as_str),
                            ) {
                                MountExec::Adopt => {
                                    info!(
                                        "[converge] {target} already mounted from a desired \
                                         source; adopting (no remount)"
                                    );
                                    ledger.insert(target.clone());
                                    continue;
                                }
                                MountExec::Foreign(active) => {
                                    warn!(
                                        "[converge] {target} occupied by foreign mount {active} \
                                         (desired {src}); refusing to stack — a safe replace \
                                         requires the drain/unmount path, leaving as-is"
                                    );
                                    continue;
                                }
                                MountExec::Proceed => {}
                            }
                        }
                        let secret_file = resolve_secret_file(d).await;
                        reqs.push(mount_req(d, &src, secret_file));
                    }
                    None => warn!(
                        "[converge] {} has NO live source ({} ordered sources down); \
                         leaving unmounted",
                        target,
                        d.sources.len()
                    ),
                }
            }
        }
    }

    if !unmounts.is_empty() {
        let r = run_privileged(&PrivilegedOp::Unmount {
            targets: unmounts.clone(),
        })
        .await;
        for e in &r.errors {
            warn!("[converge] unmount error: {e}");
        }
        // A released target reported in `changed`: if it was an orphan, it is now
        // fully torn down and leaves the ledger; a stale-remount release stays
        // (its Mount re-adds it below).
        for t in &r.changed {
            if orphan_set.contains(t.as_str()) {
                ledger.remove(t);
                info!("[converge] unmounted removed placement {t}");
            } else {
                info!("[converge] released {t} (remounting)");
            }
        }
    }
    if !reqs.is_empty() {
        // The authoritative keep-set: every secret-file path in the batch being
        // mounted. The root helper reaps any secret-file under SECRET_FILE_DIR not in
        // this set (deleted mount / rotated secret). Grammar-agnostic — core sees
        // only paths a backend produced.
        let keep_secret_files: Vec<String> = reqs
            .iter()
            .filter_map(|r| r.secret_file.as_ref().map(|sf| sf.path.clone()))
            .collect();
        let r = run_privileged(&PrivilegedOp::Mount {
            mounts: reqs,
            keep_secret_files,
        })
        .await;
        for t in &r.changed {
            ledger.insert(t.clone());
            info!("[converge] mounted {t}");
        }
        for e in &r.errors {
            warn!("[converge] mount error: {e}");
        }
    }

    // Backend consumer-stale recovery sweep — the one heal the host-mount
    // lifecycle above cannot perform: a container pinning a stale NFS superblock
    // (ESTALE inside the guest) while the host mount itself reads healthy. Ported
    // from the retired autofs self-heal loop and driven off the desired
    // (shares⋈mounts⋈routes) set, NEVER the legacy managed_mounts table. Gated by
    // the host remediation policy exactly as the self-heal loop gated it: the
    // sweep both detects AND repairs (the plugin restarts consumers behind its own
    // host-healthy + consumer-stale guard), so under a non-acting policy there is
    // no read-only probe to run and it is skipped entirely. Core never restarts
    // containers itself — it only calls the backend. Runs every tick (healthy
    // hosts included), which is exactly where consumer-stale surfaces.
    let policy = remediation_policy();
    if policy.acts() {
        let merged = crate::storage_tools::recover_backends_only(
            desired
                .iter()
                .map(|d| (d.backend.as_str(), d.target.as_str())),
            timeout,
        )
        .await;
        for t in &merged.recovered {
            info!("[converge] backend recovered consumer-stale mount {t}");
        }
        for t in &merged.remounted {
            info!("[converge] backend remounted absent mount {t}");
        }
        for t in &merged.still_stale {
            warn!("[converge] backend reports {t} still stale after recovery");
        }
        for t in &merged.still_missing {
            warn!("[converge] backend could not remount absent mount {t}");
        }
        for e in &merged.errors {
            warn!("[converge] backend recover error: {e}");
        }
        if policy.notifies() && !merged.recovered.is_empty() {
            raise_notification(
                "remediation:converge:backend-recovered".to_string(),
                Severity::Info,
                false,
                "Backend recovered stale consumer mounts".to_string(),
                format!("Recovered consumer-stale mounts: {:?}", merged.recovered),
                None,
            );
        }
    } else {
        debug!("[converge] remediation disabled; skipping backend consumer recovery sweep");
    }

    // Guest-mount reconcile: placements whose `guest` is set are never mounted on
    // this host — they are rendered INTO the guest's own config (unprivileged LXC
    // `lxc.mount.entry` / VM cloud-init) by the registered `GuestMountApplier` so
    // the guest re-establishes the mount independently on its own lifecycle. On a
    // virtualization host (e.g. Proxmox) the plugin registers exactly one applier;
    // `apply` is idempotent, so re-applying every desired spec each tick is the
    // reconcile. Hosts with no applier registered (no guest rows target them) skip.
    reconcile_guest_mounts(this_host).await;

    // Adopt desired targets orca already holds mounted (e.g. mounted in a prior
    // tick, or still Ok after a restart) so a later removal is reconciled even if
    // this process never performed the mount itself this run.
    for d in &desired {
        if mounted_any.contains(&d.target) {
            ledger.insert(d.target.clone());
        }
    }
    save_ledger(&ledger);
    persist_mount_state(
        this_host,
        &health_by_target,
        &active_by_target,
        &active_options_by_target,
        &drift_by_target,
        &mount_count_by_target,
    );
    Ok(())
}

/// Reconcile every guest-targeted placement for `this_host` by dispatching its
/// rendered [`GuestMountSpec`](plugin_toolkit::storage::GuestMountSpec) to each
/// registered [`GuestMountApplier`](plugin_toolkit::storage::GuestMountApplier).
/// `apply` is idempotent (the applier writes the guest's config only when it
/// diverges), so this runs unconditionally each tick. Best-effort: a build or
/// apply failure is logged, never fatal to the tick. No-op when no applier is
/// registered or no guest rows target this host.
async fn reconcile_guest_mounts(this_host: &str) {
    let appliers = plugin_toolkit::storage::guest_appliers();
    if appliers.is_empty() {
        return;
    }
    let specs = match desired_guest_mounts_for_host(this_host) {
        Ok(s) => s,
        Err(e) => {
            warn!("[converge] guest desired-state build failed: {e}");
            return;
        }
    };
    for spec in &specs {
        for applier in &appliers {
            match applier.apply(spec).await {
                Ok(()) => debug!(
                    "[converge] guest applier '{}' reconciled {} in {}",
                    applier.name(),
                    spec.target,
                    spec.guest
                ),
                Err(e) => warn!(
                    "[converge] guest applier '{}' failed to apply {} in {}: {e}",
                    applier.name(),
                    spec.target,
                    spec.guest
                ),
            }
        }
    }
}

/// Write each of this host's mount rows' last-known `health`, `active_route`
/// (the source it is currently mounted from), `active_options` (the live `-o`
/// tokens), and `drift` (whether those diverge from desired) so
/// `storage.mount.detail`/`list` report them WITHOUT a live probe — the read path
/// takes no fan-out. Best-effort: a persistence failure is logged, never fatal to
/// the tick.
fn persist_mount_state(
    this_host: &str,
    health_by_target: &HashMap<String, Health>,
    active_by_target: &HashMap<String, String>,
    active_options_by_target: &HashMap<String, String>,
    drift_by_target: &HashMap<String, bool>,
    mount_count_by_target: &HashMap<String, usize>,
) {
    let rows = match mounts::endpoint_db::list() {
        Ok(rows) => rows,
        Err(e) => {
            warn!("[converge] could not read mounts to persist health: {e}");
            return;
        }
    };
    for mut row in rows {
        if row.host != this_host {
            continue;
        }
        let health = health_by_target
            .get(&row.target)
            .copied()
            .unwrap_or(Health::Missing);
        let active_route = active_by_target.get(&row.target).cloned();
        let active_options = active_options_by_target.get(&row.target).cloned();
        let drift = drift_by_target.get(&row.target).copied().unwrap_or(false);
        let multi_mounted = mount_count_by_target
            .get(&row.target)
            .is_some_and(|&n| n > 1);
        if row.health == health
            && row.active_route == active_route
            && row.active_options == active_options
            && row.drift == drift
            && row.multi_mounted == multi_mounted
        {
            continue; // no change — skip the write (and its LWW clock bump)
        }
        // Surface a newly-observed stacked target: the write path blocks authoring
        // one, but a target can still end up mounted more than once out-of-band —
        // tolerate it (record + warn), leaving the unwind to the operator.
        if multi_mounted && !row.multi_mounted {
            warn!(
                "[converge] target {} is mounted more than once ({} stacked) — \
                 unwind with storage.mount.update action=unmount",
                row.target,
                mount_count_by_target.get(&row.target).copied().unwrap_or(0)
            );
        }
        row.health = health;
        row.active_route = active_route;
        row.active_options = active_options;
        row.drift = drift;
        row.multi_mounted = multi_mounted;
        if let Err(e) = mounts::endpoint_db::update(&row) {
            warn!(
                "[converge] could not persist health for {}: {e}",
                row.target
            );
        }
    }
}

/// Read the local host's remediation policy for this tick. On a DB error the
/// loop must keep running, so fall back to the conservative default
/// ([`RemediationPolicy::Notify`] — never auto-act).
fn remediation_policy() -> RemediationPolicy {
    match db::open_default() {
        Ok(conn) => remediation::policy(&conn).unwrap_or_default(),
        Err(e) => {
            warn!("[converge] could not read remediation policy ({e}); defaulting to notify");
            RemediationPolicy::default()
        }
    }
}

/// Raise a dismissable notification carrying a completed remediation. Re-raising
/// the same `key` is an idempotent upsert, so a still-unresolved condition
/// surfaces as a single row rather than spamming. Best-effort: a DB/notify error
/// must never fail the convergence tick.
fn raise_notification(
    key: String,
    severity: Severity,
    actionable: bool,
    title: String,
    body: String,
    fix: Option<Fix>,
) {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            warn!("[converge] notify: open db: {e}");
            return;
        }
    };
    let input = RaiseInput {
        key,
        source: NOTIFY_SOURCE.to_string(),
        source_ref: None,
        severity,
        actionable,
        fix,
        title,
        body: Some(body),
        user_id: None,
    };
    if let Err(e) = db::notifications_store::raise(&conn, input, utils::time::now().unix_millis()) {
        warn!("[converge] notify: raise: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(target: &str) -> DesiredMount {
        d_with(target, RemountPolicy::default())
    }
    fn d_with(target: &str, remount_policy: RemountPolicy) -> DesiredMount {
        let sources = vec!["10.0.0.1:/e".to_string(), "10.0.0.2:/e".to_string()];
        let routes: Vec<Route> = sources
            .iter()
            .map(|s| {
                let (host, path) = s.split_once(':').unwrap();
                Route {
                    path: Some(path.to_string()),
                    ..Route::new("lan_v4", "nfs", host, Some(2049))
                }
            })
            .collect();
        DesiredMount {
            target: target.to_string(),
            backend: "nfs".to_string(),
            fstype: "nfs4".to_string(),
            sources,
            routes,
            remount_policy,
            replication: None,
            options: "vers=4.2,soft".to_string(),
            credential: None,
        }
    }
    /// A desired mount bound to a replication relationship (ref id), for
    /// exercising the failover-safety gate.
    fn d_repl(target: &str, remount_policy: RemountPolicy) -> DesiredMount {
        DesiredMount {
            replication: Some("rep-0000".to_string()),
            ..d_with(target, remount_policy)
        }
    }
    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    fn no_failover() -> HashMap<String, FailoverSignal> {
        HashMap::new()
    }
    fn no_drift() -> HashSet<String> {
        HashSet::new()
    }
    fn elected(source: &str, index: usize) -> Election {
        Election::Elected {
            source: source.to_string(),
            index,
        }
    }

    #[test]
    fn bare_dir_absent_from_table_classifies_missing_despite_stat_ok() {
        // THE regression: a desired target that exists only as an empty mountpoint
        // dir `stat`s clean → `probe_health` returns `Health::Ok`, but it is absent
        // from the kernel mount table. It MUST classify Missing so `plan()` mounts
        // it — the false positive that made convergence a silent no-op fleet-wide.
        assert_eq!(classify(true, Health::Ok), Presence::Missing);
    }

    #[test]
    fn classify_uses_health_when_present_in_table() {
        assert_eq!(classify(false, Health::Ok), Presence::Healthy);
        assert_eq!(classify(false, Health::Missing), Presence::Missing);
        assert_eq!(classify(false, Health::Stale), Presence::Stale);
        assert_eq!(classify(false, Health::Timeout), Presence::Stale);
        assert_eq!(classify(false, Health::Error), Presence::Stale);
        // Absence from the table always wins, regardless of the stat health.
        assert_eq!(classify(true, Health::Stale), Presence::Missing);
    }

    #[test]
    fn missing_desired_is_mounted() {
        let out = plan(
            &[d("/mnt/data")],
            &set(&[]),
            &set(&[]),
            &no_failover(),
            &no_drift(),
        );
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
            &no_failover(),
            &no_drift(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn stale_desired_is_unmounted_then_remounted_in_order() {
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&[]),
            &no_failover(),
            &no_drift(),
        );
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
            &no_failover(),
            &no_drift(),
        );
        assert_eq!(
            out,
            vec![Action::Unmount {
                target: "/mnt/old".into()
            }]
        );
    }

    // ── failover / fail-back / held / Safe-busy swaps ─────────────────────

    fn signal(elected: Election, active: &str, busy: bool) -> HashMap<String, FailoverSignal> {
        signal_repl(elected, active, busy, None)
    }
    /// Like [`signal`] but with an explicit observed replication health, for the
    /// failover-safety gate tests.
    fn signal_repl(
        elected: Election,
        active: &str,
        busy: bool,
        replication_healthy: Option<bool>,
    ) -> HashMap<String, FailoverSignal> {
        let mut m = HashMap::new();
        m.insert(
            "/mnt/data".to_string(),
            FailoverSignal {
                elected,
                active: Some(active.to_string()),
                busy,
                replication_healthy,
            },
        );
        m
    }

    #[test]
    fn healthy_failback_to_primary_swaps() {
        // Mounted on the secondary (idx 1), primary (idx 0) elected → fail-back.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", false),
            &no_drift(),
        );
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
    fn healthy_degrade_to_secondary_swaps() {
        // On the primary, only the secondary is elected (primary down) → degrade.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.2:/e", 1), "10.0.0.1:/e", false),
            &no_drift(),
        );
        assert_eq!(out.len(), 2, "degrade remounts: {out:?}");
    }

    // ── failover-safety gate (replication) ───────────────────────────────
    // Base scenario for all four: a fail-back swap (mounted on secondary, primary
    // elected) that WOULD swap ungated. The gate only permits it when the share's
    // replication relationship is confirmed healthy.

    #[test]
    fn replication_healthy_permits_swap() {
        let out = plan(
            &[d_repl("/mnt/data", RemountPolicy::default())],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal_repl(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", false, Some(true)),
            &no_drift(),
        );
        assert_eq!(
            out.len(),
            2,
            "healthy replication permits the swap: {out:?}"
        );
    }

    #[test]
    fn replication_unknown_holds_swap() {
        // No provider / unresolved status → hold on the current source.
        let out = plan(
            &[d_repl("/mnt/data", RemountPolicy::default())],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal_repl(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", false, None),
            &no_drift(),
        );
        assert!(
            out.is_empty(),
            "unknown replication holds the swap: {out:?}"
        );
    }

    #[test]
    fn replication_unhealthy_holds_swap() {
        let out = plan(
            &[d_repl("/mnt/data", RemountPolicy::default())],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal_repl(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", false, Some(false)),
            &no_drift(),
        );
        assert!(
            out.is_empty(),
            "unhealthy replication holds the swap: {out:?}"
        );
    }

    #[test]
    fn no_replication_ref_ignores_health_and_swaps() {
        // A share with no relationship is ungated: even an (irrelevant) unhealthy
        // reading never blocks its swap — unchanged pre-gate behaviour.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal_repl(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", false, Some(false)),
            &no_drift(),
        );
        assert_eq!(
            out.len(),
            2,
            "no ref → gate bypassed, swap proceeds: {out:?}"
        );
    }

    #[test]
    fn failback_held_when_return_to_primary_false() {
        let pol = RemountPolicy {
            failover: plugin_toolkit::storage::Failover {
                return_to_primary: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let out = plan(
            &[d_with("/mnt/data", pol)],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", false),
            &no_drift(),
        );
        assert!(out.is_empty(), "must stay degraded: {out:?}");
    }

    #[test]
    fn failover_disabled_never_swaps() {
        let pol = RemountPolicy {
            failover: plugin_toolkit::storage::Failover {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let out = plan(
            &[d_with("/mnt/data", pol)],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.2:/e", 1), "10.0.0.1:/e", false),
            &no_drift(),
        );
        assert!(out.is_empty(), "pinned mount never swaps: {out:?}");
    }

    #[test]
    fn safe_aggression_busy_mount_is_not_disrupted() {
        // Default Safe policy + busy mount → pending, no swap this tick.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", true),
            &no_drift(),
        );
        assert!(out.is_empty(), "Safe never disrupts a busy mount: {out:?}");
    }

    #[test]
    fn force_aggression_swaps_even_when_busy() {
        let pol = RemountPolicy {
            aggression: RemountAggression::Force,
            ..Default::default()
        };
        let out = plan(
            &[d_with("/mnt/data", pol)],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", true),
            &no_drift(),
        );
        assert_eq!(out.len(), 2, "Force swaps a busy mount: {out:?}");
    }

    #[test]
    fn keep_stale_counter_key_preserves_failover_streak() {
        let desired = vec![d("/mnt/data")];
        // The failover streak's own key must survive the stale-streak prune …
        assert!(keep_stale_counter_key("failover:/mnt/data", &desired));
        // … even for a target that is no longer desired (the failover block owns
        // and prunes its own keys — the stale prune must not touch them).
        assert!(keep_stale_counter_key("failover:/mnt/gone", &desired));
        // A bare key for a desired target is kept; one for a gone target is pruned.
        assert!(keep_stale_counter_key("/mnt/data", &desired));
        assert!(!keep_stale_counter_key("/mnt/gone", &desired));
    }

    #[test]
    fn failover_confirm_streak_accrues_across_ticks_despite_stale_prune() {
        // Regression: the stale-streak prune and the failover-streak accrual share
        // one counters map. Before the fix, the stale prune evicted `failover:*`
        // keys every tick, resetting the failover streak so a healthy mount on a
        // held source never reached `confirm_ticks` — a drained source "failed to
        // release" its clients. Simulate the two-phase per-tick sequence and assert
        // the failover streak now survives the prune and accrues to confirm.
        use std::collections::HashMap;
        let desired = vec![d("/mnt/data")];
        let key = "failover:/mnt/data".to_string();
        let confirm_ticks = 2u32;
        let mut counters: HashMap<String, u32> = HashMap::new();
        let mut confirmed_on_tick = None;
        for tick in 1..=3 {
            // Phase A: stale-streak prune (runs first each tick).
            counters.retain(|t, _| keep_stale_counter_key(t, &desired));
            // Phase B: failover-streak accrual for a persistently swap-worthy target.
            let n = counters.entry(key.clone()).or_insert(0);
            *n += 1;
            if *n >= confirm_ticks && confirmed_on_tick.is_none() {
                confirmed_on_tick = Some(tick);
            }
        }
        assert_eq!(
            confirmed_on_tick,
            Some(2),
            "failover streak must confirm on the 2nd consecutive swap-worthy tick"
        );
    }

    #[test]
    fn mounted_on_held_source_is_swapped_to_enabled_election() {
        // Active source is not among the enabled routes (it was held/drained);
        // election picks an enabled source → Transition::Mount → swap.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.1:/e", 0), "held:/e", false),
            &no_drift(),
        );
        assert_eq!(
            out.len(),
            2,
            "un-hold remounts onto an enabled source: {out:?}"
        );
    }

    #[test]
    fn healthy_on_elected_source_is_unchanged() {
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.1:/e", 0), "10.0.0.1:/e", false),
            &no_drift(),
        );
        assert!(out.is_empty(), "already on the elected source: {out:?}");
    }

    #[test]
    fn mount_req_uses_elected_source_and_rendered_options() {
        let req = mount_req(&d("/mnt/data"), "10.0.0.2:/e", None);
        assert_eq!(req.source, "10.0.0.2:/e");
        assert_eq!(req.target, "/mnt/data");
        assert_eq!(req.fstype, "nfs4");
        assert_eq!(req.options, "vers=4.2,soft");
        assert!(req.secret_file.is_none());
    }

    #[test]
    fn source_of_route_renders_nfs_and_smb_shapes() {
        let nfs = Route {
            path: Some("/export/pool".into()),
            ..Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049))
        };
        assert_eq!(source_of_route("nfs4", &nfs), "10.0.0.1:/export/pool");
        let smb = Route {
            path: Some("/media".into()),
            ..Route::new("lan_v4", "cifs", "server", Some(445))
        };
        assert_eq!(source_of_route("cifs", &smb), "//server/media");
        // No path → the bare value (an already-whole source).
        let bare = Route::new("lan_v4", "nfs", "10.0.0.9", Some(2049));
        assert_eq!(source_of_route("nfs4", &bare), "10.0.0.9");
    }

    #[test]
    fn structured_route_round_trips_through_source_of_route() {
        // BUG A regression: a structured/shorthand NFS route must keep its export
        // path so source_of_route can render the whole `host:/export` source. A
        // dropped path rendered just the bare host and broke every mount.
        let r = plugin_toolkit::route::parse_route("lan_v4=nfs://10.10.10.10:2049/mnt/user/data")
            .unwrap();
        assert_eq!(source_of_route("nfs4", &r), "10.10.10.10:/mnt/user/data");
    }

    // ── non-stacking mount decision ──────────────────────────────────────

    #[test]
    fn mount_execution_proceeds_on_empty_target() {
        let desired = vec!["10.0.0.1:/e".to_string(), "10.0.0.2:/e".to_string()];
        assert_eq!(
            mount_execution("10.0.0.1:/e", &desired, None),
            MountExec::Proceed
        );
    }

    #[test]
    fn mount_execution_adopts_when_desired_source_already_mounted() {
        let desired = vec!["10.0.0.1:/e".to_string(), "10.0.0.2:/e".to_string()];
        // Already on the elected source → adopt.
        assert_eq!(
            mount_execution("10.0.0.1:/e", &desired, Some("10.0.0.1:/e")),
            MountExec::Adopt
        );
        // Already on a different-but-desired source (e.g. degraded onto the
        // secondary) → adopt too; re-pointing is the swap path, not a bare mount.
        assert_eq!(
            mount_execution("10.0.0.1:/e", &desired, Some("10.0.0.2:/e")),
            MountExec::Adopt
        );
    }

    #[test]
    fn mount_execution_refuses_to_stack_on_foreign_source() {
        // maple stacked under willow, live: the target already carries a source
        // outside the desired set → never stack, surface it as Foreign.
        let desired = vec!["willow:/e".to_string()];
        assert_eq!(
            mount_execution("willow:/e", &desired, Some("maple:/e")),
            MountExec::Foreign("maple:/e".to_string())
        );
    }

    #[test]
    fn orphan_targets_are_ledger_minus_desired() {
        let ledger = set(&["/mnt/data", "/mnt/old", "/mnt/gone"]);
        let desired = set(&["/mnt/data"]);
        let mut orphans = orphan_targets(&ledger, &desired);
        orphans.sort();
        assert_eq!(
            orphans,
            vec!["/mnt/gone".to_string(), "/mnt/old".to_string()]
        );
    }

    #[test]
    fn orphan_targets_empty_when_all_desired() {
        let ledger = set(&["/mnt/data", "/mnt/media"]);
        let desired = set(&["/mnt/data", "/mnt/media"]);
        assert!(orphan_targets(&ledger, &desired).is_empty());
    }

    // ── option-drift comparator (real /proc/mounts strings) ──────────────

    /// Split a real `/proc/mounts` option field into the live token vec the
    /// comparator consumes.
    fn live(opts: &str) -> Vec<String> {
        opts.split(',').map(str::to_string).collect()
    }

    #[test]
    fn options_drifted_hard_live_vs_soft_desired_is_drift() {
        // THE primary case: an operator flips the share to `soft` but the live
        // mount is still `hard`. The live string is a verbatim njord/willow NFSv4
        // `/proc/mounts` line — reordered, kernel-injected, and with `actimeo`
        // already expanded to `acregmin/acregmax/acdirmax`.
        let live = live(
            "rw,relatime,vers=4.2,rsize=1048576,wsize=1048576,namlen=255,acregmin=30,\
             acregmax=30,acdirmax=30,hard,proto=tcp,nconnect=4,timeo=150,retrans=3,sec=sys,\
             clientaddr=10.10.10.6,local_lock=none,addr=10.10.10.10",
        );
        let desired = "vers=4.2,soft,softreval,timeo=150,retrans=3,nconnect=4,actimeo=30";
        assert!(options_drifted(desired, &live), "hard live vs soft desired");
    }

    #[test]
    fn options_drifted_soft_live_matching_desired_is_no_drift() {
        // The SAME desired against a soft live mount that already carries the
        // operator's intent (soft + softreval + matching keyed tunables). The
        // kernel-injected/renamed noise (acregmin.., addr, rsize, sec, proto) and
        // the un-renamed `actimeo` must NOT false-positive.
        let live = live(
            "rw,relatime,vers=4.2,rsize=1048576,wsize=1048576,namlen=255,acregmin=30,\
             acregmax=30,acdirmax=30,soft,softreval,proto=tcp,nconnect=4,timeo=150,retrans=3,\
             sec=sys,clientaddr=10.10.10.6,local_lock=none,addr=10.10.10.10",
        );
        let desired = "vers=4.2,soft,softreval,timeo=150,retrans=3,nconnect=4,actimeo=30";
        assert!(
            !options_drifted(desired, &live),
            "soft live already matches desired — no drift"
        );
    }

    #[test]
    fn options_drifted_on_keyed_tunable_changes() {
        // Same hardness, but a changed `timeo` and a changed `nconnect` each drift.
        let base =
            "vers=4.2,soft,softreval,proto=tcp,nconnect=4,timeo=150,retrans=3,addr=10.10.10.10";
        let desired = "vers=4.2,soft,softreval,timeo=150,retrans=3,nconnect=4";
        assert!(
            !options_drifted(desired, &live(base)),
            "identical keyed set"
        );
        assert!(
            options_drifted(desired, &live(&base.replace("timeo=150", "timeo=600"))),
            "timeo change is drift"
        );
        assert!(
            options_drifted(desired, &live(&base.replace("nconnect=4", "nconnect=1"))),
            "nconnect change is drift"
        );
        assert!(
            options_drifted(desired, &live(&base.replace("vers=4.2", "vers=4.1"))),
            "vers change is drift"
        );
    }

    #[test]
    fn options_drifted_ignores_softreval_when_neither_pins_it() {
        // Desired omits softreval and the mount is soft without it → no drift,
        // and kernel noise is ignored.
        let live =
            live("rw,vers=4.2,soft,proto=tcp,timeo=150,retrans=3,nconnect=4,addr=10.10.10.10");
        let desired = "vers=4.2,soft,timeo=150,retrans=3,nconnect=4";
        assert!(!options_drifted(desired, &live));
    }

    #[test]
    fn option_drift_target_is_unmounted_then_remounted() {
        // A healthy mount on its elected source, flagged for an option-drift
        // remount, becomes a lazy Unmount → Mount pair.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &no_failover(),
            &set(&["/mnt/data"]),
        );
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

    // ── source normalization + drift-remount decision ────────────────────

    #[test]
    fn same_source_tolerates_trailing_slash_and_host_case() {
        // The cosmetic differences a legacy autofs mount / kernel table echoes:
        // a trailing slash on the export and host case — same server+export.
        assert!(same_source(
            "willow:/mnt/user/data",
            "willow:/mnt/user/data/"
        ));
        assert!(same_source(
            "WILLOW:/mnt/user/data",
            "willow:/mnt/user/data"
        ));
        assert!(same_source("//server/media", "//SERVER/media/"));
        // Genuine differences must NOT collapse: a different host (IP vs name is a
        // real source election decision) and a case-sensitive export path.
        assert!(!same_source(
            "10.10.10.10:/mnt/user/data",
            "willow:/mnt/user/data"
        ));
        assert!(!same_source(
            "willow:/mnt/user/Data",
            "willow:/mnt/user/data"
        ));
        assert!(!same_source("willow:/mnt/user/a", "willow:/mnt/user/b"));
    }

    #[test]
    fn drift_idle_on_elected_source_remounts_despite_trailing_slash() {
        // THE Task-C regression: a genuinely-drifted, IDLE mount sitting on its
        // elected source, where the kernel-active string differs only by a trailing
        // slash, must schedule a remount — not silently skip. Under both Safe and
        // Force (idle ⇒ no Safe defer).
        assert_eq!(
            drift_action(
                Some("willow:/mnt/user/data"),
                Some("willow:/mnt/user/data/"),
                RemountAggression::Safe,
                false,
            ),
            DriftDecision::Remount,
        );
        assert_eq!(
            drift_action(
                Some("willow:/mnt/user/data"),
                Some("willow:/mnt/user/data"),
                RemountAggression::Force,
                false,
            ),
            DriftDecision::Remount,
        );
    }

    #[test]
    fn drift_busy_under_safe_defers_not_remounts() {
        // The Plex/Jellyfin guarantee: a BUSY mount under Safe defers, never
        // remounts this tick. Under Force the same busy mount remounts.
        assert_eq!(
            drift_action(
                Some("willow:/e"),
                Some("willow:/e"),
                RemountAggression::Safe,
                true,
            ),
            DriftDecision::DeferBusySafe,
        );
        assert_eq!(
            drift_action(
                Some("willow:/e"),
                Some("willow:/e"),
                RemountAggression::Force,
                true,
            ),
            DriftDecision::Remount,
        );
    }

    #[test]
    fn drift_off_elected_or_no_election_does_not_remount() {
        // On a genuinely different source → leave it to the failover/pin path.
        assert_eq!(
            drift_action(
                Some("willow:/e"),
                Some("maple:/e"),
                RemountAggression::Force,
                false,
            ),
            DriftDecision::NotOnElected,
        );
        // No live elected source → nothing to remount onto.
        assert_eq!(
            drift_action(None, Some("willow:/e"), RemountAggression::Force, false),
            DriftDecision::NoElection,
        );
        // Nothing mounted at all → not on the elected source.
        assert_eq!(
            drift_action(Some("willow:/e"), None, RemountAggression::Force, false),
            DriftDecision::NotOnElected,
        );
    }

    #[test]
    fn ledger_round_trips_through_disk() {
        // Unique path per test process; absent file loads as empty.
        let path =
            std::env::temp_dir().join(format!("orca-ledger-test-{}.json", std::process::id()));
        std::fs::remove_file(&path).ok();
        assert!(
            load_ledger_at(&path).is_empty(),
            "absent ledger must be empty"
        );

        let want = set(&["/mnt/a", "/mnt/b"]);
        save_ledger_at(&path, &want).expect("save ledger");
        assert_eq!(load_ledger_at(&path), want);

        // Overwrite (atomic rename) with a smaller set.
        let want2 = set(&["/mnt/a"]);
        save_ledger_at(&path, &want2).expect("save ledger 2");
        assert_eq!(load_ledger_at(&path), want2);

        std::fs::remove_file(&path).ok();
    }

    // ── host_of_source (probe-aim parser) ────────────────────────────────

    #[test]
    fn host_of_source_parses_nfs_and_smb_shapes() {
        // NFS `host:/export` → host.
        assert_eq!(
            host_of_source("willow:/mnt/user/data").as_deref(),
            Some("willow")
        );
        assert_eq!(
            host_of_source("10.10.10.10:/e").as_deref(),
            Some("10.10.10.10")
        );
        // SMB `//server/share` → server, ignoring the share path.
        assert_eq!(host_of_source("//server/media").as_deref(), Some("server"));
        // SMB with an embedded `user@server` authority → just the host.
        assert_eq!(
            host_of_source("//user@server/media").as_deref(),
            Some("server")
        );
        // SMB with only the authority, no share path.
        assert_eq!(host_of_source("//server").as_deref(), Some("server"));
    }

    #[test]
    fn host_of_source_rejects_shapes_without_a_host() {
        // A bare token with no `:` and no `//` prefix has no discernible host.
        assert_eq!(host_of_source("noscheme"), None);
        // Empty authority (SMB) and empty host (NFS) both yield None.
        assert_eq!(host_of_source("///share"), None);
        assert_eq!(host_of_source(":/export"), None);
        assert_eq!(host_of_source(""), None);
    }

    // ── norm_source direct branches ──────────────────────────────────────

    #[test]
    fn norm_source_normalizes_each_shape() {
        // NFS: host lowercased, trailing slash trimmed, path case preserved.
        assert_eq!(norm_source("WILLOW:/mnt/User/"), "willow:/mnt/User");
        // SMB with a share path: host lowercased, trailing slash trimmed.
        assert_eq!(norm_source("//SERVER/Media/"), "//server/Media");
        // SMB with no share path (no inner slash) → authority-only branch.
        assert_eq!(norm_source("//SERVER"), "//server");
        // Bare source (no `//`, no `:`) → lowercased + trimmed fallback branch.
        assert_eq!(norm_source("HOST/"), "host");
    }

    // ── elected_source ───────────────────────────────────────────────────

    #[test]
    fn elected_source_maps_election_to_optional_string() {
        assert_eq!(
            elected_source(&elected("10.0.0.1:/e", 0)).as_deref(),
            Some("10.0.0.1:/e")
        );
        assert_eq!(elected_source(&Election::Empty), None);
    }

    // ── mount_req secret-file passthrough ────────────────────────────────

    #[test]
    fn mount_req_carries_secret_file_when_present() {
        let sf = crate::mount_exec::SecretFile {
            path: "/run/orca/secret".to_string(),
            contents: "user=alice".to_string(),
        };
        let req = mount_req(&d("/mnt/data"), "10.0.0.1:/e", Some(sf));
        let got = req.secret_file.expect("secret file present");
        assert_eq!(got.path, "/run/orca/secret");
        assert_eq!(got.contents, "user=alice");
    }

    // ── options_drifted extra branches ───────────────────────────────────

    #[test]
    fn options_drifted_soft_desired_vs_hard_live_symmetric() {
        // The reverse of the primary case: desired pins `hard`, live is `soft`.
        let live = live("rw,vers=4.2,soft,proto=tcp,timeo=150,addr=10.10.10.10");
        assert!(options_drifted("vers=4.2,hard,timeo=150", &live));
    }

    #[test]
    fn options_drifted_no_hardness_pin_is_no_drift() {
        // Desired pins neither hard nor soft → the hardness pair is not compared,
        // and with no other divergence there is no drift.
        let live = live("rw,vers=4.2,hard,proto=tcp,timeo=150,addr=10.10.10.10");
        assert!(!options_drifted("vers=4.2,timeo=150", &live));
    }

    #[test]
    fn options_drifted_softreval_desired_missing_live_is_drift() {
        // Desired pins softreval; the live mount lacks it → drift.
        let live = live("rw,vers=4.2,soft,proto=tcp,timeo=150,addr=10.10.10.10");
        assert!(options_drifted("vers=4.2,soft,softreval,timeo=150", &live));
    }

    #[test]
    fn options_drifted_keyed_absent_from_live_is_drift() {
        // Desired pins `nconnect=4` but the live mount carries no nconnect at all.
        let live = live("rw,vers=4.2,soft,proto=tcp,timeo=150,addr=10.10.10.10");
        assert!(options_drifted("vers=4.2,soft,nconnect=4", &live));
    }

    #[test]
    fn options_drifted_empty_desired_never_drifts() {
        // An empty desired string pins nothing → whatever the kernel echoes is fine.
        let live = live("rw,vers=4.2,hard,proto=tcp,timeo=600,addr=10.10.10.10");
        assert!(!options_drifted("", &live));
        // Whitespace-only tokens are filtered out too.
        assert!(!options_drifted(" , , ", &live));
    }

    // ── plan across multiple desired mounts ──────────────────────────────

    #[test]
    fn plan_handles_mixed_desired_set_in_order() {
        // /mnt/a missing → Mount; /mnt/b healthy → nothing; /mnt/c stale →
        // Unmount+Mount; /mnt/stray mounted but undesired → trailing Unmount.
        let out = plan(
            &[d("/mnt/a"), d("/mnt/b"), d("/mnt/c")],
            &set(&["/mnt/b", "/mnt/c", "/mnt/stray"]),
            &set(&["/mnt/b"]),
            &no_failover(),
            &no_drift(),
        );
        assert_eq!(
            out,
            vec![
                Action::Mount {
                    target: "/mnt/a".into()
                },
                Action::Unmount {
                    target: "/mnt/c".into()
                },
                Action::Mount {
                    target: "/mnt/c".into()
                },
                Action::Unmount {
                    target: "/mnt/stray".into()
                },
            ]
        );
    }

    #[test]
    fn plan_empty_desired_releases_all_mounted() {
        let out = plan(
            &[],
            &set(&["/mnt/gone"]),
            &set(&[]),
            &no_failover(),
            &no_drift(),
        );
        assert_eq!(
            out,
            vec![Action::Unmount {
                target: "/mnt/gone".into()
            }]
        );
    }

    // ── ledger tolerance of corrupt state ────────────────────────────────

    #[test]
    fn load_ledger_at_garbage_bytes_reads_as_empty() {
        let path =
            std::env::temp_dir().join(format!("orca-ledger-garbage-{}.json", std::process::id()));
        std::fs::write(&path, b"not json at all").expect("write garbage");
        assert!(
            load_ledger_at(&path).is_empty(),
            "unparseable ledger must degrade to empty, not panic"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_ledger_at_creates_missing_parent_dir() {
        let dir = std::env::temp_dir().join(format!("orca-ledger-nested-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("sub").join("managed_mounts.json");
        let want = set(&["/mnt/x"]);
        save_ledger_at(&path, &want).expect("save creates parent dirs");
        assert_eq!(load_ledger_at(&path), want);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── should_swap: unchanged / empty-target transitions hold ───────────

    #[test]
    fn should_swap_holds_on_unchanged_and_empty_election() {
        // Already on the elected primary → Unchanged transition → no swap.
        let dm = d("/mnt/data");
        let unchanged = FailoverSignal {
            elected: elected("10.0.0.1:/e", 0),
            active: Some("10.0.0.1:/e".to_string()),
            busy: false,
            replication_healthy: None,
        };
        assert!(!should_swap(&dm, &unchanged));
        // Empty election (every source down) → nothing to swap onto.
        let empty = FailoverSignal {
            elected: Election::Empty,
            active: Some("10.0.0.1:/e".to_string()),
            busy: false,
            replication_healthy: None,
        };
        assert!(!should_swap(&dm, &empty));
    }

    // ── should_swap: Transition::Mount active-presence gate ──────────────

    #[test]
    fn should_swap_mount_transition_with_no_active_holds() {
        // Elected a source but NOTHING is currently mounted (active = None):
        // source_election yields Transition::Mount, and the `Mount { .. } =>
        // sig.active.is_some()` guard makes should_swap decline — a swap only
        // re-points an existing mount; a bare mount is the plan()'s missing path.
        let dm = d("/mnt/data");
        let sig = FailoverSignal {
            elected: elected("10.0.0.1:/e", 0),
            active: None,
            busy: false,
            replication_healthy: None,
        };
        assert!(!should_swap(&dm, &sig));
    }

    #[test]
    fn should_swap_mount_transition_off_held_source_swaps() {
        // Mounted on a source absent from the enabled route set (held/drained):
        // transition is Mount with active present, so should_swap permits the
        // re-point onto the elected enabled source.
        let dm = d("/mnt/data");
        let sig = FailoverSignal {
            elected: elected("10.0.0.1:/e", 0),
            active: Some("held:/e".to_string()),
            busy: false,
            replication_healthy: None,
        };
        assert!(should_swap(&dm, &sig));
    }

    // ── should_swap: replication gate combined with Safe/busy ────────────

    #[test]
    fn should_swap_replication_healthy_but_safe_busy_still_holds() {
        // The gate passes (replication healthy) yet the Safe+busy guarantee is
        // evaluated AFTER it — a busy mount under Safe is never force-swapped.
        let dm = d_repl("/mnt/data", RemountPolicy::default());
        let sig = FailoverSignal {
            elected: elected("10.0.0.1:/e", 0),
            active: Some("10.0.0.2:/e".to_string()),
            busy: true,
            replication_healthy: Some(true),
        };
        assert!(!should_swap(&dm, &sig));
    }

    #[test]
    fn should_swap_replication_healthy_and_idle_swaps() {
        let dm = d_repl("/mnt/data", RemountPolicy::default());
        let sig = FailoverSignal {
            elected: elected("10.0.0.1:/e", 0),
            active: Some("10.0.0.2:/e".to_string()),
            busy: false,
            replication_healthy: Some(true),
        };
        assert!(should_swap(&dm, &sig));
    }

    #[test]
    fn should_swap_disabled_short_circuits_before_transition() {
        // failover.enabled = false returns immediately, regardless of a
        // swap-worthy transition and confirmed-healthy replication.
        let pol = RemountPolicy {
            failover: plugin_toolkit::storage::Failover {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let dm = d_with("/mnt/data", pol);
        let sig = FailoverSignal {
            elected: elected("10.0.0.1:/e", 0),
            active: Some("10.0.0.2:/e".to_string()),
            busy: false,
            replication_healthy: Some(true),
        };
        assert!(!should_swap(&dm, &sig));
    }

    // ── source_of_route: SMB with no path ────────────────────────────────

    #[test]
    fn source_of_route_smb_without_path_is_bare_value() {
        // A cifs route with no export path degrades to the bare value (the
        // None arm), NOT a `//server` rendering.
        let bare = Route::new("lan_v4", "cifs", "server", Some(445));
        assert_eq!(source_of_route("cifs", &bare), "server");
    }

    #[test]
    fn source_of_route_non_nfs_backend_uses_smb_shape() {
        // Any non-`nfs*` fstype with a path renders the `//value/path` SMB shape.
        let r = Route {
            path: Some("/vol".into()),
            ..Route::new("lan_v4", "cifs", "host", Some(445))
        };
        assert_eq!(source_of_route("smb3", &r), "//host/vol");
        // `nfs`-prefixed variants take the NFS shape.
        assert_eq!(source_of_route("nfs", &r), "host:/vol");
    }

    // ── mount_execution: extra adopt/foreign branches ────────────────────

    #[test]
    fn mount_execution_adopts_when_active_equals_elected_outside_desired() {
        // The elected source is matched by the first `a == elected` arm even when
        // it is not among the ordered `desired` sources.
        let desired = vec!["10.0.0.9:/e".to_string()];
        assert_eq!(
            mount_execution("10.0.0.1:/e", &desired, Some("10.0.0.1:/e")),
            MountExec::Adopt
        );
    }

    #[test]
    fn mount_execution_empty_desired_with_occupant_is_foreign() {
        // No desired sources and an occupant that is not the elected source → the
        // occupant is foreign and must never be stacked upon.
        assert_eq!(
            mount_execution("10.0.0.1:/e", &[], Some("maple:/e")),
            MountExec::Foreign("maple:/e".to_string())
        );
    }

    // ── host_of_source: further shapes ───────────────────────────────────

    #[test]
    fn host_of_source_smb_trailing_at_yields_empty_none() {
        // `//server@/x` → authority `server@`, rsplit('@') last is empty → None.
        assert_eq!(host_of_source("//server@/x"), None);
    }

    #[test]
    fn host_of_source_nfs_trims_whitespace_host() {
        assert_eq!(
            host_of_source("  willow  :/export").as_deref(),
            Some("willow")
        );
        // Whitespace-only NFS host → None.
        assert_eq!(host_of_source("   :/export"), None);
    }

    // ── norm_source: bare NFS/SMB equivalence edges ──────────────────────

    #[test]
    fn norm_source_smb_authority_only_trailing_slash() {
        // `//SERVER/` has an inner slash with an empty path → host lowered, empty
        // trimmed path.
        assert_eq!(norm_source("//SERVER/"), "//server/");
    }

    #[test]
    fn same_source_bare_tokens_case_and_slash_insensitive() {
        // The bare fallback branch (no `//`, no `:`) lowercases and trims.
        assert!(same_source("HOST/", "host"));
        assert!(!same_source("host-a", "host-b"));
    }

    // ── options_drifted: live carries neither hard nor soft ──────────────

    #[test]
    fn options_drifted_desired_soft_but_live_has_no_hardness_no_drift() {
        // If the live mount echoes neither `hard` nor `soft`, the hardness pair
        // is not compared (l = None) → no drift from that axis.
        let live = live("rw,vers=4.2,proto=tcp,timeo=150,addr=10.10.10.10");
        assert!(!options_drifted("vers=4.2,soft", &live));
    }

    #[test]
    fn options_drifted_live_soft_desired_hard_but_no_common_keys() {
        // Desired `hard`, live `soft` → hardness axis drifts even with no keyed
        // tunables in common.
        let live = live("rw,soft,proto=tcp");
        assert!(options_drifted("hard", &live));
    }

    #[test]
    fn options_drifted_matching_hardness_and_keyed_no_drift() {
        // Same hardness, and every desired keyed tunable is matched verbatim.
        let live = live("rw,soft,vers=4.2,timeo=150,retrans=3,nconnect=4,addr=1.1.1.1");
        assert!(!options_drifted(
            "soft,vers=4.2,timeo=150,retrans=3,nconnect=4",
            &live
        ));
    }

    // ── plan: precedence + drift-on-missing edges ────────────────────────

    #[test]
    fn plan_stale_takes_precedence_over_failover_signal() {
        // A stale target with a swap-worthy failover signal remounts via the
        // stale branch (Unmount+Mount) — the else-if chain never reaches the
        // failover arm, but the resulting action pair is identical.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&[]), // not healthy → stale branch
            &signal(elected("10.0.0.1:/e", 0), "10.0.0.2:/e", false),
            &no_drift(),
        );
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
    fn plan_drift_set_ignored_for_missing_target() {
        // A target flagged in option_drift but NOT mounted takes the missing
        // branch: a single Mount, no spurious Unmount.
        let out = plan(
            &[d("/mnt/data")],
            &set(&[]),
            &set(&[]),
            &no_failover(),
            &set(&["/mnt/data"]),
        );
        assert_eq!(
            out,
            vec![Action::Mount {
                target: "/mnt/data".into()
            }]
        );
    }

    #[test]
    fn plan_failover_swap_falls_through_to_drift_when_not_swapping() {
        // Healthy, on the elected source (should_swap = false), but flagged for an
        // option-drift remount → the drift branch fires the Unmount+Mount.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
            &signal(elected("10.0.0.1:/e", 0), "10.0.0.1:/e", false),
            &set(&["/mnt/data"]),
        );
        assert_eq!(out.len(), 2, "drift remount after non-swap: {out:?}");
    }

    #[test]
    fn plan_multiple_orphans_all_released() {
        // Two mounted targets with no desired placements → both released as
        // trailing Unmounts.
        let mut out = plan(
            &[],
            &set(&["/mnt/a", "/mnt/b"]),
            &set(&["/mnt/a", "/mnt/b"]),
            &no_failover(),
            &no_drift(),
        );
        out.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        assert_eq!(
            out,
            vec![
                Action::Unmount {
                    target: "/mnt/a".into()
                },
                Action::Unmount {
                    target: "/mnt/b".into()
                },
            ]
        );
    }

    // ── orphan_targets: empty ledger ─────────────────────────────────────

    #[test]
    fn orphan_targets_empty_ledger_is_empty() {
        let desired = set(&["/mnt/data"]);
        assert!(orphan_targets(&HashSet::new(), &desired).is_empty());
    }

    // ── ledger serialization: on-disk JSON shape ─────────────────────────

    #[test]
    fn save_ledger_at_writes_json_array() {
        // Assert on the serialized string (not a parsed Value): the ledger is a
        // flat JSON array of the mounted targets.
        let path =
            std::env::temp_dir().join(format!("orca-ledger-shape-{}.json", std::process::id()));
        std::fs::remove_file(&path).ok();
        save_ledger_at(&path, &set(&["/mnt/only"])).expect("save ledger");
        let raw = std::fs::read_to_string(&path).expect("read ledger");
        assert_eq!(raw, "[\"/mnt/only\"]");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_ledger_at_reads_hand_written_json_array() {
        let path =
            std::env::temp_dir().join(format!("orca-ledger-read-{}.json", std::process::id()));
        std::fs::write(&path, b"[\"/mnt/x\",\"/mnt/y\"]").expect("write ledger");
        assert_eq!(load_ledger_at(&path), set(&["/mnt/x", "/mnt/y"]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_ledger_at_empty_set_is_empty_array() {
        let path =
            std::env::temp_dir().join(format!("orca-ledger-empty-{}.json", std::process::id()));
        std::fs::remove_file(&path).ok();
        save_ledger_at(&path, &HashSet::new()).expect("save empty ledger");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
        assert!(load_ledger_at(&path).is_empty());
        std::fs::remove_file(&path).ok();
    }

    // ── elected_source / Election Debug shape ────────────────────────────

    #[test]
    fn action_equality_distinguishes_mount_and_unmount() {
        // The Action enum's derived Eq distinguishes variant and target.
        assert_ne!(
            Action::Mount {
                target: "/mnt/a".into()
            },
            Action::Unmount {
                target: "/mnt/a".into()
            }
        );
        assert_eq!(
            Action::Mount {
                target: "/mnt/a".into()
            },
            Action::Mount {
                target: "/mnt/a".into()
            }
        );
    }

    // ── DB-backed: desired_for_host + remediation_policy ───────────────────
    //
    // `with_thread_db_path` scopes a private sqlite to this test thread, so
    // these never race the rest of the suite. `open_default` runs `apply_schema`
    // and `apply_fragments` materialises the `shares`/`mounts` tables.

    fn with_db<F: FnOnce()>(name: &str, f: F) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        db::with_thread_db_path(&path, || {
            let conn = db::open_default().expect("open temp db");
            db::schema_fragments::apply_fragments(&conn).expect("apply fragments");
            drop(conn);
            f();
        });
    }

    fn insert_share(id: &str, enabled: bool, route_enabled: bool) {
        let route = Route {
            path: Some("/export".to_string()),
            enabled: route_enabled,
            ..Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049))
        };
        let row = shares::EndpointRow {
            id: id.to_string(),
            name: format!("share-{id}"),
            backend: "nfs".into(),
            fstype: "nfs4".into(),
            options: "{}".into(),
            options_rendered: "vers=4.2".into(),
            credential: None,
            replication: None,
            routes: plugin_toolkit::route::Routes::from(vec![route]),
            enabled,
        };
        shares::endpoint_db::insert(&row).expect("insert share");
    }

    fn insert_mount(id: &str, share_id: &str, host: &str, target: &str, enabled: bool) {
        let row = mounts::EndpointRow {
            guest: None,
            id: id.to_string(),
            name: format!("m-{id}"),
            share_id: share_id.to_string(),
            host: host.to_string(),
            target: target.to_string(),
            remount_policy: None,
            health: plugin_toolkit::storage::Health::Ok,
            active_route: None,
            active_options: None,
            drift: false,
            multi_mounted: false,
            enabled,
        };
        mounts::endpoint_db::insert(&row).expect("insert mount");
    }

    #[test]
    fn desired_for_host_maps_enabled_placement_to_desired_mount() {
        with_db("desired_ok.db", || {
            insert_share("sh-1", true, true);
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);
            let out = desired_for_host("h1").expect("desired ok");
            assert_eq!(out.len(), 1);
            let dm = &out[0];
            assert_eq!(dm.target, "/mnt/data");
            assert_eq!(dm.fstype, "nfs4");
            assert_eq!(dm.backend, "nfs");
            // The enabled route folds to an `host:/export` source.
            assert_eq!(dm.sources, vec!["10.0.0.1:/export".to_string()]);
        });
    }

    #[test]
    fn desired_for_host_filters_other_host_and_disabled() {
        with_db("desired_filter.db", || {
            insert_share("sh-1", true, true);
            // Wrong host.
            insert_mount("m-1", "sh-1", "other", "/mnt/a", true);
            // Disabled placement on the right host.
            insert_mount("m-2", "sh-1", "h1", "/mnt/b", false);
            let out = desired_for_host("h1").expect("desired ok");
            assert!(out.is_empty(), "no enabled placement for h1");
        });
    }

    fn insert_guest_mount(id: &str, share_id: &str, host: &str, target: &str, guest: &str) {
        let row = mounts::EndpointRow {
            guest: Some(guest.to_string()),
            id: id.to_string(),
            name: format!("m-{id}"),
            share_id: share_id.to_string(),
            host: host.to_string(),
            target: target.to_string(),
            remount_policy: None,
            health: plugin_toolkit::storage::Health::Ok,
            active_route: None,
            active_options: None,
            drift: false,
            multi_mounted: false,
            enabled: true,
        };
        mounts::endpoint_db::insert(&row).expect("insert guest mount");
    }

    #[test]
    fn guest_placements_route_to_guest_desired_not_host_mounts() {
        with_db("desired_guest_split.db", || {
            insert_share("sh-1", true, true);
            // A host mount and a guest mount on the same host, same share.
            insert_mount("m-host", "sh-1", "h1", "/mnt/data", true);
            insert_guest_mount("m-guest", "sh-1", "h1", "/mnt/data", "110");

            // Host-mount desired set EXCLUDES the guest-targeted placement.
            let host = desired_for_host("h1").expect("desired host");
            assert_eq!(host.len(), 1, "only the host mount");
            assert_eq!(host[0].target, "/mnt/data");

            // Guest desired set INCLUDES only the guest-targeted placement, joined
            // to its share's source/fstype/backend.
            let guest = desired_guest_mounts_for_host("h1").expect("desired guest");
            assert_eq!(guest.len(), 1, "only the guest mount");
            assert_eq!(guest[0].guest, "110");
            assert_eq!(guest[0].target, "/mnt/data");
            assert_eq!(guest[0].backend, "nfs");
            assert_eq!(guest[0].fstype, "nfs4");
            assert_eq!(guest[0].sources, vec!["10.0.0.1:/export".to_string()]);

            // A guest placement for a different host is not this host's concern.
            insert_guest_mount("m-other", "sh-1", "other", "/mnt/data", "200");
            let guest2 = desired_guest_mounts_for_host("h1").expect("desired guest 2");
            assert_eq!(guest2.len(), 1, "other host's guest mount excluded");
        });
    }

    #[test]
    fn desired_for_host_skips_missing_share_ref() {
        with_db("desired_missing_share.db", || {
            // Mount references a share id that does not exist.
            insert_mount("m-1", "ghost", "h1", "/mnt/data", true);
            let out = desired_for_host("h1").expect("desired ok");
            assert!(out.is_empty(), "dangling share ref is skipped");
        });
    }

    #[test]
    fn desired_for_host_skips_when_all_routes_held() {
        with_db("desired_held.db", || {
            // Share exists but its only route is held (disabled) → no sources.
            insert_share("sh-1", true, false);
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);
            let out = desired_for_host("h1").expect("desired ok");
            assert!(out.is_empty(), "all-held routes yield no desired sources");
        });
    }

    #[test]
    fn desired_for_host_skips_disabled_share() {
        with_db("desired_disabled_share.db", || {
            insert_share("sh-1", false, true);
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);
            let out = desired_for_host("h1").expect("desired ok");
            assert!(out.is_empty(), "disabled share is excluded from the map");
        });
    }

    #[test]
    fn remediation_policy_defaults_to_notify_on_fresh_db() {
        with_db("remediation.db", || {
            // A fresh db has no stored policy → the conservative Notify default.
            assert_eq!(remediation_policy(), RemediationPolicy::Notify);
        });
    }

    // ── persist_mount_state: writes observed signals onto the matching row ──

    fn map1<V>(target: &str, v: V) -> HashMap<String, V> {
        let mut m = HashMap::new();
        m.insert(target.to_string(), v);
        m
    }

    #[test]
    fn persist_mount_state_writes_observed_signals_to_matching_host_row() {
        with_db("persist_write.db", || {
            insert_share("sh-1", true, true);
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);
            persist_mount_state(
                "h1",
                &map1("/mnt/data", Health::Stale),
                &map1("/mnt/data", "10.0.0.1:/e".to_string()),
                &map1("/mnt/data", "soft,vers=4.2".to_string()),
                &map1("/mnt/data", true),
                &map1("/mnt/data", 2usize), // stacked → multi_mounted
            );
            let row = mounts::endpoint_db::get_by_id("m-1").unwrap().unwrap();
            assert_eq!(row.health, Health::Stale);
            assert_eq!(row.active_route.as_deref(), Some("10.0.0.1:/e"));
            assert_eq!(row.active_options.as_deref(), Some("soft,vers=4.2"));
            assert!(row.drift);
            assert!(row.multi_mounted, "count > 1 sets multi_mounted");
        });
    }

    #[test]
    fn persist_mount_state_defaults_missing_target_to_health_missing() {
        with_db("persist_default.db", || {
            insert_share("sh-1", true, true);
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);
            // No maps carry this target → health defaults to Missing, others clear.
            persist_mount_state(
                "h1",
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            );
            let row = mounts::endpoint_db::get_by_id("m-1").unwrap().unwrap();
            assert_eq!(row.health, Health::Missing);
            assert!(row.active_route.is_none());
            assert!(!row.drift);
            assert!(!row.multi_mounted);
        });
    }

    #[test]
    fn persist_mount_state_skips_rows_for_other_hosts() {
        with_db("persist_other_host.db", || {
            insert_share("sh-1", true, true);
            // Row belongs to h1; the tick runs for a different host.
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);
            persist_mount_state(
                "other-host",
                &map1("/mnt/data", Health::Stale),
                &map1("/mnt/data", "10.0.0.1:/e".to_string()),
                &map1("/mnt/data", "soft".to_string()),
                &map1("/mnt/data", true),
                &map1("/mnt/data", 2usize),
            );
            // Row untouched: it stays at the inserted Ok health, no drift.
            let row = mounts::endpoint_db::get_by_id("m-1").unwrap().unwrap();
            assert_eq!(row.health, Health::Ok);
            assert!(row.active_route.is_none());
            assert!(!row.drift);
        });
    }

    // ── raise_notification: idempotent upsert into the notifications store ──

    #[test]
    fn raise_notification_persists_a_row_readable_by_key() {
        with_db("raise_notify.db", || {
            raise_notification(
                "remediation:converge:test-key".to_string(),
                Severity::Info,
                false,
                "Test title".to_string(),
                "Test body".to_string(),
                None,
            );
            let conn = db::open_default().unwrap();
            let got = db::notifications_store::get(&conn, "remediation:converge:test-key")
                .unwrap()
                .expect("notification raised");
            assert_eq!(got.title, "Test title");
            assert_eq!(got.body.as_deref(), Some("Test body"));
            assert_eq!(got.source, NOTIFY_SOURCE);
        });
    }

    #[test]
    fn raise_notification_same_key_is_idempotent_single_row() {
        with_db("raise_notify_idem.db", || {
            for _ in 0..3 {
                raise_notification(
                    "remediation:converge:dup".to_string(),
                    Severity::Warn,
                    true,
                    "Dup".to_string(),
                    "again".to_string(),
                    None,
                );
            }
            let conn = db::open_default().unwrap();
            let all = db::notifications_store::list(
                &conn,
                &db::notifications_store::ListFilter::default(),
            )
            .unwrap();
            let matching = all
                .iter()
                .filter(|n| n.key == "remediation:converge:dup")
                .count();
            assert_eq!(matching, 1, "re-raising one key upserts a single row");
        });
    }

    // ── resolve_secret_file: fail-closed branches (no DB, no backend) ──────

    #[tokio::test]
    async fn resolve_secret_file_none_when_no_credential() {
        // A desired mount with no credential short-circuits to None before any
        // backend lookup — the common case for public NFS exports.
        assert!(resolve_secret_file(&d("/mnt/data")).await.is_none());
    }

    #[tokio::test]
    async fn resolve_secret_file_none_when_credential_empty_string() {
        // An empty-string credential is filtered out just like `None`.
        let dm = DesiredMount {
            credential: Some(String::new()),
            ..d("/mnt/data")
        };
        assert!(resolve_secret_file(&dm).await.is_none());
    }

    #[tokio::test]
    async fn resolve_secret_file_none_when_backend_unregistered_fails_closed() {
        // A credential is declared but the backend is not registered in this bare
        // test process → fail closed with None (the mount will not proceed with a
        // missing secret-file), exercising the `backend()` None arm.
        let dm = DesiredMount {
            backend: "definitely-not-a-registered-backend".to_string(),
            credential: Some("cred-ref".to_string()),
            ..d("/mnt/data")
        };
        assert!(resolve_secret_file(&dm).await.is_none());
    }

    // ── resolve_secret_file: registered-backend success + validate-error arms ──
    //
    // These register a fake StorageBackend against the process-global registry so
    // `resolve_secret_file` reaches the `backend()` Some arm and drives its
    // `validate_spec` — the success path (a rendered secret-file threads through)
    // and the fail-closed error path (validate_spec errs → None). Serialized on
    // `storage_registry` per the global-state race gotcha; deregistered after.

    /// A fake backend whose `validate_spec` either returns a NormalizedSpec
    /// carrying a rendered secret-file, or errors — selected by `render_secret`.
    struct SecretFileBackend {
        name: String,
        /// `Some((path, contents))` → validate_spec renders that secret-file;
        /// `None` → validate_spec returns an error (the fail-closed path).
        render_secret: Option<(String, String)>,
    }

    #[derive::orca_async]
    impl plugin_toolkit::storage::StorageBackend for SecretFileBackend {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> plugin_toolkit::storage::StorageKind {
            plugin_toolkit::storage::StorageKind::NetworkShare
        }
        fn capabilities(&self) -> Vec<plugin_toolkit::storage::Capability> {
            vec![plugin_toolkit::storage::Capability::Mount]
        }
        fn endpoint(&self) -> String {
            format!("fake://{}", self.name)
        }
        async fn validate_spec(
            &self,
            spec: &plugin_toolkit::storage::MountSpec,
        ) -> Result<plugin_toolkit::storage::NormalizedSpec, plugin_toolkit::storage::StorageError>
        {
            let Some((path, contents)) = &self.render_secret else {
                return Err(plugin_toolkit::storage::StorageError::Other(
                    "secret render failed".into(),
                ));
            };
            Ok(plugin_toolkit::storage::NormalizedSpec {
                backend: spec.backend.clone(),
                target: spec.target.clone(),
                fstype: spec.fstype.clone(),
                source: spec.source.clone(),
                failover_sources: spec.failover_sources.clone(),
                options: plugin_toolkit::storage::OptionSet::Raw {
                    options: spec.options.clone(),
                },
                credential: spec.credential.clone(),
                secret_file: Some(plugin_toolkit::storage::SecretFile {
                    path: path.clone(),
                    contents: contents.clone(),
                }),
                remount_policy: spec.remount_policy.clone(),
                enabled: spec.enabled,
            })
        }
    }

    /// A guest applier that records every spec it is asked to apply, so a test can
    /// assert the converge tick routes guest-targeted placements to it.
    struct RecordingGuestApplier {
        name: String,
        applied: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    #[derive::orca_async]
    impl plugin_toolkit::storage::GuestMountApplier for RecordingGuestApplier {
        fn name(&self) -> &str {
            &self.name
        }
        async fn apply(
            &self,
            spec: &plugin_toolkit::storage::GuestMountSpec,
        ) -> Result<(), plugin_toolkit::storage::StorageError> {
            self.applied
                .lock()
                .expect("applied log poisoned")
                .push((spec.guest.clone(), spec.target.clone()));
            Ok(())
        }
        async fn remove(
            &self,
            _guest: &str,
            _target: &str,
        ) -> Result<(), plugin_toolkit::storage::StorageError> {
            Ok(())
        }
    }

    #[tokio::test]
    #[serial_test::serial(storage_registry)]
    async fn reconcile_guest_mounts_routes_guest_placement_to_applier() {
        let applied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let name = "proxmox-guest-fake";
        plugin_toolkit::storage::register_guest_applier(std::sync::Arc::new(
            RecordingGuestApplier {
                name: name.to_string(),
                applied: applied.clone(),
            },
        ));
        // Pin a temp DB for the whole async body — `with_db`'s scoped closure
        // would not hold the thread-local across the `.await` in reconcile. The
        // `#[tokio::test]` current-thread runtime keeps this same thread across
        // the await, so the thread-local db path stays live.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reconcile_guest.db");
        db::set_thread_db_path(Some(&path.to_string_lossy()));
        {
            let conn = db::open_default().expect("open temp db");
            db::schema_fragments::apply_fragments(&conn).expect("apply fragments");
            drop(conn);
            insert_share("sh-1", true, true);
            insert_guest_mount("m-guest", "sh-1", "h1", "/mnt/backups", "110");
            // A host-targeted placement on the same host must NOT reach the applier.
            insert_mount("m-host", "sh-1", "h1", "/mnt/data", true);
        }
        reconcile_guest_mounts("h1").await;
        db::set_thread_db_path(None);
        plugin_toolkit::storage::deregister_guest_applier(name);

        let log = applied.lock().expect("applied log poisoned").clone();
        assert_eq!(log, vec![("110".to_string(), "/mnt/backups".to_string())]);
    }

    #[tokio::test]
    #[serial_test::serial(storage_registry)]
    async fn resolve_secret_file_threads_rendered_secret_from_backend() {
        let name = "smb-secret-fake";
        plugin_toolkit::storage::register_backend(std::sync::Arc::new(SecretFileBackend {
            name: name.to_string(),
            render_secret: Some((
                "/run/orca/secret-files/mnt-data".to_string(),
                "username=alice\npassword=hunter2".to_string(),
            )),
        }));
        let dm = DesiredMount {
            backend: name.to_string(),
            credential: Some("secret:cred-ref".to_string()),
            ..d("/mnt/data")
        };
        let got = resolve_secret_file(&dm).await;
        plugin_toolkit::storage::deregister_backend(name);
        let sf = got.expect("registered backend renders the secret-file");
        assert_eq!(sf.path, "/run/orca/secret-files/mnt-data");
        assert_eq!(sf.contents, "username=alice\npassword=hunter2");
    }

    #[tokio::test]
    #[serial_test::serial(storage_registry)]
    async fn resolve_secret_file_none_when_backend_validate_errs_fails_closed() {
        let name = "smb-erroring-fake";
        plugin_toolkit::storage::register_backend(std::sync::Arc::new(SecretFileBackend {
            name: name.to_string(),
            render_secret: None, // validate_spec returns Err → fail closed
        }));
        let dm = DesiredMount {
            backend: name.to_string(),
            credential: Some("secret:cred-ref".to_string()),
            ..d("/mnt/data")
        };
        let got = resolve_secret_file(&dm).await;
        plugin_toolkit::storage::deregister_backend(name);
        assert!(
            got.is_none(),
            "a validate_spec error must fail closed to None"
        );
    }

    // ── replication_health_by_target: no-ref fast path ────────────────────

    #[tokio::test]
    async fn replication_health_by_target_empty_when_no_ref_declared() {
        // No desired mount carries a replication ref → the relationship list read
        // is skipped entirely and the map is empty (the gate reads absence as
        // `None` = hold, but that never fires without a ref).
        let out = replication_health_by_target(&[d("/mnt/a"), d("/mnt/b")]).await;
        assert!(out.is_empty());
    }

    /// Block on an async future on THIS thread so the thread-local test db path
    /// set by `with_db` is still in scope for the resolve.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
            .block_on(fut)
    }

    #[test]
    fn replication_health_dangling_ref_resolves_to_none() {
        // A ref is declared but no relationship with that id exists (deleted out
        // from under the share) → the ref-present path runs the relationship list
        // read, the `by_id.get` misses, and the target resolves to `None` (unknown
        // ⇒ the gate holds).
        with_db("repl_dangling.db", || {
            let desired = [d_repl("/mnt/data", RemountPolicy::default())];
            let out = block_on(replication_health_by_target(&desired));
            assert_eq!(
                out.get("/mnt/data"),
                Some(&None),
                "dangling ref present in map as unknown"
            );
        });
    }

    #[test]
    fn replication_health_present_relationship_unknown_without_provider() {
        // The relationship exists, but no status provider is registered in this
        // bare test process → `resolve_replication_status` returns None, so the
        // matching (`Some(rel)`) arm still resolves the target to `None`. Two
        // shares sharing one relationship id both resolve (the per-id cache).
        with_db("repl_present.db", || {
            let rel = replication::EndpointRow {
                id: "rep-0000".to_string(),
                name: "media-replica".to_string(),
                provider: "syncthing".to_string(),
                folder: "folder-1".to_string(),
                routes: plugin_toolkit::route::Routes::from(vec![Route::new(
                    "lan_v4",
                    "nfs",
                    "10.0.0.1",
                    Some(2049),
                )]),
                enabled: true,
            };
            replication::endpoint_db::insert(&rel).expect("insert relationship");

            let desired = [
                d_repl("/mnt/data", RemountPolicy::default()),
                d_repl("/mnt/media", RemountPolicy::default()),
            ];
            let out = block_on(replication_health_by_target(&desired));
            assert_eq!(out.get("/mnt/data"), Some(&None));
            assert_eq!(out.get("/mnt/media"), Some(&None));
        });
    }

    // ── elect: empty ordered sources → Empty election ─────────────────────

    #[tokio::test]
    async fn elect_empty_sources_is_empty_election() {
        // No sources to probe → election is Empty without any network I/O.
        let out = elect(&[], "nfs4", SourceProbe::Tcp, Duration::from_millis(1)).await;
        assert_eq!(out, Election::Empty);
    }

    // ── probe_live: NFS probe with an unaimable source is down ────────────

    #[tokio::test]
    async fn probe_live_nfs_source_without_host_is_down() {
        // An explicit NFS probe against a source `host_of_source` cannot parse a
        // host from returns false WITHOUT any network I/O — the `None` arm of the
        // spawn_blocking match. `noscheme` has no `:` and no `//`.
        assert!(
            !probe_live(
                "noscheme",
                "nfs4",
                SourceProbe::Nfs,
                Duration::from_millis(1)
            )
            .await
        );
    }

    #[tokio::test]
    async fn probe_live_auto_resolves_to_nfs_for_nfs_fstype() {
        // `Auto` resolves against the fstype: an `nfs*` fstype picks the RPC-NULL
        // probe, so a hostless source still classifies down via the NFS `None` arm
        // — exercising `probe.resolve(fstype)` on the nfs branch.
        assert!(
            !probe_live(
                "noscheme",
                "nfs4",
                SourceProbe::Auto,
                Duration::from_millis(1)
            )
            .await
        );
    }

    #[tokio::test]
    async fn elect_all_nfs_hostless_sources_yields_empty() {
        // Two ordered sources that the NFS probe can never aim (no parseable host)
        // → each `probe_live` returns false, the loop exhausts, and the election is
        // Empty. Drives the `elect` loop body (index/enumerate) with no network.
        let sources = vec!["noscheme".to_string(), "alsonoscheme".to_string()];
        let out = elect(&sources, "nfs4", SourceProbe::Nfs, Duration::from_millis(1)).await;
        assert_eq!(out, Election::Empty);
    }

    // ── mount_req: full field mapping incl. absent secret ─────────────────

    #[test]
    fn mount_req_maps_target_fstype_options_and_no_secret() {
        let req = mount_req(&d("/mnt/x"), "10.0.0.1:/e", None);
        assert_eq!(req.source, "10.0.0.1:/e");
        assert_eq!(req.target, "/mnt/x");
        assert_eq!(req.fstype, "nfs4");
        assert_eq!(req.options, "vers=4.2,soft");
        assert!(req.secret_file.is_none());
    }

    // ── remediation_policy: reads a stored non-default value ───────────────

    #[test]
    fn remediation_policy_reads_stored_auto_fix_value() {
        with_db("remediation_stored.db", || {
            let conn = db::open_default().unwrap();
            db::settings::set(&conn, crate::remediation::POLICY_KEY, "auto_fix").unwrap();
            drop(conn);
            assert_eq!(remediation_policy(), RemediationPolicy::AutoFix);
        });
    }

    // ── desired_for_host: full field passthrough + held-route split ────────

    #[test]
    fn desired_for_host_passes_through_replication_credential_and_splits_held_routes() {
        with_db("desired_passthrough.db", || {
            // A share carrying a credential + replication ref, whose route set mixes
            // one enabled and one held route.
            let enabled_route = Route {
                path: Some("/export".to_string()),
                enabled: true,
                ..Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049))
            };
            let held_route = Route {
                path: Some("/export".to_string()),
                enabled: false,
                ..Route::new("lan_v4", "nfs", "10.0.0.2", Some(2049))
            };
            let share = shares::EndpointRow {
                id: "sh-1".to_string(),
                name: "media".to_string(),
                backend: "nfs".into(),
                fstype: "nfs4".into(),
                options: "{}".into(),
                options_rendered: "vers=4.2,soft".into(),
                credential: Some("secret:cred-ref".to_string()),
                replication: Some("rep-abc".to_string()),
                routes: plugin_toolkit::route::Routes::from(vec![enabled_route, held_route]),
                enabled: true,
            };
            shares::endpoint_db::insert(&share).expect("insert share");
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);

            let out = desired_for_host("h1").expect("desired ok");
            assert_eq!(out.len(), 1);
            let dm = &out[0];
            // Only the ENABLED route becomes an ordered source...
            assert_eq!(dm.sources, vec!["10.0.0.1:/export".to_string()]);
            // ...but the full route set (held included) is retained for policy.
            assert_eq!(dm.routes.len(), 2);
            assert_eq!(dm.replication.as_deref(), Some("rep-abc"));
            assert_eq!(dm.credential.as_deref(), Some("secret:cred-ref"));
            assert_eq!(dm.options, "vers=4.2,soft");
            // No per-mount policy stored → the engine default.
            assert_eq!(dm.remount_policy, RemountPolicy::default());
        });
    }

    #[test]
    fn desired_for_host_carries_per_mount_remount_policy() {
        with_db("desired_policy.db", || {
            insert_share("sh-1", true, true);
            let pol = RemountPolicy {
                aggression: RemountAggression::Force,
                ..Default::default()
            };
            let row = mounts::EndpointRow {
                guest: None,
                id: "m-1".to_string(),
                name: "m-1".to_string(),
                share_id: "sh-1".to_string(),
                host: "h1".to_string(),
                target: "/mnt/data".to_string(),
                remount_policy: Some(pol.clone()),
                health: plugin_toolkit::storage::Health::Ok,
                active_route: None,
                active_options: None,
                drift: false,
                multi_mounted: false,
                enabled: true,
            };
            mounts::endpoint_db::insert(&row).expect("insert mount");

            let out = desired_for_host("h1").expect("desired ok");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].remount_policy, pol);
            assert_eq!(out[0].remount_policy.aggression, RemountAggression::Force);
        });
    }

    #[test]
    fn desired_for_host_returns_multiple_placements_for_same_host() {
        with_db("desired_multi.db", || {
            insert_share("sh-1", true, true);
            insert_mount("m-1", "sh-1", "h1", "/mnt/a", true);
            insert_mount("m-2", "sh-1", "h1", "/mnt/b", true);
            let mut out = desired_for_host("h1").expect("desired ok");
            out.sort_by(|a, b| a.target.cmp(&b.target));
            let targets: Vec<&str> = out.iter().map(|d| d.target.as_str()).collect();
            assert_eq!(targets, vec!["/mnt/a", "/mnt/b"]);
        });
    }

    // ── persist_mount_state: unchanged row is not rewritten ────────────────

    #[test]
    fn persist_mount_state_no_change_leaves_row_untouched() {
        with_db("persist_nochange.db", || {
            insert_share("sh-1", true, true);
            // Insert a mount whose stored fields already match what we persist:
            // health Ok, no active route/options, no drift, not multi-mounted.
            insert_mount("m-1", "sh-1", "h1", "/mnt/data", true);
            // Persist Ok health with all-empty maps → row already matches, so the
            // early `continue` fires and no update is issued (still readable).
            persist_mount_state(
                "h1",
                &map1("/mnt/data", Health::Ok),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            );
            let row = mounts::endpoint_db::get_by_id("m-1").unwrap().unwrap();
            assert_eq!(row.health, Health::Ok);
            assert!(row.active_route.is_none());
            assert!(!row.drift);
            assert!(!row.multi_mounted);
        });
    }

    // ── ledger_file / save_ledger / load_ledger via $ORCA_HOME ─────────────
    //
    // The `_at` variants are unit-tested above; these drive the wrappers that
    // resolve the on-disk path through `contract::config::state_dir()`. Nextest
    // runs each test in its own process, so the `$ORCA_HOME` override is isolated.

    #[test]
    #[serial_test::serial(env)]
    fn ledger_file_resolves_managed_mounts_under_orca_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("ORCA_HOME", dir.path()) };
        let path = ledger_file().expect("ledger path resolves from $ORCA_HOME");
        assert_eq!(path, dir.path().join("managed_mounts.json"));
    }

    #[test]
    #[serial_test::serial(env)]
    fn save_ledger_then_load_ledger_round_trips_via_orca_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("ORCA_HOME", dir.path()) };
        // A fresh state dir with no ledger file yet loads as empty.
        assert!(
            load_ledger().is_empty(),
            "absent ledger under a fresh $ORCA_HOME is empty"
        );
        let want = set(&["/mnt/one", "/mnt/two"]);
        save_ledger(&want);
        // The wrapper wrote it to the resolved path, and the read wrapper reads it back.
        assert_eq!(load_ledger(), want);
        // And the file materialized exactly where `ledger_file` points.
        assert!(dir.path().join("managed_mounts.json").exists());
    }

    // ── probe_live: Tcp arm resolves for a non-nfs fstype (no I/O) ─────────

    #[tokio::test]
    async fn probe_live_tcp_arm_hostless_source_is_down() {
        // A non-nfs fstype keeps `SourceProbe::Tcp` as Tcp; a source with no
        // parseable host yields `false` from `probe_source` without any network
        // I/O — exercising the `_ =>` (Tcp) arm of the spawn_blocking match.
        assert!(
            !probe_live(
                "noscheme",
                "ext4",
                SourceProbe::Tcp,
                Duration::from_millis(1)
            )
            .await
        );
    }

    #[tokio::test]
    async fn elect_tcp_hostless_sources_yield_empty() {
        // The `elect` loop over ordered sources with the Tcp probe: each hostless
        // source probes down, the loop exhausts, and the election is Empty.
        let sources = vec!["noscheme".to_string(), "stillnone".to_string()];
        let out = elect(&sources, "ext4", SourceProbe::Tcp, Duration::from_millis(1)).await;
        assert_eq!(out, Election::Empty);
    }

    // ── tick: end-to-end no-op orchestration pass ─────────────────────────
    //
    // Drives the whole async convergence pass for a host with NO desired
    // placements: the probe/election/plan/exec/persist skeleton runs but issues
    // no privileged mount/unmount and touches no foreign mount. Under the default
    // (Notify) policy the backend recovery sweep is skipped; the ledger is
    // persisted (empty) under $ORCA_HOME. Exercises the tick top-level flow that
    // the pure-unit tests above cannot reach.

    #[test]
    #[serial_test::serial(env)]
    fn tick_no_desired_placements_is_ok_noop_and_persists_empty_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("ORCA_HOME", dir.path()) };
        crate::host_identity::init(dir.path()).ok();
        with_db("tick_noop.db", || {
            // A placement for a DIFFERENT host: desired_for_host(this_host) filters
            // it out, so the tick has nothing to converge and persist skips its row.
            insert_share("sh-1", true, true);
            insert_mount("m-1", "sh-1", "some-other-host", "/mnt/elsewhere", true);
            let counters = Mutex::new(HashMap::new());
            let res = block_on(tick(&counters));
            assert!(res.is_ok(), "no-op tick returns Ok: {res:?}");
            // No target was mounted, so the ledger persisted empty.
            assert!(
                load_ledger().is_empty(),
                "no-op tick persists an empty managed-mount ledger"
            );
            // The other-host row is untouched by this host's persist pass.
            let row = mounts::endpoint_db::get_by_id("m-1").unwrap().unwrap();
            assert_eq!(row.health, Health::Ok);
            assert!(row.active_route.is_none());
        });
    }

    #[test]
    #[serial_test::serial(env)]
    fn tick_under_acting_policy_runs_backend_sweep_still_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("ORCA_HOME", dir.path()) };
        crate::host_identity::init(dir.path()).ok();
        with_db("tick_autofix.db", || {
            // Store an acting policy so `policy.acts()` is true and the backend
            // consumer-stale recovery sweep runs (over an empty desired set — no
            // backend is registered in this bare process, so it merges to nothing).
            let conn = db::open_default().unwrap();
            db::settings::set(&conn, crate::remediation::POLICY_KEY, "auto_fix").unwrap();
            drop(conn);
            assert_eq!(remediation_policy(), RemediationPolicy::AutoFix);

            let counters = Mutex::new(HashMap::new());
            let res = block_on(tick(&counters));
            assert!(res.is_ok(), "acting-policy tick returns Ok: {res:?}");
            assert!(load_ledger().is_empty());
        });
    }
}
