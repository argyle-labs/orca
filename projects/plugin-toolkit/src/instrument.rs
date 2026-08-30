//! Opt-in plugin instrumentation framework.
//!
//! Every plugin *inherits* this from the orca parent with no per-plugin code:
//!
//! 1. **Allocator substrate.** The serve macros (and the public
//!    [`bootstrap!`](crate::instrument::bootstrap) macro, for hand-rolled mains)
//!    set the plugin's `#[global_allocator]` to jemalloc — the same allocator the
//!    parent daemon runs (`server::main`). It is **inert** without `MALLOC_CONF`:
//!    no profiling, negligible overhead, behaves like the system allocator. This
//!    is always compiled in; it does not depend on the enable flag.
//!
//! 2. **Profiling activation (opt-in).** The parent activates heap profiling for
//!    a specific plugin by respawning it with `MALLOC_CONF=prof:true,…` plus the
//!    [`INSTRUMENT_ENV`] marker (`ORCA_PLUGIN_INSTRUMENT=1`). See
//!    `contract::plugin_instrument` for the parent side. **Toggle+restart
//!    contract:** the env is read once, by jemalloc, at process start — so
//!    activating profiling requires the plugin to *respawn* with the env set.
//!
//! 3. **Auto-diagnostics (opt-in).** When (and only when) [`enabled`] is true,
//!    the serve loop advertises a built-in `diagnostics` provider and answers its
//!    `diagnose`/`repair` ops with typed findings — jemalloc stats, open FDs,
//!    process RSS, and the size of every registered [`PollInventory`]. With the
//!    flag off, nothing is advertised, no provider is registered, and no extra
//!    work runs: the plugin behaves exactly as before.
//!
//! 4. **Anti-leak primitives.** [`PollInventory`] holds poll-cycle entity state
//!    keyed by `K`; each cycle the plugin hands it the *full* current set via
//!    [`PollInventory::reconcile`], which REPLACES the contents — so cross-cycle
//!    accumulation (the unbounded-poll-cache leak class) is structurally
//!    impossible. [`PollDriver`] owns the poll loop with fixed interval and
//!    single-flight (no overlapping polls). Every `PollInventory` registers a weak
//!    handle into a process-global registry so the auto-diagnostics provider can
//!    report its size without the plugin wiring anything.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

/// Environment marker the parent sets (to `"1"`) on a plugin subprocess to
/// activate instrumentation. Absent/unset → the plugin runs stock.
pub const INSTRUMENT_ENV: &str = "ORCA_PLUGIN_INSTRUMENT";

/// Whether the parent activated instrumentation for this process. Reads
/// [`INSTRUMENT_ENV`]; `true` only when it is exactly `"1"`. This is the single
/// gate for every opt-in behavior (provider registration, diagnostics work).
pub fn enabled() -> bool {
    std::env::var_os(INSTRUMENT_ENV).is_some_and(|v| v == "1")
}

// ── Allocator substrate ──────────────────────────────────────────────────────

/// Set this binary's `#[global_allocator]` to jemalloc — the instrumentation
/// allocator substrate. Inert without `MALLOC_CONF` (see the module docs).
///
/// The serve macros emit this for every plugin automatically. A **hand-rolled
/// `main`** (a plugin that does not use a `serve_*_plugin!` macro) calls it at
/// module scope instead:
///
/// ```rust,ignore
/// plugin_toolkit::instrument::bootstrap!();
///
/// fn main() -> anyhow::Result<()> { /* … */ }
/// ```
///
/// **Single-declaration rule:** a `#[global_allocator]` may be declared exactly
/// once per binary. A plugin therefore uses *either* a `serve_*_plugin!` macro
/// (which emits it) *or* one explicit `bootstrap!()` — never both, or the two
/// `__ORCA_PLUGIN_ALLOC` statics collide at compile time. A hybrid main (e.g.
/// nut, which hand-rolls `main` but merges toolkit backend defs) is the
/// hand-rolled case: it calls `bootstrap!()` once and is the single source.
#[macro_export]
macro_rules! instrument_bootstrap {
    () => {
        #[global_allocator]
        static __ORCA_PLUGIN_ALLOC: $crate::tikv_jemallocator::Jemalloc =
            $crate::tikv_jemallocator::Jemalloc;
    };
}
#[doc(inline)]
pub use crate::instrument_bootstrap as bootstrap;

// ── PollInventory: the anti-leak primitive ──────────────────────────────────

