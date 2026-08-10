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
use crate::source_election::{self, Election};
use crate::{host_identity, mounts, periodic, shares};
use plugin_toolkit::route::Route;
use plugin_toolkit::storage::{
    Health, RemountAggression, RemountPolicy, SourceProbe, probe_source, probe_source_nfs,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

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
}

/// Whether a healthy mount should be re-pointed at its elected source, honouring
/// the mount's typed [`RemountPolicy`]. Pure so the fail-back / degrade / held /
/// Safe-busy matrix is unit-tested.
///
/// - failover disabled            → never swap (mount pinned).
/// - fail-back but `return_to_primary = false` → stay degraded.
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
    // The Plex/Jellyfin guarantee: never force-swap a busy mount under Safe.
    !(pol.aggression == RemountAggression::Safe && sig.busy)
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
        c.retain(|t, _| desired.iter().any(|d| &d.target == t));
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
        failover.insert(
            target,
            FailoverSignal {
                elected: election,
                active,
                busy,
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
        // Only re-option a mount that is on its ELECTED source. A mount parked on
        // a non-elected source is the failover/pin path's to decide; re-optioning
        // it here could silently move it under a pinned policy.
        let elected = elected_by_target.get(&d.target).and_then(elected_source);
        if elected.is_none()
            || elected.as_deref() != active_by_target.get(&d.target).map(String::as_str)
        {
            continue;
        }
        // The Plex/Jellyfin guarantee. The remount is a lazy detach + remount, so
        // open handles survive on the old superblock — but the umount→mount window
        // momentarily has no filesystem bound at the path, so a NEW open racing it
        // would fail. Under Safe we therefore defer a BUSY mount to the next idle
        // tick rather than risk live media; Force remounts immediately.
        let busy = autofs::is_busy(&d.target).await;
        if d.remount_policy.aggression == RemountAggression::Safe && busy {
            info!(
                "[converge] {} option drift (desired `{}`); deferring remount — busy under \
                 Safe, will re-option when idle",
                d.target, d.options
            );
            continue;
        }
        info!(
            "[converge] {} option drift (desired `{}`); scheduling lazy re-option remount",
            d.target, d.options
        );
        option_drift_remount.insert(d.target.clone());
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
    if actions.is_empty() {
        // Persist last-known health/active even on a no-op tick so detail stays
        // current without a live probe.
        persist_mount_state(
            this_host,
            &health_by_target,
            &active_by_target,
            &active_options_by_target,
            &drift_by_target,
        );
        // Nothing to mount/unmount — but still adopt desired targets orca already
        // holds mounted (e.g. mounted by a prior tick, or healthy after a restart)
        // so a later placement removal is reconciled even on a no-op tick. Without
        // this, an already-healthy placement never enters the ledger and its
        // eventual removal would leave the mount orphaned.
        for d in &desired {
            if mounted_any.contains(&d.target) {
                ledger.insert(d.target.clone());
            }
        }
        // The probe above may also have forgotten already-gone orphans, so persist
        // the (adopted + pruned) ledger.
        save_ledger(&ledger);
        return Ok(());
    }

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
    );
    Ok(())
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
        if row.health == health
            && row.active_route == active_route
            && row.active_options == active_options
            && row.drift == drift
        {
            continue; // no change — skip the write (and its LWW clock bump)
        }
        row.health = health;
        row.active_route = active_route;
        row.active_options = active_options;
        row.drift = drift;
        if let Err(e) = mounts::endpoint_db::update(&row) {
            warn!(
                "[converge] could not persist health for {}: {e}",
                row.target
            );
        }
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
            options: "vers=4.2,soft".to_string(),
            credential: None,
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
        let mut m = HashMap::new();
        m.insert(
            "/mnt/data".to_string(),
            FailoverSignal {
                elected,
                active: Some(active.to_string()),
                busy,
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
}
