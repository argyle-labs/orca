use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use derive::orca_tool;
use notifications::{Event, EventClass, Severity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::periodic::{self, PeriodicSpec};

pub const CONFIG_KEY: &str = "storage.quota.config";
const DEFAULT_FLOOR_PCT: u8 = 15;
const MAX_FLOOR_PCT: u8 = 90;
const TICK_INTERVAL: Duration = Duration::from_secs(1800);
const WALK_WORKERS: usize = 4;
const LOCK_FILES: &[&str] = &[".cargo-lock", ".package-cache", ".lock"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PathClass {
    CargoTarget,
    CiCache,
    Tmp,
    VmDisk,
}

impl PathClass {
    pub fn rebuildable(self) -> bool {
        !matches!(self, Self::VmDisk)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct QuotaConfig {
    /// Run the janitor on the scheduler. The verbs work whenever budgets exist.
    pub enabled: bool,
    /// Directories every root must resolve strictly inside.
    pub bases: Vec<String>,
    /// Minimum free space per volume, percent of its size. Default 15.
    pub floor_pct: Option<u8>,
    pub budgets: Vec<ClassBudget>,
}

impl QuotaConfig {
    pub fn floor_pct(&self) -> u8 {
        self.floor_pct
            .unwrap_or(DEFAULT_FLOOR_PCT)
            .min(MAX_FLOOR_PCT)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassBudget {
    pub class: PathClass,
    pub budget_bytes: u64,
    /// Paths or globs (`~` expanded), e.g. `~/code/*/target`.
    pub roots: Vec<String>,
    /// Member id this budget applies to; absent applies to every member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RefusedRoot {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
struct Unit {
    path: PathBuf,
    root: PathBuf,
    class_idx: usize,
    volume: Option<String>,
    bytes: u64,
    newest_unix: u64,
    locked: bool,
    rebuildable: bool,
}

#[derive(Clone, Debug)]
struct ClassScan {
    class: PathClass,
    budget_bytes: u64,
    roots: Vec<PathBuf>,
    refused: Vec<RefusedRoot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Volume {
    total: u64,
    avail: u64,
}

#[derive(Debug, Default)]
struct Scan {
    classes: Vec<ClassScan>,
    units: Vec<Unit>,
    volumes: HashMap<String, Volume>,
}

fn applies_to(member: Option<&str>, local: &str) -> bool {
    member.is_none_or(|m| m == local)
}

fn canonical_bases(bases: &[String]) -> Vec<PathBuf> {
    bases
        .iter()
        .filter_map(|b| std::fs::canonicalize(utils::path::expand_tilde(b)).ok())
        .filter(|b| b.parent().is_some())
        .collect()
}

fn check_root(root: &Path, bases: &[PathBuf], home: Option<&Path>) -> Result<(), &'static str> {
    if root.parent().is_none() {
        return Err("resolves to the filesystem root");
    }
    if let Some(home) = home
        && home.starts_with(root)
    {
        return Err("resolves to the home directory or one of its ancestors");
    }
    if !bases.iter().any(|b| root.starts_with(b) && root != b) {
        return Err("outside every configured base");
    }
    Ok(())
}

fn resolve_roots(
    patterns: &[String],
    bases: &[PathBuf],
    home: Option<&Path>,
) -> (Vec<PathBuf>, Vec<RefusedRoot>) {
    let mut roots = Vec::new();
    let mut refused = Vec::new();
    for pattern in patterns {
        let expanded = utils::path::expand_tilde(pattern);
        let matches = match glob::glob(&expanded) {
            Ok(m) => m,
            Err(e) => {
                refused.push(RefusedRoot {
                    path: pattern.clone(),
                    reason: format!("invalid glob: {e}"),
                });
                continue;
            }
        };
        for found in matches.flatten() {
            let Ok(canon) = std::fs::canonicalize(&found) else {
                continue;
            };
            match check_root(&canon, bases, home) {
                Ok(()) => {
                    if !roots.contains(&canon) {
                        roots.push(canon);
                    }
                }
                Err(reason) => refused.push(RefusedRoot {
                    path: found.display().to_string(),
                    reason: reason.to_string(),
                }),
            }
        }
    }
    (roots, refused)
}

fn try_lock_all(paths: &[PathBuf]) -> Option<Vec<File>> {
    let mut held = Vec::new();
    for p in paths {
        let Ok(f) = File::open(p) else { continue };
        match f.try_lock() {
            Ok(()) => held.push(f),
            Err(_) => return None,
        }
    }
    Some(held)
}

fn lock_files_in(dir: &Path) -> Vec<PathBuf> {
    LOCK_FILES.iter().map(|n| dir.join(n)).collect()
}

fn unit_lock_files(root: &Path, unit: &Path) -> Vec<PathBuf> {
    let mut out = lock_files_in(root);
    out.extend(lock_files_in(unit));
    if let Ok(rd) = std::fs::read_dir(unit) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                out.extend(lock_files_in(&e.path()));
            }
        }
    }
    out
}

#[cfg(unix)]
fn dev_of(md: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(md.dev())
}

#[cfg(not(unix))]
fn dev_of(_md: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn disk_bytes(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn disk_bytes(md: &std::fs::Metadata) -> u64 {
    md.len()
}

fn mtime_unix(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn measure(path: &Path, dev: Option<u64>) -> (u64, u64, bool) {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return (0, 0, false);
    };
    if dev.is_some() && dev_of(&md) != dev {
        return (0, 0, true);
    }
    let mut bytes = disk_bytes(&md);
    let mut newest = mtime_unix(&md);
    let mut crosses = false;
    if !md.is_dir() {
        return (bytes, newest, crosses);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(md) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if dev.is_some() && dev_of(&md) != dev {
                crosses = true;
                continue;
            }
            bytes = bytes.saturating_add(disk_bytes(&md));
            newest = newest.max(mtime_unix(&md));
            if md.is_dir() {
                stack.push(e.path());
            }
        }
    }
    (bytes, newest, crosses)
}

fn units_of(class: PathClass, root: &Path) -> Vec<PathBuf> {
    if !class.rebuildable() {
        return vec![root.to_path_buf()];
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| !t.is_symlink())
                && !LOCK_FILES.iter().any(|n| e.file_name() == *n)
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

fn volume_table() -> Vec<(PathBuf, Volume)> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| {
            (
                d.mount_point().to_path_buf(),
                Volume {
                    total: d.total_space(),
                    avail: d.available_space(),
                },
            )
        })
        .collect()
}

fn volume_for(path: &Path, table: &[(PathBuf, Volume)]) -> Option<(String, Volume)> {
    table
        .iter()
        .filter(|(m, _)| path.starts_with(m))
        .max_by_key(|(m, _)| m.as_os_str().len())
        .map(|(m, v)| (m.display().to_string(), *v))
}

struct Pending {
    path: PathBuf,
    root: PathBuf,
    class_idx: usize,
    class: PathClass,
    dev: Option<u64>,
}

fn scan(cfg: &QuotaConfig, local: &str, home: Option<&Path>) -> Scan {
    let bases = canonical_bases(&cfg.bases);
    let table = volume_table();
    let mut out = Scan::default();
    let mut seen = HashSet::new();
    let mut pending: Vec<Pending> = Vec::new();
    for b in cfg
        .budgets
        .iter()
        .filter(|b| applies_to(b.member.as_deref(), local))
    {
        let (roots, refused) = resolve_roots(&b.roots, &bases, home);
        let class_idx = out.classes.len();
        for root in &roots {
            if let Some((name, vol)) = volume_for(root, &table) {
                out.volumes.insert(name, vol);
            }
            let dev = std::fs::symlink_metadata(root)
                .ok()
                .and_then(|m| dev_of(&m));
            for unit in units_of(b.class, root) {
                if seen.insert(unit.clone()) {
                    pending.push(Pending {
                        path: unit,
                        root: root.clone(),
                        class_idx,
                        class: b.class,
                        dev,
                    });
                }
            }
        }
        out.classes.push(ClassScan {
            class: b.class,
            budget_bytes: b.budget_bytes,
            roots,
            refused,
        });
    }

    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(pending.len()));
    std::thread::scope(|s| {
        for _ in 0..WALK_WORKERS.min(pending.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(Pending {
                        path,
                        root,
                        class_idx,
                        class,
                        dev,
                    }) = pending.get(i)
                    else {
                        break;
                    };
                    let (bytes, newest_unix, crosses) = measure(path, *dev);
                    let locked =
                        class.rebuildable() && try_lock_all(&unit_lock_files(root, path)).is_none();
                    let unit = Unit {
                        path: path.clone(),
                        root: root.clone(),
                        class_idx: *class_idx,
                        volume: volume_for(path, &table).map(|(n, _)| n),
                        bytes,
                        newest_unix,
                        locked,
                        rebuildable: class.rebuildable() && !crosses,
                    };
                    if let Ok(mut r) = results.lock() {
                        r.push((i, unit));
                    }
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_default();
    results.sort_by_key(|(i, _)| *i);
    out.units = results.into_iter().map(|(_, u)| u).collect();
    out
}

fn floor_bytes(total: u64, floor_pct: u8) -> u64 {
    total / 100 * u64::from(floor_pct)
}

fn plan_evictions(scan: &Scan, floor_pct: u8) -> Vec<usize> {
    let mut chosen: Vec<usize> = Vec::new();
    let mut picked: HashSet<usize> = HashSet::new();
    let mut freed: HashMap<&str, u64> = HashMap::new();

    let mut oldest_first: Vec<usize> = (0..scan.units.len()).collect();
    oldest_first.sort_by_key(|&i| scan.units[i].newest_unix);
    let evictable = |i: usize| scan.units[i].rebuildable && !scan.units[i].locked;

    for (class_idx, class) in scan.classes.iter().enumerate() {
        if !class.class.rebuildable() {
            continue;
        }
        let mut used: u64 = scan
            .units
            .iter()
            .filter(|u| u.class_idx == class_idx)
            .map(|u| u.bytes)
            .sum();
        for &i in &oldest_first {
            if used <= class.budget_bytes {
                break;
            }
            let u = &scan.units[i];
            if u.class_idx != class_idx || !evictable(i) {
                continue;
            }
            used = used.saturating_sub(u.bytes);
            if let Some(v) = &u.volume {
                *freed.entry(v.as_str()).or_default() += u.bytes;
            }
            picked.insert(i);
            chosen.push(i);
        }
    }

    let mut vols: Vec<(&String, &Volume)> = scan.volumes.iter().collect();
    vols.sort_by_key(|(n, _)| n.as_str());
    for (name, vol) in vols {
        let need = floor_bytes(vol.total, floor_pct);
        let mut free = vol
            .avail
            .saturating_add(freed.get(name.as_str()).copied().unwrap_or(0));
        for &i in &oldest_first {
            if free >= need {
                break;
            }
            let u = &scan.units[i];
            if picked.contains(&i) || !evictable(i) || u.volume.as_ref() != Some(name) {
                continue;
            }
            free = free.saturating_add(u.bytes);
            picked.insert(i);
            chosen.push(i);
        }
    }
    chosen
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EvictionOutcome {
    Planned,
    Deleted,
    SkippedLocked,
    SkippedUnsafe,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Eviction {
    pub path: String,
    pub class: PathClass,
    pub bytes: u64,
    pub last_used_unix: u64,
    pub outcome: EvictionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn still_safe(unit: &Unit) -> bool {
    let Ok(root_now) = std::fs::canonicalize(&unit.root) else {
        return false;
    };
    if root_now != unit.root || unit.path.parent() != Some(unit.root.as_path()) {
        return false;
    }
    let (Ok(md), Ok(root_md)) = (
        std::fs::symlink_metadata(&unit.path),
        std::fs::symlink_metadata(&unit.root),
    ) else {
        return false;
    };
    !md.file_type().is_symlink() && dev_of(&md) == dev_of(&root_md)
}

fn evict(unit: &Unit) -> (EvictionOutcome, Option<String>) {
    if !still_safe(unit) {
        return (EvictionOutcome::SkippedUnsafe, None);
    }
    let Some(_guards) = try_lock_all(&unit_lock_files(&unit.root, &unit.path)) else {
        return (EvictionOutcome::SkippedLocked, None);
    };
    let removed = if unit.path.is_dir() {
        std::fs::remove_dir_all(&unit.path)
    } else {
        std::fs::remove_file(&unit.path)
    };
    match removed {
        Ok(()) => (EvictionOutcome::Deleted, None),
        Err(e) => (EvictionOutcome::Failed, Some(e.to_string())),
    }
}

fn apply(scan: &Scan, plan: &[usize], dry_run: bool) -> Vec<Eviction> {
    plan.iter()
        .map(|&i| {
            let u = &scan.units[i];
            let (outcome, error) = if dry_run {
                (EvictionOutcome::Planned, None)
            } else {
                evict(u)
            };
            Eviction {
                path: u.path.display().to_string(),
                class: scan.classes[u.class_idx].class,
                bytes: u.bytes,
                last_used_unix: u.newest_unix,
                outcome,
                error,
            }
        })
        .collect()
}

fn load_config() -> anyhow::Result<Option<QuotaConfig>> {
    let raw = db::pool::with_pooled_or_open(|conn| db::settings::get(conn, CONFIG_KEY))?;
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => Ok(Some(serde_json::from_str(s).map_err(|e| {
            anyhow::anyhow!("settings key `{CONFIG_KEY}` is not a valid quota config: {e}")
        })?)),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").and_then(|h| std::fs::canonicalize(h).ok())
}

async fn scan_local(cfg: QuotaConfig) -> anyhow::Result<Scan> {
    let local = crate::host_identity::machine_id().to_string();
    let home = home_dir();
    Ok(tokio::task::spawn_blocking(move || scan(&cfg, &local, home.as_deref())).await?)
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageQuotaListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClassReport {
    pub class: PathClass,
    pub budget_bytes: u64,
    pub used_bytes: u64,
    pub over_budget: bool,
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<RefusedRoot>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VolumeReport {
    pub mount: String,
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub floor_bytes: u64,
    pub below_floor: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuotaListOutput {
    pub configured: bool,
    pub enabled: bool,
    pub floor_pct: u8,
    pub classes: Vec<ClassReport>,
    pub volumes: Vec<VolumeReport>,
}

fn report(cfg: &QuotaConfig, scan: &Scan) -> StorageQuotaListOutput {
    let floor_pct = cfg.floor_pct();
    let classes = scan
        .classes
        .iter()
        .enumerate()
        .map(|(idx, c)| {
            let used_bytes = scan
                .units
                .iter()
                .filter(|u| u.class_idx == idx)
                .map(|u| u.bytes)
                .sum();
            ClassReport {
                class: c.class,
                budget_bytes: c.budget_bytes,
                used_bytes,
                over_budget: used_bytes > c.budget_bytes,
                roots: c.roots.iter().map(|r| r.display().to_string()).collect(),
                refused: c.refused.clone(),
            }
        })
        .collect();
    let mut volumes: Vec<VolumeReport> = scan
        .volumes
        .iter()
        .map(|(mount, v)| {
            let floor = floor_bytes(v.total, floor_pct);
            VolumeReport {
                mount: mount.clone(),
                total_bytes: v.total,
                avail_bytes: v.avail,
                floor_bytes: floor,
                below_floor: v.avail < floor,
            }
        })
        .collect();
    volumes.sort_by(|a, b| a.mount.cmp(&b.mount));
    StorageQuotaListOutput {
        configured: true,
        enabled: cfg.enabled,
        floor_pct,
        classes,
        volumes,
    }
}

/// Usage of each declared path class against its budget, and free space of every
/// volume a root lives on against the free floor. Reads only.
#[orca_tool(domain = "storage.quota", verb = "list")]
async fn storage_quota_list(
    _args: StorageQuotaListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageQuotaListOutput> {
    let Some(cfg) = load_config()? else {
        return Ok(StorageQuotaListOutput {
            configured: false,
            enabled: false,
            floor_pct: DEFAULT_FLOOR_PCT,
            classes: Vec::new(),
            volumes: Vec::new(),
        });
    };
    let scan = scan_local(cfg.clone()).await?;
    Ok(report(&cfg, &scan))
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageQuotaEnforceArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuotaEnforceOutput {
    pub dry_run: bool,
    pub evictions: Vec<Eviction>,
    /// Bytes freed, or with `dryRun` the bytes that would be freed.
    pub freed_bytes: u64,
}

async fn enforce(cfg: QuotaConfig, dry_run: bool) -> anyhow::Result<StorageQuotaEnforceOutput> {
    let floor_pct = cfg.floor_pct();
    let evictions = tokio::task::spawn_blocking({
        let local = crate::host_identity::machine_id().to_string();
        let home = home_dir();
        move || {
            let scan = scan(&cfg, &local, home.as_deref());
            let plan = plan_evictions(&scan, floor_pct);
            apply(&scan, &plan, dry_run)
        }
    })
    .await?;
    let freed_bytes = evictions
        .iter()
        .filter(|e| {
            matches!(
                e.outcome,
                EvictionOutcome::Deleted | EvictionOutcome::Planned
            )
        })
        .map(|e| e.bytes)
        .sum();
    if !dry_run {
        record(&evictions, freed_bytes).await;
    }
    Ok(StorageQuotaEnforceOutput {
        dry_run,
        evictions,
        freed_bytes,
    })
}

async fn record(evictions: &[Eviction], freed_bytes: u64) {
    for e in evictions {
        let line = format!(
            "[storage.quota] {:?} {} ({} MiB, {:?}){}",
            e.outcome,
            e.path,
            e.bytes / 1_048_576,
            e.class,
            e.error
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        );
        tracing::info!("{}", utils::scrub::scrub_line(&line));
    }
    let deleted = evictions
        .iter()
        .filter(|e| e.outcome == EvictionOutcome::Deleted)
        .count();
    let failed = evictions
        .iter()
        .filter(|e| e.outcome == EvictionOutcome::Failed)
        .count();
    if deleted == 0 && failed == 0 {
        return;
    }
    let host = sysinfo::System::host_name().unwrap_or_else(|| "this host".to_string());
    let severity = if failed > 0 {
        Severity::Warn
    } else {
        Severity::Info
    };
    let body = utils::scrub::scrub_line(&format!(
        "Storage janitor on {host} evicted {deleted} item(s), freeing {} MiB; {failed} failed.",
        freed_bytes / 1_048_576
    ))
    .into_owned();
    let event = Event::new(
        EventClass::Alert,
        severity,
        format!(
            "storage quota janitor freed {} MiB on {host}",
            freed_bytes / 1_048_576
        ),
        "storage.quota.enforce",
    )
    .with_host(host)
    .with_body(body);
    let _ = notifications::emit(&event).await;
}

/// [MUTATES STATE] Delete least-recently-used items in rebuildable path classes
/// (`cargo-target`, `ci-cache`, `tmp`) until every class is within budget and every
/// volume is above the free floor. Never touches anything outside a declared root,
/// and skips items whose cargo/CI lock is held. `dryRun` lists exactly what would
/// be deleted and deletes nothing.
#[orca_tool(
    domain = "storage.quota",
    verb = "enforce",
    role = "admin",
    execute_gated = false,
    dry_run_aware = true
)]
async fn storage_quota_enforce(
    _args: StorageQuotaEnforceArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageQuotaEnforceOutput> {
    let cfg = load_config()?.ok_or_else(|| {
        anyhow::anyhow!("no storage quota configured; set settings key `{CONFIG_KEY}`")
    })?;
    enforce(cfg, ctx.dry_run()).await
}

async fn janitor_tick() -> anyhow::Result<()> {
    let Some(cfg) = load_config()? else {
        return Ok(());
    };
    if !cfg.enabled {
        return Ok(());
    }
    enforce(cfg, false).await.map(|_| ())
}

pub fn spawn_periodic() {
    use std::sync::OnceLock;
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "storage.quota.janitor",
            initial_delay: Duration::from_secs(360),
            interval: TICK_INTERVAL,
        },
        periodic::boxed(janitor_tick),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; bytes]).unwrap();
    }

    fn set_mtime(path: &Path, unix: u64) {
        let t = UNIX_EPOCH + Duration::from_secs(unix);
        for e in std::iter::once(path.to_path_buf()).chain(
            std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path()),
        ) {
            File::options()
                .write(true)
                .open(&e)
                .or_else(|_| File::open(&e))
                .and_then(|f| f.set_modified(t))
                .ok();
        }
    }

    fn cfg(base: &Path, class: PathClass, budget: u64, roots: Vec<String>) -> QuotaConfig {
        QuotaConfig {
            enabled: true,
            bases: vec![base.display().to_string()],
            floor_pct: Some(0),
            budgets: vec![ClassBudget {
                class,
                budget_bytes: budget,
                roots,
                member: None,
            }],
        }
    }

    fn target_with_profiles(base: &Path) -> PathBuf {
        let target = base.join("repo/target");
        for (name, mtime) in [("debug", 3_000), ("release", 1_000), ("prepush", 2_000)] {
            write(&target.join(name).join("deps/blob"), 64 * 1024);
            set_mtime(&target.join(name).join("deps"), mtime);
            set_mtime(&target.join(name), mtime);
        }
        target
    }

    fn names(ev: &[Eviction]) -> Vec<String> {
        ev.iter()
            .map(|e| {
                Path::new(&e.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn budget_accounting_sums_every_unit_of_a_class() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        target_with_profiles(&base);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            u64::MAX,
            vec![format!("{}/*/target", base.display())],
        );
        let s = scan(&c, "m", None);
        let r = report(&c, &s);
        assert_eq!(r.classes.len(), 1);
        assert_eq!(s.units.len(), 3);
        assert!(r.classes[0].used_bytes >= 3 * 64 * 1024);
        assert!(!r.classes[0].over_budget);
    }

    #[test]
    fn evicts_oldest_first_until_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let target = target_with_profiles(&base);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        let s = scan(&c, "m", None);
        let one = s.units[0].bytes;
        let mut c2 = c.clone();
        c2.budgets[0].budget_bytes = one * 2;
        let s2 = scan(&c2, "m", None);
        let ev = apply(&s2, &plan_evictions(&s2, 0), true);
        assert_eq!(names(&ev), vec!["release"]);
        let ev = apply(&s, &plan_evictions(&s, 0), true);
        assert_eq!(names(&ev), vec!["release", "prepush", "debug"]);
    }

    #[test]
    fn dry_run_deletes_nothing_and_execute_deletes_planned() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let target = target_with_profiles(&base);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        let s = scan(&c, "m", None);
        let plan = plan_evictions(&s, 0);
        let ev = apply(&s, &plan, true);
        assert!(ev.iter().all(|e| e.outcome == EvictionOutcome::Planned));
        assert!(target.join("debug").exists());
        assert!(target.join("release").exists());
        let ev = apply(&s, &plan, false);
        assert!(ev.iter().all(|e| e.outcome == EvictionOutcome::Deleted));
        assert!(!target.join("debug").exists());
        assert!(target.exists());
    }

    #[test]
    fn floor_evicts_beyond_budget_until_free_space_recovers() {
        let mk = |name: &str, newest: u64, bytes: u64| Unit {
            path: PathBuf::from(format!("/b/r/{name}")),
            root: PathBuf::from("/b/r"),
            class_idx: 0,
            volume: Some("/v".into()),
            bytes,
            newest_unix: newest,
            locked: false,
            rebuildable: true,
        };
        let s = Scan {
            classes: vec![ClassScan {
                class: PathClass::Tmp,
                budget_bytes: 1_000,
                roots: vec![],
                refused: vec![],
            }],
            units: vec![mk("a", 30, 100), mk("b", 10, 100), mk("c", 20, 100)],
            volumes: HashMap::from([(
                "/v".to_string(),
                Volume {
                    total: 10_000,
                    avail: 1_350,
                },
            )]),
        };
        assert!(plan_evictions(&s, 13).is_empty());
        assert_eq!(plan_evictions(&s, 15), vec![1, 2]);
        assert_eq!(plan_evictions(&s, 90), vec![1, 2, 0]);
    }

    #[test]
    fn non_rebuildable_class_is_reported_never_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        write(&base.join("vm/disk.img"), 4096);
        let c = cfg(
            &base,
            PathClass::VmDisk,
            0,
            vec![base.join("vm/disk.img").display().to_string()],
        );
        let s = scan(&c, "m", None);
        assert!(report(&c, &s).classes[0].over_budget);
        assert!(plan_evictions(&s, 90).is_empty());
    }

    #[test]
    fn unsafe_roots_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let home = base.join("home");
        let outside = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        std::fs::create_dir_all(home.join("code/x")).unwrap();
        std::fs::create_dir_all(outside.join("t")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("t"), base.join("escape")).unwrap();
        let bases = vec![base.clone()];
        let patterns = vec![
            "/".to_string(),
            home.display().to_string(),
            base.display().to_string(),
            outside.join("t").display().to_string(),
            base.join("escape").display().to_string(),
            home.join("code/x").display().to_string(),
        ];
        let (roots, refused) = resolve_roots(&patterns, &bases, Some(&home));
        assert_eq!(roots, vec![home.join("code/x")]);
        #[cfg(unix)]
        assert_eq!(refused.len(), 5);
    }

    #[test]
    fn member_scoped_budget_applies_only_to_its_member() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let target = target_with_profiles(&base);
        let mut c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        c.budgets[0].member = Some("other".into());
        assert!(scan(&c, "m", None).classes.is_empty());
        assert_eq!(scan(&c, "other", None).classes.len(), 1);
    }

    #[test]
    fn held_cargo_lock_skips_unit() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let target = target_with_profiles(&base);
        let lock_path = target.join("release/.cargo-lock");
        write(&lock_path, 0);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        let holder = File::open(&lock_path).unwrap();
        holder.lock().unwrap();
        let s = scan(&c, "m", None);
        let ev = apply(&s, &plan_evictions(&s, 0), false);
        assert_eq!(names(&ev), vec!["prepush", "debug"]);
        assert!(target.join("release").exists());
        drop(holder);
    }

    #[test]
    fn lock_taken_after_scan_skips_at_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let root = base.join("ci");
        write(&root.join("slot1/data"), 1024);
        write(&root.join("slot1/.lock"), 0);
        let c = cfg(
            &base,
            PathClass::CiCache,
            0,
            vec![root.display().to_string()],
        );
        let s = scan(&c, "m", None);
        let plan = plan_evictions(&s, 0);
        let holder = File::open(root.join("slot1/.lock")).unwrap();
        holder.lock().unwrap();
        let ev = apply(&s, &plan, false);
        assert_eq!(ev[0].outcome, EvictionOutcome::SkippedLocked);
        assert!(root.join("slot1/data").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_units_are_never_followed_or_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let keep = base.join("keep/precious");
        write(&keep, 1024);
        let root = base.join("tmpdir");
        write(&root.join("old/file"), 1024);
        std::os::unix::fs::symlink(base.join("keep"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(base.join("keep"), root.join("old/inner")).unwrap();
        let c = cfg(&base, PathClass::Tmp, 0, vec![root.display().to_string()]);
        let s = scan(&c, "m", None);
        let ev = apply(&s, &plan_evictions(&s, 0), false);
        assert_eq!(names(&ev), vec!["old"]);
        assert!(keep.exists());
        assert!(root.join("link").exists());
    }
}