/// A snapshot of one [`PollInventory`]'s size, read by the auto-diagnostics
/// provider through the process-global registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryStats {
    /// Registry name (the plugin-supplied inventory label).
    pub name: String,
    /// Live entry count (after TTL pruning).
    pub len: usize,
    /// Configured capacity cap, if any.
    pub capacity: Option<usize>,
}

/// Object-safe view every `PollInventory` exposes to the registry, erasing `K`/`V`.
trait InventoryHandle: Send + Sync {
    fn stats(&self) -> InventoryStats;
}

static REGISTRY: LazyLock<Mutex<Vec<Weak<dyn InventoryHandle>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

fn register_inventory(h: Weak<dyn InventoryHandle>) {
    if let Ok(mut reg) = REGISTRY.lock() {
        reg.retain(|w| w.strong_count() > 0);
        reg.push(h);
    }
}

/// Snapshot of every live [`PollInventory`]'s stats. Dead (dropped) inventories
/// are pruned here. Used by the auto-diagnostics provider.
pub fn inventory_stats() -> Vec<InventoryStats> {
    let mut out = Vec::new();
    if let Ok(mut reg) = REGISTRY.lock() {
        reg.retain(|w| w.strong_count() > 0);
        for w in reg.iter() {
            if let Some(h) = w.upgrade() {
                out.push(h.stats());
            }
        }
    }
    out
}

struct Entry<V> {
    value: V,
    inserted: Instant,
}

struct Inner<K, V> {
    name: String,
    map: HashMap<K, Entry<V>>,
    /// Insertion/recency order; front = oldest. Used for capacity eviction.
    order: VecDeque<K>,
    capacity: Option<usize>,
    ttl: Option<Duration>,
}

impl<K: Eq + Hash + Clone, V> Inner<K, V> {
    fn prune_expired(&mut self, now: Instant) {
        let Some(ttl) = self.ttl else { return };
        let expired: Vec<K> = self
            .map
            .iter()
            .filter(|(_, e)| now.duration_since(e.inserted) >= ttl)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            self.map.remove(&k);
            self.order.retain(|o| o != &k);
        }
    }

    fn enforce_capacity(&mut self) {
        let Some(cap) = self.capacity else { return };
        while self.map.len() > cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&oldest);
        }
    }
}

impl<K, V> InventoryHandle for Mutex<Inner<K, V>>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Send + 'static,
{
    fn stats(&self) -> InventoryStats {
        let now = Instant::now();
        let mut g = self.lock().expect("PollInventory poisoned");
        g.prune_expired(now);
        InventoryStats {
            name: g.name.clone(),
            len: g.map.len(),
            capacity: g.capacity,
        }
    }
}

/// Poll-cycle entity state keyed by `K`. Each poll the plugin hands the FULL
/// current set to [`reconcile`](Self::reconcile), which REPLACES the contents:
/// keys absent from the snapshot are dropped, so a poll cache cannot grow across
/// cycles. Optional [`with_capacity`](Self::with_capacity) cap (evict-oldest
/// beyond) and per-entry [`with_ttl`](Self::with_ttl) bound it further.
///
/// Cheap to clone (shares one `Arc`); every clone reads the same state. The
/// inventory registers a weak handle into a process-global registry on
/// construction so the auto-diagnostics provider can report its size.
pub struct PollInventory<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Send + 'static,
{
    inner: Arc<Mutex<Inner<K, V>>>,
}

