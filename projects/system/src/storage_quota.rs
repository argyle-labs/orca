use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use derive::orca_tool;
use notifications::{Event, EventClass, Severity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::periodic::{self, PeriodicSpec};

pub const CONFIG_KEY: &str = "storage.quota.config";
pub const OPT_IN_MARKER: &str = ".orca-quota-ok";
const DEFAULT_FLOOR_PCT: u8 = 15;
const MAX_FLOOR_PCT: u8 = 90;
const DEFAULT_FLOOR_MIN_AGE_SECS: u64 = 6 * 3600;
const DEFAULT_FLOOR_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;
const DEFAULT_FLOOR_MAX_UNITS: u32 = 20;
const TICK_INTERVAL: Duration = Duration::from_secs(1800);
const LIST_TTL: Duration = Duration::from_secs(60);
const WALK_WORKERS: usize = 4;
const LOCK_FILES: &[&str] = &[".cargo-lock", ".package-cache", ".lock"];
const CARGO_MARKERS: &[&str] = &["CACHEDIR.TAG", ".rustc_info.json"];
const SYSTEM_DIRS: &[&str] = &[
    "/Applications",
    "/Library",
    "/System",
    "/bin",
    "/boot",
    "/cores",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/opt/homebrew",
    "/private/etc",
    "/private/var",
    "/proc",
    "/sbin",
    "/sys",
    "/usr",
    "/var",
];

static ENFORCE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static LIST_CACHE: Mutex<Option<(Instant, String, StorageQuotaListOutput)>> = Mutex::new(None);
static LIST_GENERATION: AtomicU64 = AtomicU64::new(0);

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

#[derive(clap::Args, Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct QuotaConfig {
    /// Run the janitor on the scheduler. The verbs work whenever budgets exist.
    #[arg(long, default_value_t = false)]
    pub enabled: bool,
    /// Directories every root must resolve strictly inside. `/`, `$HOME`, its
    /// ancestors and system directories are refused.
    #[arg(long)]
    pub bases: Vec<String>,
    /// Minimum free space per volume, percent of its size. Default 15, max 90.
    #[arg(long)]
    pub floor_pct: Option<u8>,
    /// Floor eviction only takes items untouched for at least this long. Default 6h.
    #[arg(long)]
    pub floor_min_age_secs: Option<u64>,
    /// Most bytes floor eviction may delete per run. Default 50 GiB.
    #[arg(long)]
    pub floor_max_bytes: Option<u64>,
    /// Most items floor eviction may delete per run. Default 20.
    #[arg(long)]
    pub floor_max_units: Option<u32>,
    /// Per-class budgets. On the CLI, one JSON object per flag:
    /// `--budgets '{"class":"cargo-target","budgetBytes":1,"roots":["~/code/*/target"]}'`.
    #[arg(long, value_parser = parse_budget)]
    pub budgets: Vec<ClassBudget>,
}

impl QuotaConfig {
    pub fn floor_pct(&self) -> u8 {
        self.floor_pct
            .unwrap_or(DEFAULT_FLOOR_PCT)
            .min(MAX_FLOOR_PCT)
    }