impl<K, V> Clone for PollInventory<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> PollInventory<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Send + 'static,
{
    /// A new inventory labeled `name` (surfaced in diagnostics), unbounded, no TTL.
    pub fn new(name: impl Into<String>) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            name: name.into(),
            map: HashMap::new(),
            order: VecDeque::new(),
            capacity: None,
            ttl: None,
        }));
        // The registry holds only a Weak, erased to `dyn InventoryHandle`; it
        // upgrades while this struct's `inner` strong ref lives and prunes once
        // the inventory is dropped.
        let handle: Arc<dyn InventoryHandle> = inner.clone();
        register_inventory(Arc::downgrade(&handle));
        drop(handle);
        Self { inner }
    }

    /// Set a capacity cap. Beyond it, [`reconcile`](Self::reconcile) /
    /// [`insert`](Self::insert) evict the oldest entries.
    pub fn with_capacity(self, capacity: usize) -> Self {
        self.inner.lock().expect("PollInventory poisoned").capacity = Some(capacity);
        self
    }

    /// Set a per-entry TTL. Entries older than `ttl` are pruned on the next
    /// mutation or stats read.
    pub fn with_ttl(self, ttl: Duration) -> Self {
        self.inner.lock().expect("PollInventory poisoned").ttl = Some(ttl);
        self
    }

    /// Replace the contents with `snapshot` (the full current set from one poll).
    /// Keys present in both keep their original insertion time (so TTL measures
    /// age, not last-seen); keys absent from the snapshot are DROPPED. Capacity is
    /// enforced after the replace.
    pub fn reconcile(&self, snapshot: impl IntoIterator<Item = (K, V)>) {
        let now = Instant::now();
        let mut g = self.inner.lock().expect("PollInventory poisoned");
        g.prune_expired(now);
        let mut next: HashMap<K, Entry<V>> = HashMap::new();
        let mut order: VecDeque<K> = VecDeque::new();
        for (k, v) in snapshot {
            let inserted = g.map.get(&k).map(|e| e.inserted).unwrap_or(now);
            if !next.contains_key(&k) {
                order.push_back(k.clone());
            }
            next.insert(k, Entry { value: v, inserted });
        }
        g.map = next;
        g.order = order;
        g.enforce_capacity();
    }

    /// Incrementally insert or update one entry (for plugins that stream events
    /// rather than snapshot). Refreshes the insertion time. Capacity is enforced.
    pub fn insert(&self, key: K, value: V) {
        let now = Instant::now();
        let mut g = self.inner.lock().expect("PollInventory poisoned");
        g.prune_expired(now);
        if !g.map.contains_key(&key) {
            g.order.push_back(key.clone());
        }
        g.map.insert(
            key,
            Entry {
                value,
                inserted: now,
            },
        );
        g.enforce_capacity();
    }

    /// Live entry count after TTL pruning.
    pub fn len(&self) -> usize {
        let now = Instant::now();
        let mut g = self.inner.lock().expect("PollInventory poisoned");
        g.prune_expired(now);
        g.map.len()
    }

    /// Whether the inventory is empty (after TTL pruning).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read a value by key, if present and unexpired (via `f` to avoid cloning `V`).
    pub fn with<R>(&self, key: &K, f: impl FnOnce(&V) -> R) -> Option<R> {
        let now = Instant::now();
        let mut g = self.inner.lock().expect("PollInventory poisoned");
        g.prune_expired(now);
        g.map.get(key).map(|e| f(&e.value))
    }

    /// This inventory's current stats (name, len, capacity).
    pub fn stats(&self) -> InventoryStats {
        InventoryHandle::stats(&*self.inner)
    }
}

// ── PollDriver: single-flight poll loop ──────────────────────────────────────

/// A single-flight guard: at most one holder at a time. `PollDriver` uses it so
/// a slow poll never overlaps the next tick (the leak-prone re-entrancy the
/// throttle-map fix addressed, generalized here).
#[derive(Default)]
pub struct SingleFlight {
    busy: AtomicBool,
}

/// RAII guard from [`SingleFlight::try_enter`]; releases on drop.
pub struct FlightGuard<'a>(&'a AtomicBool);

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SingleFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter the critical section, or `None` if another holder is active.
    pub fn try_enter(&self) -> Option<FlightGuard<'_>> {
        match self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Some(FlightGuard(&self.busy)),
            Err(_) => None,
        }
    }
}

/// Owns a fixed-interval, single-flight poll loop that reconciles a
/// [`PollInventory`] each cycle. The plugin supplies an async `poll` closure
/// returning the full current snapshot; the driver reconciles it. Single-flight
/// is inherent to the sequential loop (a cycle awaits to completion before the
/// next), and [`SingleFlight`] additionally rejects any concurrent external tick.
pub struct PollDriver {
    interval: Duration,
    flight: SingleFlight,
}

impl PollDriver {
    /// A driver polling every `interval`.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            flight: SingleFlight::new(),
        }
    }

    /// Run one poll → reconcile cycle, skipped (returning `false`) if a poll is
    /// already in flight. Pure of the loop/sleep so it is unit-testable.
    pub async fn poll_once<K, V, F, Fut, I>(&self, inv: &PollInventory<K, V>, poll: F) -> bool
    where
        K: Eq + Hash + Clone + Send + 'static,
        V: Send + 'static,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = I>,
        I: IntoIterator<Item = (K, V)>,
    {
        let Some(_guard) = self.flight.try_enter() else {
            return false;
        };
        let snapshot = poll().await;
        inv.reconcile(snapshot);
        true
    }

    /// Drive the loop forever: reconcile, then sleep `interval`. Never overlaps
    /// (each cycle awaits to completion). A plugin spawns this on the shared
    /// reactor. `poll` is called once per cycle.
    pub async fn run<K, V, F, Fut, I>(&self, inv: &PollInventory<K, V>, mut poll: F) -> !
    where
        K: Eq + Hash + Clone + Send + 'static,
        V: Send + 'static,
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = I>,
        I: IntoIterator<Item = (K, V)>,
    {
        loop {
            self.poll_once(inv, &mut poll).await;
            crate::time::sleep(self.interval).await;
        }
    }
}

// ── Runtime stats substrate ──────────────────────────────────────────────────

/// jemalloc heap stats, in bytes, as reported by mallctl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JemallocStats {
    pub allocated: usize,
    pub active: usize,
    pub resident: usize,
    pub retained: usize,
    pub mapped: usize,
}

/// A source of allocator stats. The real source reads jemalloc via mallctl
/// (`jemalloc-stats` feature); tests substitute a mock so the finding shape is
/// verifiable without a live allocator.
pub trait StatsSource {
    fn read(&self) -> Option<JemallocStats>;
}

/// Live jemalloc stats via `tikv-jemalloc-ctl` (`jemalloc-stats` feature). Off
/// the feature, [`read`](StatsSource::read) returns `None` and the provider
/// degrades gracefully.
pub struct JemallocSource;

impl StatsSource for JemallocSource {
    #[cfg(feature = "jemalloc-stats")]
    fn read(&self) -> Option<JemallocStats> {
        use tikv_jemalloc_ctl::{epoch, stats};
        // Refresh the cached stats epoch, then read each counter.
        epoch::advance().ok()?;
        Some(JemallocStats {
            allocated: stats::allocated::read().ok()?,
            active: stats::active::read().ok()?,
            resident: stats::resident::read().ok()?,
            retained: stats::retained::read().ok()?,
            mapped: stats::mapped::read().ok()?,
        })
    }

    #[cfg(not(feature = "jemalloc-stats"))]
    fn read(&self) -> Option<JemallocStats> {
        None
    }
}

/// Count this process's open file descriptors (Linux: entries in `/proc/self/fd`).
/// `None` off Linux.
pub fn open_fd_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/fd")
            .ok()
            .map(|d| d.count().saturating_sub(1)) // minus the readdir handle itself
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// This process's resident set size in bytes (Linux: `/proc/self/statm`).
/// `None` off Linux.
pub fn process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page = libc_sysconf_pagesize();
        Some(rss_pages.saturating_mul(page))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn libc_sysconf_pagesize() -> u64 {
    // 4 KiB is the near-universal Linux page size; reading `_SC_PAGESIZE` would
    // add a libc dep for a value that is constant on every host orca runs.
    4096
}

// ── Auto-diagnostics provider (opt-in, gated on `tools`) ─────────────────────

/// The built-in instrumentation diagnostics provider. Advertised by the serve
/// loop ONLY when [`enabled`] is true, so a stock plugin registers nothing.
#[cfg(feature = "tools")]
pub mod diag {
    // The diagnostics dispatch mirrors the serve loop's `BackendDispatch` wire
    // boundary: args/results cross as `serde_json::Value` (the wire's own type),
    // typed against the diagnostics schemas at the ends. Sanctioned opaque seam,
    // scoped to this dispatch shim — the same allowance `serve.rs` carries.
    #![allow(clippy::disallowed_types)]
    use super::*;
    use crate::contract::diagnostics::{DIAGNOSE_OP, DiagnoseArgs, REPAIR_OP, RepairArgs};
    use crate::contract::{Finding, RepairOutcome, Severity};
    use crate::serde_json::{self, Value};

    /// Diagnostics-provider registry name for a plugin's built-in instrumentation
    /// provider (distinct from any provider the plugin already ships).
    pub fn provider_name(plugin: &str) -> String {
        format!("{plugin}-instrument")
    }

    /// Invoke prefix the core diagnostics proxy calls back through.
    pub fn invoke_prefix(plugin: &str) -> String {
        format!("{plugin}.__orca_diag")
    }

    /// The `diagnostics` [`BackendDef`](crate::abi::BackendDef) JSON value the
    /// serve loop appends to the plugin's advertised backends when instrumentation
    /// is active.
    pub fn backend_def(plugin: &str) -> Value {
        serde_json::json!({
            "domain": "diagnostics",
            "name": provider_name(plugin),
            "invoke_prefix": invoke_prefix(plugin),
        })
    }