    fn floor_limits(&self) -> FloorLimits {
        FloorLimits {
            floor_pct: self.floor_pct(),
            min_age_secs: self
                .floor_min_age_secs
                .unwrap_or(DEFAULT_FLOOR_MIN_AGE_SECS),
            max_bytes: self.floor_max_bytes.unwrap_or(DEFAULT_FLOOR_MAX_BYTES),
            max_units: self.floor_max_units.unwrap_or(DEFAULT_FLOOR_MAX_UNITS),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassBudget {
    pub class: PathClass,
    pub budget_bytes: u64,
    /// Paths or globs (`~` expanded), e.g. `~/code/*/target`. A `cargo-target` root
    /// needs `CACHEDIR.TAG` or `.rustc_info.json`; `ci-cache` and `tmp` roots need
    /// an `.orca-quota-ok` marker file.
    pub roots: Vec<String>,
    /// Member id this budget applies to; absent applies to every member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

fn parse_budget(s: &str) -> Result<ClassBudget, String> {
    serde_json::from_str(s).map_err(|e| format!("invalid budget JSON: {e}"))
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RefusedRoot {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug)]
struct FloorLimits {
    floor_pct: u8,
    min_age_secs: u64,
    max_bytes: u64,
    max_units: u32,
}

type FileId = (u64, u64);

#[derive(Clone, Debug)]
struct Unit {
    path: PathBuf,
    name: OsString,
    id: Option<FileId>,
    root: PathBuf,
    root_id: Option<FileId>,
    class_idx: usize,
    volume: Option<String>,
    bytes: u64,
    newest_unix: u64,
    locked: bool,
    crosses: bool,
    rebuildable: bool,
}

impl Unit {
    fn evictable(&self) -> bool {
        self.rebuildable && !self.locked && !self.crosses
    }
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
    refused_bases: Vec<RefusedRoot>,
    classes: Vec<ClassScan>,
    units: Vec<Unit>,
    volumes: HashMap<String, Volume>,
}

fn applies_to(member: Option<&str>, local: &str) -> bool {
    member.is_none_or(|m| m == local)
}

fn temp_roots() -> Vec<PathBuf> {
    [std::env::temp_dir(), PathBuf::from("/tmp")]
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
}

fn check_base(base: &Path, home: Option<&Path>, temps: &[PathBuf]) -> Result<(), &'static str> {
    if base.parent().is_none() {
        return Err("is the filesystem root");
    }
    if let Some(home) = home
        && home.starts_with(base)
    {
        return Err("is the home directory or one of its ancestors");
    }
    if temps.iter().any(|t| base.starts_with(t)) {
        return Ok(());
    }
    if SYSTEM_DIRS
        .iter()
        .map(Path::new)
        .any(|s| base.starts_with(s) || s.starts_with(base))
    {
        return Err("is or contains a system directory");
    }
    Ok(())
}

fn resolve_bases(bases: &[String], home: Option<&Path>) -> (Vec<PathBuf>, Vec<RefusedRoot>) {
    let temps = temp_roots();
    let mut ok = Vec::new();
    let mut refused = Vec::new();
    for b in bases {
        let reason = match std::fs::canonicalize(utils::path::expand_tilde(b)) {
            Err(_) => "does not exist",
            Ok(canon) => match check_base(&canon, home, &temps) {
                Ok(()) => {
                    ok.push(canon);
                    continue;
                }
                Err(r) => r,
            },
        };
        refused.push(RefusedRoot {
            path: b.clone(),
            reason: reason.to_string(),
        });
    }
    (ok, refused)
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

fn is_plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn has_class_marker(class: PathClass, root: &Path) -> bool {
    match class {
        PathClass::CargoTarget => CARGO_MARKERS.iter().any(|m| is_plain_file(&root.join(m))),
        PathClass::CiCache | PathClass::Tmp => is_plain_file(&root.join(OPT_IN_MARKER)),
        PathClass::VmDisk => true,
    }
}

fn resolve_roots(
    class: PathClass,
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
            let verdict = check_root(&canon, bases, home).and_then(|()| {
                if has_class_marker(class, &canon) {
                    Ok(())
                } else {
                    Err("missing the class marker file")
                }
            });
            match verdict {
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

#[cfg(unix)]
fn any_lock_held(paths: &[PathBuf]) -> bool {
    paths.iter().any(|p| {
        matches!(
            fd_remove::lock_at(rustix::fs::CWD, p.as_path()),
            fd_remove::LockProbe::Held
        )
    })
}

#[cfg(not(unix))]
fn any_lock_held(paths: &[PathBuf]) -> bool {
    paths
        .iter()
        .any(|p| std::fs::File::open(p).is_ok_and(|f| f.try_lock().is_err()))
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
fn id_of(md: &std::fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some((md.dev(), md.ino()))
}

#[cfg(not(unix))]
fn id_of(_md: &std::fs::Metadata) -> Option<FileId> {
    None
}

fn dev_of(md: &std::fs::Metadata) -> Option<u64> {
    id_of(md).map(|(d, _)| d)
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

struct Measured {
    id: Option<FileId>,
    bytes: u64,
    newest: u64,
    crosses: bool,
}

fn measure(path: &Path, dev: Option<u64>) -> Measured {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return Measured {
            id: None,
            bytes: 0,
            newest: 0,
            crosses: false,
        };
    };
    let mut out = Measured {
        id: id_of(&md),
        bytes: disk_bytes(&md),
        newest: mtime_unix(&md),
        crosses: dev.is_some() && dev_of(&md) != dev,
    };
    if out.crosses || !md.is_dir() {
        return out;
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
                out.crosses = true;
                continue;
            }
            out.bytes = out.bytes.saturating_add(disk_bytes(&md));
            out.newest = out.newest.max(mtime_unix(&md));
            if md.is_dir() {
                stack.push(e.path());
            }
        }
    }
    out
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
            let Ok(t) = e.file_type() else { return false };
            let name = e.file_name();
            match class {
                PathClass::CargoTarget => t.is_dir(),
                _ => {
                    !t.is_symlink()
                        && name != OPT_IN_MARKER
                        && !LOCK_FILES.iter().any(|n| name == *n)
                }
            }
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
    root_id: Option<FileId>,
    class_idx: usize,
    class: PathClass,
}

fn scan(cfg: &QuotaConfig, local: &str, home: Option<&Path>, probe_locks: bool) -> Scan {
    let (bases, refused_bases) = resolve_bases(&cfg.bases, home);
    let table = volume_table();
    let mut out = Scan {
        refused_bases,
        ..Scan::default()
    };
    let mut seen = HashSet::new();
    let mut pending: Vec<Pending> = Vec::new();
    for b in cfg
        .budgets
        .iter()
        .filter(|b| applies_to(b.member.as_deref(), local))
    {
        let (roots, refused) = resolve_roots(b.class, &b.roots, &bases, home);
        let class_idx = out.classes.len();
        for root in &roots {
            if let Some((name, vol)) = volume_for(root, &table) {
                out.volumes.insert(name, vol);
            }
            let root_id = std::fs::symlink_metadata(root).ok().and_then(|m| id_of(&m));
            for unit in units_of(b.class, root) {
                if seen.insert(unit.clone()) {
                    pending.push(Pending {
                        path: unit,
                        root: root.clone(),
                        root_id,
                        class_idx,
                        class: b.class,
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
                    let Some(p) = pending.get(i) else {
                        break;
                    };
                    let m = measure(&p.path, p.root_id.map(|(d, _)| d));
                    let locked = probe_locks
                        && p.class.rebuildable()
                        && any_lock_held(&unit_lock_files(&p.root, &p.path));
                    let unit = Unit {
                        path: p.path.clone(),
                        name: p.path.file_name().map(OsString::from).unwrap_or_default(),
                        id: m.id,
                        root: p.root.clone(),
                        root_id: p.root_id,
                        class_idx: p.class_idx,
                        volume: volume_for(&p.path, &table).map(|(n, _)| n),
                        bytes: m.bytes,
                        newest_unix: m.newest,
                        locked,
                        crosses: m.crosses,
                        rebuildable: p.class.rebuildable(),
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

fn class_used(scan: &Scan, class_idx: usize) -> u64 {
    scan.units
        .iter()
        .filter(|u| u.class_idx == class_idx && !u.crosses)
        .map(|u| u.bytes)
        .sum()
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FloorShortfall {
    pub volume: String,
    pub need_bytes: u64,
    pub reachable_bytes: u64,
}

#[derive(Debug, Default)]
struct Plan {
    evict: Vec<usize>,
    shortfalls: Vec<FloorShortfall>,
}

fn plan_evictions(scan: &Scan, limits: FloorLimits, now_unix: u64) -> Plan {
    let mut plan = Plan::default();
    let mut picked: HashSet<usize> = HashSet::new();
    let mut freed: HashMap<&str, u64> = HashMap::new();

    let mut oldest_first: Vec<usize> = (0..scan.units.len()).collect();
    oldest_first.sort_by_key(|&i| scan.units[i].newest_unix);

    for (class_idx, class) in scan.classes.iter().enumerate() {
        if !class.class.rebuildable() {
            continue;
        }
        let mut used = class_used(scan, class_idx);
        for &i in &oldest_first {
            if used <= class.budget_bytes {
                break;
            }
            let u = &scan.units[i];
            if u.class_idx != class_idx || !u.evictable() {
                continue;
            }
            used = used.saturating_sub(u.bytes);
            if let Some(v) = &u.volume {
                *freed.entry(v.as_str()).or_default() += u.bytes;
            }
            picked.insert(i);
            plan.evict.push(i);
        }
    }

    let mut floor_units: u32 = 0;
    let mut floor_spent: u64 = 0;
    let mut vols: Vec<(&String, &Volume)> = scan.volumes.iter().collect();
    vols.sort_by_key(|(n, _)| n.as_str());
    for (name, vol) in vols {
        let need = floor_bytes(vol.total, limits.floor_pct);
        let start = vol
            .avail
            .saturating_add(freed.get(name.as_str()).copied().unwrap_or(0));
        if start >= need {
            continue;
        }
        let mut free = start;
        let mut tentative = Vec::new();
        let (mut units, mut spent) = (floor_units, floor_spent);
        for &i in &oldest_first {
            if free >= need {
                break;
            }
            let u = &scan.units[i];
            if picked.contains(&i)
                || !u.evictable()
                || u.volume.as_ref() != Some(name)
                || now_unix.saturating_sub(u.newest_unix) < limits.min_age_secs
            {
                continue;
            }
            if units >= limits.max_units || spent.saturating_add(u.bytes) > limits.max_bytes {
                break;
            }
            units += 1;
            spent = spent.saturating_add(u.bytes);
            free = free.saturating_add(u.bytes);
            tentative.push(i);
        }
        if free < need {
            plan.shortfalls.push(FloorShortfall {
                volume: name.clone(),
                need_bytes: need,
                reachable_bytes: free,
            });
            continue;
        }
        floor_units = units;
        floor_spent = spent;
        picked.extend(tentative.iter().copied());
        plan.evict.extend(tentative);
    }
    plan
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
    pub freed_bytes: u64,
    pub partial: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type RemoveResult = Result<u64, (EvictionOutcome, String, u64)>;

#[cfg(unix)]
mod fd_remove {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::os::fd::{AsFd, OwnedFd};
    use std::path::Path;

    use rustix::fs::{
        AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat, fstat, openat, statat, unlinkat,
    };
    use rustix::io::Errno;
    use rustix::path::Arg;

    use super::{EvictionOutcome, FileId, LOCK_FILES, RemoveResult};

    const MAX_DEPTH: usize = 512;

    type Step = Result<(), (EvictionOutcome, String)>;

    #[allow(clippy::unnecessary_cast)]
    fn id(st: &Stat) -> FileId {
        (st.st_dev as u64, st.st_ino as u64)
    }

    #[allow(clippy::unnecessary_cast)]
    fn disk_bytes(st: &Stat) -> u64 {
        (st.st_blocks as u64).saturating_mul(512)
    }

    fn is_dir(st: &Stat) -> bool {
        FileType::from_raw_mode(st.st_mode) == FileType::Directory
    }

    fn refuse(why: &str) -> (EvictionOutcome, String) {
        (EvictionOutcome::SkippedUnsafe, why.to_string())
    }

    fn io(e: Errno) -> (EvictionOutcome, String) {
        (EvictionOutcome::Failed, std::io::Error::from(e).to_string())
    }

    fn open_dir<P: Arg>(dir: impl AsFd, p: P) -> Result<OwnedFd, Errno> {
        openat(
            dir,
            p,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }

    fn list(dir: &OwnedFd) -> Result<Vec<CString>, Errno> {
        let mut names: Vec<CString> = Dir::read_from(dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_owned())
            .filter(|n| n.as_bytes() != b"." && n.as_bytes() != b"..")
            .collect();
        names.sort();
        names.reverse();
        Ok(names)
    }

    pub(super) enum LockProbe {
        Absent,
        Held,
        Locked(File),
    }

    pub(super) fn lock_at<P: Arg>(dir: impl AsFd, p: P) -> LockProbe {
        let Ok(fd) = openat(
            dir,
            p,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return LockProbe::Absent;
        };
        match fstat(&fd) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile => {}
            _ => return LockProbe::Absent,
        }
        let f = File::from(fd);
        match f.try_lock() {
            Ok(()) => LockProbe::Locked(f),
            Err(_) => LockProbe::Held,
        }
    }

    fn lock_dir(dir: &OwnedFd, guards: &mut Vec<File>) -> Step {
        for n in LOCK_FILES {
            match lock_at(dir, *n) {
                LockProbe::Absent => {}
                LockProbe::Locked(f) => guards.push(f),
                LockProbe::Held => {
                    return Err((
                        EvictionOutcome::SkippedLocked,
                        "a cargo/CI lock is held".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn remove_unit(
        root: &Path,
        root_id: FileId,
        name: &OsStr,
        unit_id: FileId,
    ) -> RemoveResult {
        let mut freed = 0;
        remove_unit_inner(root, root_id, name, unit_id, &mut freed)
            .map(|()| freed)
            .map_err(|(o, m)| (o, m, freed))
    }

    fn remove_unit_inner(
        root: &Path,
        root_id: FileId,
        name: &OsStr,
        unit_id: FileId,
        freed: &mut u64,
    ) -> Step {
        let rfd = open_dir(CWD, root).map_err(|_| refuse("root is no longer a real directory"))?;
        if id(&fstat(&rfd).map_err(io)?) != root_id {
            return Err(refuse("root changed since the scan"));
        }
        let st = statat(&rfd, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| refuse("item vanished since the scan"))?;
        if id(&st) != unit_id {
            return Err(refuse("item changed since the scan"));
        }
        let mut guards = Vec::new();
        lock_dir(&rfd, &mut guards)?;
        if !is_dir(&st) {
            unlinkat(&rfd, name, AtFlags::empty()).map_err(io)?;
            *freed += disk_bytes(&st);
            return Ok(());
        }
        let ufd = open_dir(&rfd, name).map_err(|_| refuse("item is no longer a real directory"))?;
        if id(&fstat(&ufd).map_err(io)?) != unit_id {
            return Err(refuse("item changed since the scan"));
        }
        lock_dir(&ufd, &mut guards)?;
        for n in list(&ufd).map_err(io)? {
            if let Ok(cfd) = open_dir(&ufd, n.as_c_str()) {
                lock_dir(&cfd, &mut guards)?;
            }
        }
        remove_tree(ufd, unit_id, root_id.0, freed)?;
        unlinkat(&rfd, name, AtFlags::REMOVEDIR).map_err(io)
    }

    struct Frame {
        name: CString,
        id: FileId,
        pending: Vec<CString>,
    }

    fn remove_tree(unit_fd: OwnedFd, unit_id: FileId, dev: u64, freed: &mut u64) -> Step {
        let mut top = list(&unit_fd).map_err(io)?;
        let mut cur = unit_fd;
        let mut stack: Vec<Frame> = Vec::new();
        loop {
            let next = match stack.last_mut() {
                Some(f) => f.pending.pop(),
                None => top.pop(),
            };
            let Some(n) = next else {
                let Some(done) = stack.pop() else {
                    return Ok(());
                };
                let parent = open_dir(&cur, c"..").map_err(io)?;
                let expect = stack.last().map_or(unit_id, |f| f.id);
                if id(&fstat(&parent).map_err(io)?) != expect {
                    return Err(refuse("parent changed during removal"));
                }
                cur = parent;
                unlinkat(&cur, done.name.as_c_str(), AtFlags::REMOVEDIR).map_err(io)?;
                continue;
            };
            let st = statat(&cur, n.as_c_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
            if id(&st).0 != dev {
                return Err(refuse("crosses onto another filesystem"));
            }
            if !is_dir(&st) {
                unlinkat(&cur, n.as_c_str(), AtFlags::empty()).map_err(io)?;
                *freed += disk_bytes(&st);
                continue;
            }
            if stack.len() >= MAX_DEPTH {
                return Err(refuse("directory tree is too deep"));
            }
            let child = open_dir(&cur, n.as_c_str()).map_err(io)?;
            if id(&fstat(&child).map_err(io)?) != id(&st) {
                return Err(refuse("directory changed during removal"));
            }
            let pending = list(&child).map_err(io)?;
            stack.push(Frame {
                name: n,
                id: id(&st),
                pending,
            });
            cur = child;
        }
    }
}

#[cfg(unix)]
fn remove_unit(unit: &Unit) -> RemoveResult {
    match (unit.root_id, unit.id) {
        (Some(root_id), Some(id)) => fd_remove::remove_unit(&unit.root, root_id, &unit.name, id),
        _ => Err((
            EvictionOutcome::SkippedUnsafe,
            "item identity unknown".to_string(),
            0,
        )),
    }
}

#[cfg(not(unix))]
fn remove_unit(_unit: &Unit) -> RemoveResult {
    Err((
        EvictionOutcome::SkippedUnsafe,
        "eviction is supported on unix only".to_string(),
        0,
    ))
}

fn evict(unit: &Unit) -> (EvictionOutcome, Option<String>, u64) {
    match remove_unit(unit) {
        Ok(freed) => (EvictionOutcome::Deleted, None, freed),
        Err((outcome, why, freed)) => (outcome, Some(why), freed),
    }
}

fn apply(scan: &Scan, plan: &[usize], dry_run: bool) -> Vec<Eviction> {
    plan.iter()
        .map(|&i| {
            let u = &scan.units[i];
            let (outcome, error, freed_bytes) = if dry_run {
                (EvictionOutcome::Planned, None, 0)
            } else {
                evict(u)
            };
            Eviction {
                path: u.path.display().to_string(),
                class: scan.classes[u.class_idx].class,
                bytes: u.bytes,
                last_used_unix: u.newest_unix,
                outcome,
                freed_bytes,
                partial: outcome != EvictionOutcome::Deleted && freed_bytes > 0,
                error,
            }
        })
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn load_config() -> anyhow::Result<Option<QuotaConfig>> {
    let raw = db::pool::with_pooled_or_open(|conn| db::settings::get(conn, CONFIG_KEY))?;
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => Ok(Some(serde_json::from_str(s).map_err(|e| {
            anyhow::anyhow!(
                "stored storage quota config is invalid ({e}); rewrite it with storage.quota.config.upsert"
            )
        })?)),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").and_then(|h| std::fs::canonicalize(h).ok())
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageQuotaListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClassReport {
    pub class: PathClass,
    pub budget_bytes: u64,
    pub used_bytes: u64,
    pub over_budget: bool,
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<RefusedRoot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crossing: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VolumeReport {
    pub mount: String,
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub floor_bytes: u64,
    pub below_floor: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuotaListOutput {
    pub configured: bool,
    pub enabled: bool,
    pub floor_pct: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused_bases: Vec<RefusedRoot>,
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
            let used_bytes = class_used(scan, idx);
            ClassReport {
                class: c.class,
                budget_bytes: c.budget_bytes,
                used_bytes,
                over_budget: used_bytes > c.budget_bytes,
                roots: c.roots.iter().map(|r| r.display().to_string()).collect(),
                refused: c.refused.clone(),
                crossing: scan
                    .units
                    .iter()
                    .filter(|u| u.class_idx == idx && u.crosses)
                    .map(|u| u.path.display().to_string())
                    .collect(),
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
        refused_bases: scan.refused_bases.clone(),
        classes,
        volumes,
    }
}

fn cached_list(key: &str) -> Option<StorageQuotaListOutput> {
    let guard = LIST_CACHE.lock().ok()?;
    let (at, k, out) = guard.as_ref()?;
    (k == key && at.elapsed() < LIST_TTL).then(|| out.clone())
}

fn invalidate_list_cache() {
    if let Ok(mut g) = LIST_CACHE.lock() {
        LIST_GENERATION.fetch_add(1, Ordering::SeqCst);
        *g = None;
    }
}

fn store_list(generation: u64, key: String, out: &StorageQuotaListOutput) {
    if let Ok(mut g) = LIST_CACHE.lock()
        && LIST_GENERATION.load(Ordering::SeqCst) == generation
    {
        *g = Some((Instant::now(), key, out.clone()));
    }
}

/// Usage of each declared path class against its budget, and free space of every
/// volume a root lives on against the free floor. Reads only; results are cached
/// for a minute.
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
            refused_bases: Vec::new(),
            classes: Vec::new(),
            volumes: Vec::new(),
        });
    };
    let key = serde_json::to_string(&cfg)?;
    if let Some(hit) = cached_list(&key) {
        return Ok(hit);
    }
    let generation = LIST_GENERATION.load(Ordering::SeqCst);
    let local = crate::host_identity::machine_id().to_string();
    let home = home_dir();
    let out = tokio::task::spawn_blocking(move || {
        let s = scan(&cfg, &local, home.as_deref(), false);
        report(&cfg, &s)
    })
    .await?;
    store_list(generation, key, &out);
    Ok(out)
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageQuotaEnforceArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuotaEnforceOutput {
    pub dry_run: bool,
    pub evictions: Vec<Eviction>,
    pub freed_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub floor_shortfalls: Vec<FloorShortfall>,
}

async fn enforce_with(
    cfg: QuotaConfig,
    local: String,
    home: Option<PathBuf>,
    dry_run: bool,
) -> anyhow::Result<StorageQuotaEnforceOutput> {
    let _serial = ENFORCE_LOCK.lock().await;
    let limits = cfg.floor_limits();
    let (evictions, floor_shortfalls) = tokio::task::spawn_blocking(move || {
        let s = scan(&cfg, &local, home.as_deref(), true);
        let plan = plan_evictions(&s, limits, now_unix());
        (apply(&s, &plan.evict, dry_run), plan.shortfalls)
    })
    .await?;
    let freed_bytes = evictions
        .iter()
        .map(|e| {
            if e.outcome == EvictionOutcome::Planned {
                e.bytes
            } else {
                e.freed_bytes
            }
        })
        .sum();
    if !dry_run {
        invalidate_list_cache();
    }
    Ok(StorageQuotaEnforceOutput {
        dry_run,
        evictions,
        freed_bytes,
        floor_shortfalls,
    })
}

async fn enforce(cfg: QuotaConfig, dry_run: bool) -> anyhow::Result<StorageQuotaEnforceOutput> {
    let local = crate::host_identity::machine_id().to_string();
    let out = enforce_with(cfg, local, home_dir(), dry_run).await?;
    if !dry_run {
        record(&out).await;
    }
    Ok(out)
}

async fn record(out: &StorageQuotaEnforceOutput) {
    for e in &out.evictions {
        let line = format!(
            "[storage.quota] {:?}{} {} ({} of {} MiB freed, {:?}){}",
            e.outcome,
            if e.partial { " partial" } else { "" },
            e.path,
            e.freed_bytes / 1_048_576,
            e.bytes / 1_048_576,
            e.class,
            e.error
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        );
        tracing::info!("{}", utils::scrub::scrub_line(&line));
    }
    for s in &out.floor_shortfalls {
        let line = format!(
            "[storage.quota] floor unreachable on {}: need {} MiB free, eligible items reach {} MiB; floor eviction skipped",
            s.volume,
            s.need_bytes / 1_048_576,
            s.reachable_bytes / 1_048_576
        );
        tracing::warn!("{}", utils::scrub::scrub_line(&line));
    }
    let deleted = out
        .evictions
        .iter()
        .filter(|e| e.outcome == EvictionOutcome::Deleted)
        .count();
    let failed = out
        .evictions
        .iter()
        .filter(|e| e.outcome == EvictionOutcome::Failed)
        .count();
    let partial = out.evictions.iter().filter(|e| e.partial).count();
    let short = out.floor_shortfalls.len();
    if deleted == 0 && failed == 0 && short == 0 && partial == 0 {
        return;
    }
    let host = sysinfo::System::host_name().unwrap_or_else(|| "this host".to_string());
    let severity = if failed > 0 || short > 0 || partial > 0 {
        Severity::Warn
    } else {
        Severity::Info
    };
    let body = utils::scrub::scrub_line(&format!(
        "Storage janitor on {host} evicted {deleted} item(s), freeing {} MiB; {failed} failed ({partial} partially deleted); {short} volume(s) below the free floor with too little eligible data to recover it.",
        out.freed_bytes / 1_048_576
    ))
    .into_owned();
    let event = Event::new(
        EventClass::Alert,
        severity,
        format!(
            "storage quota janitor freed {} MiB on {host}",
            out.freed_bytes / 1_048_576
        ),
        "storage.quota.enforce",
    )
    .with_host(host)
    .with_body(body);
    let _ = notifications::emit(&event).await;
}

/// [MUTATES STATE] Delete least-recently-used items in rebuildable path classes
/// (`cargo-target`, `ci-cache`, `tmp`) until every class is within budget, then
/// (bounded by min age and per-run caps) until every volume is above the free
/// floor. Never touches anything outside a declared root, and skips items whose
/// cargo/CI lock is held. `dryRun` lists exactly what would be deleted and
/// deletes nothing.
///
/// Lock probing is point-in-time: a cargo build that starts after an item's
/// directory is locked for removal can still lose its unit. The min age bounds
/// the exposure to targets idle for at least that long.
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
        anyhow::anyhow!("no storage quota configured; set one with storage.quota.config.upsert")
    })?;
    enforce(cfg, ctx.dry_run()).await
}

fn validate(cfg: &QuotaConfig, home: Option<&Path>) -> anyhow::Result<()> {
    if let Some(p) = cfg.floor_pct
        && p > MAX_FLOOR_PCT
    {
        anyhow::bail!("floorPct {p} exceeds the maximum of {MAX_FLOOR_PCT}");
    }
    if cfg.bases.is_empty() && !cfg.budgets.is_empty() {
        anyhow::bail!("at least one base is required");
    }
    let (_, refused) = resolve_bases(&cfg.bases, home);
    if let Some(r) = refused.first() {
        anyhow::bail!("base `{}` refused: {}", r.path, r.reason);
    }
    for b in &cfg.budgets {
        if b.roots.is_empty() {
            anyhow::bail!("{:?} budget has no roots", b.class);
        }
        for r in &b.roots {
            glob::Pattern::new(&utils::path::expand_tilde(r))
                .map_err(|e| anyhow::anyhow!("root `{r}` is not a valid glob: {e}"))?;
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuotaConfigUpsertOutput {
    pub config: QuotaConfig,
}

/// [MUTATES STATE] Replace this member's storage quota config after validating
/// it: bases may not be `/`, `$HOME`, its ancestors or system directories, the
/// floor is capped at 90%, and every root must be a valid path or glob.
#[orca_tool(domain = "storage.quota.config", verb = "upsert", role = "admin")]
async fn storage_quota_config_upsert(
    args: QuotaConfig,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageQuotaConfigUpsertOutput> {
    validate(&args, home_dir().as_deref())?;
    let json = serde_json::to_string(&args)?;
    db::pool::with_pooled_or_open(|conn| db::settings::set(conn, CONFIG_KEY, &json))?;
    invalidate_list_cache();
    Ok(StorageQuotaConfigUpsertOutput { config: args })
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
    use std::fs::File;

    use super::*;

    const OPEN: FloorLimits = FloorLimits {
        floor_pct: 0,
        min_age_secs: 0,
        max_bytes: u64::MAX,
        max_units: u32::MAX,
    };

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

    fn tmp_base() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        (dir, base)
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
            ..QuotaConfig::default()
        }
    }

    fn target_with_profiles(base: &Path) -> PathBuf {
        let target = base.join("repo/target");
        write(&target.join("CACHEDIR.TAG"), 10);
        for (name, mtime) in [("debug", 3_000), ("release", 1_000), ("prepush", 2_000)] {
            write(&target.join(name).join("deps/blob"), 64 * 1024);
            set_mtime(&target.join(name).join("deps"), mtime);
            set_mtime(&target.join(name), mtime);
        }
        target
    }

    fn opted_in(root: &Path) {
        write(&root.join(OPT_IN_MARKER), 0);
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

    fn synthetic(units: Vec<Unit>, budget: u64, avail: u64) -> Scan {
        Scan {
            classes: vec![ClassScan {
                class: PathClass::Tmp,
                budget_bytes: budget,
                roots: vec![],
                refused: vec![],
            }],
            units,
            volumes: HashMap::from([(
                "/v".to_string(),
                Volume {
                    total: 10_000,
                    avail,
                },
            )]),
            ..Scan::default()
        }
    }

    fn unit(name: &str, newest: u64, bytes: u64) -> Unit {
        Unit {
            path: PathBuf::from(format!("/b/r/{name}")),
            name: name.into(),
            id: None,
            root: PathBuf::from("/b/r"),
            root_id: None,
            class_idx: 0,
            volume: Some("/v".into()),
            bytes,
            newest_unix: newest,
            locked: false,
            crosses: false,
            rebuildable: true,
        }
    }

    #[test]
    fn budget_accounting_sums_every_unit_of_a_class() {
        let (_d, base) = tmp_base();
        target_with_profiles(&base);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            u64::MAX,
            vec![format!("{}/*/target", base.display())],
        );
        let s = scan(&c, "m", None, true);
        let r = report(&c, &s);
        assert_eq!(r.classes.len(), 1);
        assert_eq!(s.units.len(), 3);
        assert!(r.classes[0].used_bytes >= 3 * 64 * 1024);
        assert!(!r.classes[0].over_budget);
    }

    #[test]
    fn evicts_oldest_first_until_under_budget() {
        let (_d, base) = tmp_base();
        let target = target_with_profiles(&base);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        let s = scan(&c, "m", None, true);
        let one = s.units[0].bytes;
        let mut c2 = c.clone();
        c2.budgets[0].budget_bytes = one * 2;
        let s2 = scan(&c2, "m", None, true);
        let ev = apply(&s2, &plan_evictions(&s2, OPEN, 0).evict, true);
        assert_eq!(names(&ev), vec!["release"]);
        let ev = apply(&s, &plan_evictions(&s, OPEN, 0).evict, true);
        assert_eq!(names(&ev), vec!["release", "prepush", "debug"]);
    }

    #[test]
    fn dry_run_deletes_nothing_and_execute_deletes_planned() {
        let (_d, base) = tmp_base();
        let target = target_with_profiles(&base);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        let s = scan(&c, "m", None, true);
        let plan = plan_evictions(&s, OPEN, 0).evict;
        let ev = apply(&s, &plan, true);
        assert!(ev.iter().all(|e| e.outcome == EvictionOutcome::Planned));
        assert!(target.join("debug").exists());
        assert!(target.join("release").exists());
        let ev = apply(&s, &plan, false);
        assert!(
            ev.iter().all(|e| e.outcome == EvictionOutcome::Deleted),
            "{ev:?}"
        );
        assert!(!target.join("debug").exists());
        assert!(target.join("CACHEDIR.TAG").exists());
    }

    #[test]
    fn floor_evicts_beyond_budget_until_free_space_recovers() {
        let s = synthetic(
            vec![unit("a", 30, 100), unit("b", 10, 100), unit("c", 20, 100)],
            1_000,
            1_350,
        );
        let at = |pct| FloorLimits {
            floor_pct: pct,
            ..OPEN
        };
        assert!(plan_evictions(&s, at(13), 100).evict.is_empty());
        assert_eq!(plan_evictions(&s, at(15), 100).evict, vec![1, 2]);
        assert_eq!(plan_evictions(&s, at(16), 100).evict, vec![1, 2, 0]);
    }

    #[test]
    fn floor_skips_recently_used_items() {
        let s = synthetic(
            vec![unit("a", 30, 100), unit("b", 10, 100), unit("c", 95, 100)],
            1_000,
            1_350,
        );
        let limits = FloorLimits {
            floor_pct: 15,
            min_age_secs: 10,
            ..OPEN
        };
        let plan = plan_evictions(&s, limits, 100);
        assert_eq!(plan.evict, vec![1, 0]);
        assert!(plan.shortfalls.is_empty());
    }

    #[test]
    fn floor_respects_per_run_caps() {
        let s = synthetic(
            vec![unit("a", 30, 100), unit("b", 10, 100), unit("c", 20, 100)],
            1_000,
            1_350,
        );
        let units_capped = FloorLimits {
            floor_pct: 15,
            max_units: 1,
            ..OPEN
        };
        let plan = plan_evictions(&s, units_capped, 100);
        assert!(plan.evict.is_empty());
        assert_eq!(plan.shortfalls.len(), 1);
        let bytes_capped = FloorLimits {
            floor_pct: 15,
            max_bytes: 150,
            ..OPEN
        };
        assert!(plan_evictions(&s, bytes_capped, 100).evict.is_empty());
    }

    #[test]
    fn unreachable_floor_deletes_nothing_and_reports_shortfall() {
        let s = synthetic(vec![unit("a", 30, 100), unit("b", 10, 100)], 1_000, 1_000);
        let plan = plan_evictions(
            &s,
            FloorLimits {
                floor_pct: 50,
                ..OPEN
            },
            100,
        );
        assert!(plan.evict.is_empty());
        assert_eq!(
            plan.shortfalls,
            vec![FloorShortfall {
                volume: "/v".into(),
                need_bytes: 5_000,
                reachable_bytes: 1_200,
            }]
        );
    }

    #[test]
    fn crossing_units_are_neither_counted_nor_evicted() {
        let mut big = unit("mnt", 1, 1_000_000);
        big.crosses = true;
        let s = synthetic(vec![big, unit("a", 5, 100)], 500, 10_000);
        let plan = plan_evictions(&s, OPEN, 100);
        assert!(plan.evict.is_empty());
        let c = QuotaConfig::default();
        let r = report(&c, &s);
        assert_eq!(r.classes[0].used_bytes, 100);
        assert_eq!(r.classes[0].crossing, vec!["/b/r/mnt".to_string()]);
    }

    #[test]
    fn non_rebuildable_class_is_reported_never_evicted() {
        let (_d, base) = tmp_base();
        write(&base.join("vm/disk.img"), 4096);
        let c = cfg(
            &base,
            PathClass::VmDisk,
            0,
            vec![base.join("vm/disk.img").display().to_string()],
        );
        let s = scan(&c, "m", None, true);
        assert!(report(&c, &s).classes[0].over_budget);
        assert!(
            plan_evictions(
                &s,
                FloorLimits {
                    floor_pct: 90,
                    ..OPEN
                },
                0
            )
            .evict
            .is_empty()
        );
    }

    #[test]
    fn unsafe_roots_are_refused() {
        let (_d, base) = tmp_base();
        let home = base.join("home");
        let (_o, outside) = tmp_base();
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
        let (roots, refused) = resolve_roots(PathClass::VmDisk, &patterns, &bases, Some(&home));
        assert_eq!(roots, vec![home.join("code/x")]);
        #[cfg(unix)]
        assert_eq!(refused.len(), 5);
    }

    #[test]
    fn unsafe_bases_are_refused_and_their_roots_with_them() {
        let (_d, base) = tmp_base();
        let home = base.join("home");
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        opted_in(&home.join("Documents"));
        let c = QuotaConfig {
            bases: vec![
                home.display().to_string(),
                base.display().to_string(),
                "/".to_string(),
                "/etc".to_string(),
                "/usr".to_string(),
            ],
            floor_pct: Some(0),
            budgets: vec![ClassBudget {
                class: PathClass::Tmp,
                budget_bytes: 0,
                roots: vec![home.join("Documents").display().to_string()],
                member: None,
            }],
            ..QuotaConfig::default()
        };
        let s = scan(&c, "m", Some(&home), true);
        assert_eq!(s.refused_bases.len(), 5);
        assert!(s.classes[0].roots.is_empty());
        assert_eq!(
            s.classes[0].refused[0].reason,
            "outside every configured base"
        );
        assert!(s.units.is_empty());
        assert!(validate(&c, Some(&home)).is_err());
        let mut ok = c.clone();
        ok.bases = vec![
            home.join("Documents")
                .parent()
                .unwrap()
                .join("code")
                .display()
                .to_string(),
        ];
        std::fs::create_dir_all(home.join("code")).unwrap();
        assert!(validate(&ok, Some(&home)).is_ok());
    }

    #[test]
    fn roots_without_class_marker_are_refused() {
        let (_d, base) = tmp_base();
        let target = base.join("repo/target");
        write(&target.join("debug/x"), 10);
        let tmp = base.join("scratch");
        write(&tmp.join("old"), 10);
        let bases = vec![base.clone()];
        let (roots, refused) = resolve_roots(
            PathClass::CargoTarget,
            &[target.display().to_string()],
            &bases,
            None,
        );
        assert!(roots.is_empty());
        assert_eq!(refused[0].reason, "missing the class marker file");
        let (roots, _) = resolve_roots(PathClass::Tmp, &[tmp.display().to_string()], &bases, None);
        assert!(roots.is_empty());
        opted_in(&tmp);
        let (roots, _) = resolve_roots(PathClass::Tmp, &[tmp.display().to_string()], &bases, None);
        assert_eq!(roots, vec![tmp]);
    }

    #[test]
    fn member_scoped_budget_applies_only_to_its_member() {
        let (_d, base) = tmp_base();
        let target = target_with_profiles(&base);
        let mut c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        c.budgets[0].member = Some("other".into());
        assert!(scan(&c, "m", None, true).classes.is_empty());
        assert_eq!(scan(&c, "other", None, true).classes.len(), 1);
    }

    #[test]
    fn held_cargo_lock_skips_unit() {
        let (_d, base) = tmp_base();
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
        let s = scan(&c, "m", None, true);
        let ev = apply(&s, &plan_evictions(&s, OPEN, 0).evict, false);
        assert_eq!(names(&ev), vec!["prepush", "debug"]);
        assert!(target.join("release").exists());
        assert!(!scan(&c, "m", None, false).units.iter().any(|u| u.locked));
        drop(holder);
    }

    #[test]
    fn lock_taken_after_scan_skips_at_eviction() {
        let (_d, base) = tmp_base();
        let root = base.join("ci");
        opted_in(&root);
        write(&root.join("slot1/data"), 1024);
        write(&root.join("slot1/.lock"), 0);
        let c = cfg(
            &base,
            PathClass::CiCache,
            0,
            vec![root.display().to_string()],
        );
        let s = scan(&c, "m", None, true);
        let plan = plan_evictions(&s, OPEN, 0).evict;
        let holder = File::open(root.join("slot1/.lock")).unwrap();
        holder.lock().unwrap();
        let ev = apply(&s, &plan, false);
        assert_eq!(ev[0].outcome, EvictionOutcome::SkippedLocked);
        assert!(root.join("slot1/data").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_units_are_never_followed_or_deleted() {
        let (_d, base) = tmp_base();
        let keep = base.join("keep/precious");
        write(&keep, 1024);
        let root = base.join("tmpdir");
        opted_in(&root);
        write(&root.join("old/file"), 1024);
        std::os::unix::fs::symlink(base.join("keep"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(base.join("keep"), root.join("old/inner")).unwrap();
        let c = cfg(&base, PathClass::Tmp, 0, vec![root.display().to_string()]);
        let s = scan(&c, "m", None, true);
        let ev = apply(&s, &plan_evictions(&s, OPEN, 0).evict, false);
        assert_eq!(names(&ev), vec!["old"]);
        assert_eq!(ev[0].outcome, EvictionOutcome::Deleted);
        assert!(keep.exists());
        assert!(root.join("link").exists());
    }

    #[cfg(unix)]
    #[test]
    fn swapped_root_or_unit_is_refused_at_eviction() {
        let (_d, base) = tmp_base();
        let root = base.join("tmpdir");
        opted_in(&root);
        write(&root.join("old/file"), 1024);
        let c = cfg(&base, PathClass::Tmp, 0, vec![root.display().to_string()]);
        let s = scan(&c, "m", None, true);
        let plan = plan_evictions(&s, OPEN, 0).evict;

        std::fs::rename(&root, base.join("moved")).unwrap();
        write(&root.join("old/file"), 1024);
        let ev = apply(&s, &plan, false);
        assert_eq!(ev[0].outcome, EvictionOutcome::SkippedUnsafe);
        assert!(root.join("old/file").exists());

        std::fs::remove_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(base.join("moved"), &root).unwrap();
        let ev = apply(&s, &plan, false);
        assert_eq!(ev[0].outcome, EvictionOutcome::SkippedUnsafe);
        assert!(base.join("moved/old/file").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_unit_is_refused_at_eviction() {
        let (_d, base) = tmp_base();
        let root = base.join("tmpdir");
        opted_in(&root);
        write(&root.join("old/file"), 1024);
        let c = cfg(&base, PathClass::Tmp, 0, vec![root.display().to_string()]);
        let s = scan(&c, "m", None, true);
        let plan = plan_evictions(&s, OPEN, 0).evict;
        std::fs::remove_dir_all(root.join("old")).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), root.join("old")).unwrap();
        let ev = apply(&s, &plan, false);
        assert_eq!(ev[0].outcome, EvictionOutcome::SkippedUnsafe);
        assert!(root.join("old").symlink_metadata().is_ok());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_enforce_runs_serially() {
        let (_d, base) = tmp_base();
        let target = target_with_profiles(&base);
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        let (a, b) = tokio::join!(
            enforce_with(c.clone(), "m".into(), None, false),
            enforce_with(c.clone(), "m".into(), None, false),
        );
        let all: Vec<Eviction> = a
            .unwrap()
            .evictions
            .into_iter()
            .chain(b.unwrap().evictions)
            .collect();
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(all.iter().all(|e| e.outcome == EvictionOutcome::Deleted));
    }

    #[cfg(unix)]
    fn deep_tree(parent: &Path, depth: usize) {
        use rustix::fs::{Mode, OFlags, mkdirat, openat};
        std::fs::create_dir_all(parent).unwrap();
        let mut fd = openat(
            rustix::fs::CWD,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
        for _ in 0..depth {
            mkdirat(&fd, "d", Mode::from_raw_mode(0o755)).unwrap();
            fd = openat(&fd, "d", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
        }
        std::fs::write(parent.join("top"), b"x").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn overly_deep_tree_is_refused_and_bounded_tree_is_removed() {
        let (_d, base) = tmp_base();
        let root = base.join("tmpdir");
        opted_in(&root);
        deep_tree(&root.join("deep"), 520);
        deep_tree(&root.join("ok"), 60);
        let c = cfg(&base, PathClass::Tmp, 0, vec![root.display().to_string()]);
        let s = scan(&c, "m", None, true);
        let ev = apply(&s, &plan_evictions(&s, OPEN, 0).evict, false);
        let by_name: HashMap<String, &Eviction> = names(&ev).into_iter().zip(ev.iter()).collect();
        assert_eq!(by_name["deep"].outcome, EvictionOutcome::SkippedUnsafe);
        assert_eq!(
            by_name["deep"].error.as_deref(),
            Some("directory tree is too deep")
        );
        assert_eq!(by_name["ok"].outcome, EvictionOutcome::Deleted);
        assert!(!root.join("ok").exists());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_and_symlinked_lock_files_are_not_locks() {
        let (_d, base) = tmp_base();
        let target = target_with_profiles(&base);
        let fifo = std::ffi::CString::new(
            target
                .join("debug/.cargo-lock")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let held = base.join("held.lock");
        write(&held, 0);
        let holder = File::open(&held).unwrap();
        holder.lock().unwrap();
        std::os::unix::fs::symlink(&held, target.join("release/.cargo-lock")).unwrap();
        let c = cfg(
            &base,
            PathClass::CargoTarget,
            0,
            vec![target.display().to_string()],
        );
        let s = scan(&c, "m", None, true);
        assert!(s.units.iter().all(|u| !u.locked));
        let ev = apply(&s, &plan_evictions(&s, OPEN, 0).evict, false);
        assert!(
            ev.iter().all(|e| e.outcome == EvictionOutcome::Deleted),
            "{ev:?}"
        );
        assert!(held.exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_removal_reports_partial_freed_bytes() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let (_d, base) = tmp_base();
        if std::fs::metadata(&base).unwrap().uid() == 0 {
            return;
        }
        let root = base.join("tmpdir");
        opted_in(&root);
        write(&root.join("item/a"), 64 * 1024);
        write(&root.join("item/z/f"), 10);
        std::fs::set_permissions(root.join("item/z"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        let c = cfg(&base, PathClass::Tmp, 0, vec![root.display().to_string()]);
        let s = scan(&c, "m", None, true);
        let ev = apply(&s, &plan_evictions(&s, OPEN, 0).evict, false);
        std::fs::set_permissions(root.join("item/z"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(ev[0].outcome, EvictionOutcome::Failed, "{ev:?}");
        assert!(ev[0].partial);
        assert!(ev[0].freed_bytes >= 64 * 1024);
        assert!(!root.join("item/a").exists());
    }

    #[test]
    fn list_store_is_dropped_after_invalidation() {
        let out = StorageQuotaListOutput {
            configured: true,
            enabled: true,
            floor_pct: DEFAULT_FLOOR_PCT,
            refused_bases: Vec::new(),
            classes: Vec::new(),
            volumes: Vec::new(),
        };
        let key = "list-generation-test".to_string();
        let stale = LIST_GENERATION.load(Ordering::SeqCst);
        invalidate_list_cache();
        store_list(stale, key.clone(), &out);
        assert!(cached_list(&key).is_none());
        let fresh = LIST_GENERATION.load(Ordering::SeqCst);
        store_list(fresh, key.clone(), &out);
        assert!(cached_list(&key).is_some());
        invalidate_list_cache();
    }
}