    /// Build the instrumentation findings for `plugin` from a stats source.
    pub fn findings(plugin: &str, stats: &dyn StatsSource) -> Vec<Finding> {
        let provider = provider_name(plugin);
        let mut out = Vec::new();

        if let Some(s) = stats.read() {
            out.push(Finding {
                id: "jemalloc".into(),
                provider: provider.clone(),
                severity: Severity::Info,
                title: "jemalloc heap stats".into(),
                detail: format!(
                    "allocated={} active={} resident={} retained={} mapped={} (bytes)",
                    s.allocated, s.active, s.resident, s.retained, s.mapped
                ),
                repair: None,
            });
        }

        if let Some(fds) = open_fd_count() {
            out.push(Finding {
                id: "open-fds".into(),
                provider: provider.clone(),
                severity: Severity::Info,
                title: "open file descriptors".into(),
                detail: format!("{fds} open fds"),
                repair: None,
            });
        }

        if let Some(rss) = process_rss_bytes() {
            out.push(Finding {
                id: "rss".into(),
                provider: provider.clone(),
                severity: Severity::Info,
                title: "process RSS".into(),
                detail: format!("{rss} bytes resident"),
                repair: None,
            });
        }

        for inv in inventory_stats() {
            let cap = inv
                .capacity
                .map(|c| format!(", capacity={c}"))
                .unwrap_or_default();
            let near_cap = inv.capacity.is_some_and(|c| c > 0 && inv.len >= c);
            out.push(Finding {
                id: format!("poll-inventory:{}", inv.name),
                provider: provider.clone(),
                severity: if near_cap {
                    Severity::Warn
                } else {
                    Severity::Info
                },
                title: format!("poll inventory '{}'", inv.name),
                detail: format!("len={}{cap}", inv.len),
                repair: None,
            });
        }

        out
    }

    /// Answer `<plugin>.__orca_diag.{diagnose,repair}` — the ops the core
    /// diagnostics proxy invokes. Returns `None` for any tool outside this
    /// prefix, so the serve loop falls through to normal dispatch.
    pub fn dispatch(plugin: &str, tool: &str, _args: Value) -> Option<Result<Value, Value>> {
        let op = tool
            .strip_prefix(&invoke_prefix(plugin))
            .and_then(|r| r.strip_prefix('.'))?;
        Some(match op {
            DIAGNOSE_OP => {
                // Args are a DiagnoseArgs filter; we ignore the provider filter
                // (core already routes to us) and always return our findings.
                let _: DiagnoseArgs = serde_json::from_value(_args).unwrap_or_default();
                let findings = findings(plugin, &JemallocSource);
                serde_json::to_value(findings).map_err(|e| Value::String(e.to_string()))
            }
            REPAIR_OP => {
                // Instrumentation findings are read-only observations; there is
                // nothing to repair. Return a benign no-op outcome.
                let args: RepairArgs = match serde_json::from_value(_args) {
                    Ok(a) => a,
                    Err(e) => return Some(Err(Value::String(e.to_string()))),
                };
                let outcome = RepairOutcome {
                    id: args.repair_id,
                    provider: provider_name(plugin),
                    ok: false,
                    message: "instrumentation findings are observational; nothing to repair".into(),
                };
                serde_json::to_value(outcome).map_err(|e| Value::String(e.to_string()))
            }
            other => Err(Value::String(format!(
                "unknown instrumentation diagnostics op '{other}'"
            ))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_adds_replaces_and_drops_absent() {
        let inv: PollInventory<u32, &str> = PollInventory::new("t-reconcile");
        inv.reconcile([(1, "a"), (2, "b")]);
        assert_eq!(inv.len(), 2);
        // Next cycle: key 1 kept+updated, 2 dropped, 3 added.
        inv.reconcile([(1, "a2"), (3, "c")]);
        assert_eq!(inv.len(), 2);
        assert_eq!(inv.with(&1, |v| *v), Some("a2"));
        assert_eq!(inv.with(&2, |v| *v), None, "absent key dropped");
        assert_eq!(inv.with(&3, |v| *v), Some("c"));
    }

    #[test]
    fn capacity_evicts_oldest() {
        let inv: PollInventory<u32, u32> = PollInventory::new("t-cap").with_capacity(2);
        inv.reconcile([(1, 1), (2, 2), (3, 3)]);
        assert_eq!(inv.len(), 2, "capacity caps the set");
    }

    #[test]
    fn insert_capacity_evicts_oldest_first() {
        let inv: PollInventory<u32, u32> = PollInventory::new("t-insert-cap").with_capacity(2);
        inv.insert(1, 10);
        inv.insert(2, 20);
        inv.insert(3, 30);
        assert_eq!(inv.len(), 2);
        assert_eq!(inv.with(&1, |v| *v), None, "oldest evicted");
        assert_eq!(inv.with(&3, |v| *v), Some(30));
    }

    #[test]
    fn ttl_expires_entries() {
        let inv: PollInventory<u32, u32> =
            PollInventory::new("t-ttl").with_ttl(Duration::from_millis(20));
        inv.insert(1, 1);
        assert_eq!(inv.len(), 1);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(inv.len(), 0, "expired entry pruned on read");
    }

    #[test]
    fn single_flight_rejects_concurrent_entry() {
        let sf = SingleFlight::new();
        let g1 = sf.try_enter();
        assert!(g1.is_some(), "first entry admitted");
        assert!(sf.try_enter().is_none(), "second concurrent entry rejected");
        drop(g1);
        assert!(sf.try_enter().is_some(), "entry available after release");
    }

    #[tokio::test]
    async fn poll_driver_skips_when_in_flight() {
        let inv: PollInventory<u32, u32> = PollInventory::new("t-driver");
        let driver = PollDriver::new(Duration::from_secs(3600));
        let ran = driver
            .poll_once(&inv, || async { vec![(1u32, 1u32)] })
            .await;
        assert!(ran);
        assert_eq!(inv.len(), 1);

        // Hold the flight to simulate an overlapping tick → the poll is skipped.
        let _held = driver.flight.try_enter().unwrap();
        let polled = AtomicBool::new(false);
        let ran2 = driver
            .poll_once(&inv, || {
                polled.store(true, Ordering::SeqCst);
                async { Vec::<(u32, u32)>::new() }
            })
            .await;
        assert!(!ran2, "overlapping poll skipped");
        assert!(!polled.load(Ordering::SeqCst), "poll closure not invoked");
    }

    #[test]
    fn inventory_registry_reports_live_and_prunes_dropped() {
        let inv: PollInventory<u32, u32> = PollInventory::new("t-registry-live");
        inv.insert(1, 1);
        inv.insert(2, 2);
        let stats = inventory_stats();
        let mine = stats.iter().find(|s| s.name == "t-registry-live");
        assert_eq!(mine.map(|s| s.len), Some(2));
        drop(inv);
        let after = inventory_stats();
        assert!(
            !after.iter().any(|s| s.name == "t-registry-live"),
            "dropped inventory pruned from registry"
        );
    }

    #[test]
    fn enabled_reads_the_marker() {
        // Off by default in the test process (marker unset or not "1").
        // Guard against a parallel test setting it: only assert the exact-match rule.
        assert!(!super::enabled() || std::env::var(INSTRUMENT_ENV).as_deref() == Ok("1"));
    }
}

#[cfg(all(test, feature = "tools"))]
mod diag_tests {
    use super::*;
    use crate::contract::Severity;

    struct MockStats(Option<JemallocStats>);
    impl StatsSource for MockStats {
        fn read(&self) -> Option<JemallocStats> {
            self.0
        }
    }

    #[test]
    fn findings_shape_includes_jemalloc_from_mock_source() {
        let src = MockStats(Some(JemallocStats {
            allocated: 100,
            active: 200,
            resident: 300,
            retained: 40,
            mapped: 500,
        }));
        let findings = diag::findings("demo", &src);
        let jem = findings
            .iter()
            .find(|f| f.id == "jemalloc")
            .expect("jemalloc finding present");
        assert_eq!(jem.provider, "demo-instrument");
        assert_eq!(jem.severity, Severity::Info);
        assert!(jem.detail.contains("retained=40"), "{}", jem.detail);
    }

    #[test]
    fn findings_omit_jemalloc_when_source_none() {
        let findings = diag::findings("demo", &MockStats(None));
        assert!(!findings.iter().any(|f| f.id == "jemalloc"));
    }

    #[test]
    fn dispatch_ignores_foreign_tool() {
        let out = diag::dispatch(
            "demo",
            "service.__backend.demo.status",
            serde_json::json!({}),
        );
        assert!(out.is_none());
    }

    #[test]
    fn dispatch_answers_diagnose() {
        let out = diag::dispatch("demo", "demo.__orca_diag.diagnose", serde_json::json!({}))
            .expect("owned")
            .expect("ok");
        assert!(out.is_array(), "diagnose returns a Finding array");
    }
}
