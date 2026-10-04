//! Generic, location-agnostic backup store.
//!
//! One filesystem tree beneath a target-supplied `root`, laid out as:
//!
//! ```text
//! <root>/<collection…>/<id>/
//!     manifest.json   — the typed `BackupRecord` (written last, on commit)
//!     payload/…       — provider-written backup files
//! ```
//!
//! `<collection…>` is the provider-declared labeled layout (e.g.
//! `hosts/thor`). The store treats it as an opaque relative path, so it
//! organizes backups identically beneath any target root. `id` is a sortable
//! compact UTC stamp (`YYYYMMDD-HHMMSS`), suffixed with the writer's hostname on
//! a shared store (`YYYYMMDD-HHMMSS-<host>`) so hosts sharing one pool never
//! mint the same id; either form sorts chronologically. A backup's identity
//! (`kind`+`instance`) lives in its manifest; listing and selection filter on
//! that manifest identity, so a provider may file backups under any layout. The
//! store owns dating, listing, selection, and retention pruning
//! ([[service-backup-restore-location-agnostic]]).
//!
//! A slot dir with a `manifest.json` is a complete backup; one without is
//! in-progress and is skipped by `list`/`resolve`. A manifest's slot and payload
//! dirs are always derived from where the manifest was found, never from its
//! contents, and its `id` must equal its slot dir's name: on a shared pool a
//! manifest may come from another host or be crafted, and prune deletes the
//! slot. Entries named `.orca-*` belong to the store or the target plugin (lock,
//! staging, trash) and are never walked.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use contract::backup::{BackupRecord, BackupSelector, BackupWriter, Retention, STAGE_LOCK_FILE};
use utils::time::Timestamp;

/// One backup a prune selected for removal but could not remove.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PruneFailure {
    pub id: String,
    pub path: String,
    pub error: String,
}
/// What a prune INTENDED versus what it achieved.
///
/// A prune that selects 107 snapshots and removes none is a total failure, but
/// reported as a count of warnings it is indistinguishable from partial success
/// — that is exactly how a fleet-wide retention policy looked applied for weeks
/// while every snapshot stayed on disk (#610). Carrying both numbers makes the
/// difference impossible to lose: `selected` is the intent, `removed` is the
/// outcome, and anything short of equality is a failure, never a warning.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    /// How many backups the retention policy chose to remove.
    pub selected: usize,
    /// The backups actually gone from the store.
    pub removed: Vec<BackupRecord>,
    /// Per-backup reasons for every removal that did not happen.
    pub failures: Vec<PruneFailure>,
}

impl PruneReport {
    /// True only when every selected backup was actually removed.
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty() && self.removed.len() == self.selected
    }

    /// `selected N, removed M` — the reconciliation the issue asks for, in the
    /// one line an operator reads.
    pub fn summary(&self) -> String {
        format!("selected {}, removed {}", self.selected, self.removed.len())
    }
}

const MANIFEST: &str = "manifest.json";
const PAYLOAD: &str = "payload";
/// Entries with this prefix are store/plugin bookkeeping, never backups.
const RESERVED_PREFIX: &str = ".orca-";
const MANIFEST_TMP: &str = ".orca-manifest.json.tmp";
/// Matches the target plugins' acquisition limit on the same lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const LOCK_POLL: Duration = Duration::from_millis(250);

/// A filesystem-backed store of dated backups.
#[derive(Debug, Clone)]
pub struct BackupStore {
    root: PathBuf,
    shared: bool,
}

/// A complete backup and the slot dir its manifest was found in.
struct Located {
    rec: BackupRecord,
    slot: PathBuf,
}

/// An exclusive hold on a store's [`STAGE_LOCK_FILE`]; released on drop.
#[derive(Debug)]
pub struct StageLock {
    _file: fs::File,
}

impl BackupStore {
    /// A store rooted at `root`. The directory is created lazily on first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            shared: false,
        }
    }

    /// Mark the store as a pool several hosts write: new slot ids carry this
    /// host's name so concurrent writers never collide on an id.
    pub fn shared(mut self, shared: bool) -> Self {
        self.shared = shared;
        self
    }

    /// The default store: `<orca state dir>/backups` (`~/.orca/backups`).
    pub fn default_store() -> Result<Self> {
        let root = contract::config::state_dir()
            .context("resolve orca state dir for default backup store")?
            .join("backups");
        Ok(Self::new(root))
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Take the exclusive stage lock, waiting up to ten minutes. Hold it for the
    /// whole of any mutation of the tree: a target plugin reconciling the same
    /// root with its remote takes it too.
    pub fn lock(&self) -> Result<StageLock> {
        self.lock_within(LOCK_TIMEOUT)
    }

    /// [`lock`](Self::lock) off the async runtime's worker threads.
    pub async fn lock_async(&self) -> Result<StageLock> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.lock())
            .await
            .map_err(|e| anyhow!("stage lock task panicked: {e}"))?
    }

    fn lock_within(&self, timeout: Duration) -> Result<StageLock> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create backup store root {}", self.root.display()))?;
        let path = self.root.join(STAGE_LOCK_FILE);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open stage lock {}", path.display()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(StageLock { _file: file }),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(LOCK_POLL);
                }
                Err(fs::TryLockError::WouldBlock) => bail!(
                    "timed out after {}s waiting for stage lock {}",
                    timeout.as_secs(),
                    path.display()
                ),
                Err(fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("lock {}", path.display()));
                }
            }
        }
    }

    /// The directory a backup's slots live under: the target root joined with the
    /// provider-declared `collection` layout segments (e.g. `hosts/thor`).
    /// Each segment is sanitized so a provider can't escape the root.
    fn collection_dir(&self, collection: &[String]) -> PathBuf {
        let mut dir = self.root.clone();
        for seg in collection {
            dir.push(sanitize_segment(seg));
        }
        dir
    }

    /// Allocate a fresh, empty slot for a new backup written under `collection`
    /// (the provider's labeled layout, e.g. `["hosts","thor"]`). `kind`
    /// and `instance` are recorded in the manifest as the backup's IDENTITY —
    /// independent of where it physically lands, so listing/selection filter by
    /// identity, not directory names.
    ///
    /// Creates `<root>/<collection…>/<id>/payload/`; the provider writes its files
    /// under [`BackupSlot::payload_dir`], then calls [`BackupSlot::commit`] (or
    /// [`BackupSlot::abort`] on failure). The `id` is the current UTC compact
    /// stamp (plus this host's name on a shared store), disambiguated with a
    /// `-N` suffix if that slot already exists so ids stay unique and sortable.
    pub fn new_slot(
        &self,
        collection: &[String],
        kind: &str,
        instance: &str,
    ) -> Result<BackupSlot> {
        let now = utils::time::now();
        let created_ms = now.unix_millis();
        let writer = local_writer();
        let base = if self.shared {
            format!("{}-{}", now.compact(), id_host_segment(&writer.host))
        } else {
            now.compact()
        };
        let coll_dir = self.collection_dir(collection);

        // Disambiguate collisions within the same second.
        let mut id = base.clone();
        let mut n = 1u32;
        while coll_dir.join(&id).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }

        let dir = coll_dir.join(&id);
        let payload = dir.join(PAYLOAD);
        fs::create_dir_all(&payload)
            .with_context(|| format!("create backup slot {}", payload.display()))?;

        Ok(BackupSlot {
            id,
            domain: kind.to_string(),
            instance: instance.to_string(),
            dir,
            payload,
            created_ms,
            writer,
        })
    }

    /// List backups, newest first. `domain` (kind) / `instance` match against the
    /// manifest identity, so a backup filed under any layout
    /// (`hosts/thor`) is found by `list(Some("host"), Some("thor"))`.
    /// `None` matches any value on that axis. In-progress slots (no manifest) and
    /// invalid manifests are skipped; a missing tree lists as empty.
    pub fn list(&self, domain: Option<&str>, instance: Option<&str>) -> Result<Vec<BackupRecord>> {
        Ok(self
            .located(domain, instance)?
            .into_iter()
            .map(|l| l.rec)
            .collect())
    }

    /// Matching complete backups with their slot dirs, newest first.
    fn located(&self, domain: Option<&str>, instance: Option<&str>) -> Result<Vec<Located>> {
        let mut out = self.all_located()?;
        out.retain(|l| {
            domain.is_none_or(|k| l.rec.kind == k) && instance.is_none_or(|i| l.rec.instance == i)
        });
        // The id stamp sorts chronologically, across writers too.
        out.sort_by(|a, b| b.rec.id.cmp(&a.rec.id));
        Ok(out)
    }

    /// Every complete backup in the store, in arbitrary order, walking the whole
    /// tree for `manifest.json` files so any provider layout is matched. A dir
    /// holding a manifest is a slot and is not descended further. An unreadable
    /// root is an error; any other unreadable entry or invalid manifest is
    /// skipped with a warning so one bad slot cannot hide the rest.
    fn all_located(&self) -> Result<Vec<Located>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) if dir == self.root => {
                    return Err(anyhow!("read dir {}: {e}", dir.display()));
                }
                Err(e) => {
                    tracing::warn!("[backup] skipping unreadable {}: {e}", dir.display());
                    continue;
                }
            };
            let mut subdirs = Vec::new();
            let mut manifest = None;
            for entry in rd {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("[backup] skipping entry in {}: {e}", dir.display());
                        continue;
                    }
                };
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(RESERVED_PREFIX) {
                    continue;
                }
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                // DirEntry::file_type does not follow symlinks, so a link can
                // never lead the walk (or a prune) outside the root.
                if ft.is_dir() {
                    if name != PAYLOAD {
                        subdirs.push(entry.path());
                    }
                } else if ft.is_file() && name == MANIFEST {
                    manifest = Some(entry.path());
                }
            }
            match manifest {
                Some(path) if dir != self.root => match load_manifest(&path) {
                    Ok(l) => out.push(l),
                    Err(e) => tracing::warn!("[backup] skipping {}: {e:#}", path.display()),
                },
                // A manifest at the root would make the root itself a slot.
                Some(path) => {
                    tracing::warn!(
                        "[backup] ignoring manifest at store root {}",
                        path.display()
                    );
                    stack.extend(subdirs);
                }
                None => stack.extend(subdirs),
            }
        }
        Ok(out)
    }

    /// Resolve a selector to a concrete record for `(domain, instance)`.
    pub fn resolve(
        &self,
        domain: &str,
        instance: &str,
        sel: &BackupSelector,
    ) -> Result<BackupRecord> {
        let records = self.list(Some(domain), Some(instance))?;
        match sel {
            BackupSelector::Latest => records
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no backups exist for {domain}/{instance}")),
            BackupSelector::Id(id) => records
                .into_iter()
                .find(|r| &r.id == id)
                .ok_or_else(|| anyhow!("no backup `{id}` for {domain}/{instance}")),
        }
    }

    /// Delete the backup identified by `rec` (its whole slot dir). The slot is
    /// re-located in this store by identity; `rec.path` is never trusted.
    pub fn remove(&self, rec: &BackupRecord) -> Result<()> {
        let found = self
            .located(Some(&rec.kind), Some(&rec.instance))?
            .into_iter()
            .find(|l| l.rec.id == rec.id && l.rec.writer == rec.writer);
        match found {
            Some(l) => remove_slot(&l.slot, &l.rec.id),
            None => Ok(()),
        }
    }

    /// Can this store actually delete from `dir`?
    ///
    /// The real fault was an identity mismatch — PBS running as `uid=34(backup)`
    /// against files owned `99:100` on the NAS — which surfaced only as a
    /// per-snapshot failure at prune time, long after the datastore was
    /// configured and trusted. Probing is a create-then-rename-then-delete in
    /// the directory itself, because that is exactly the permission a prune
    /// needs and the only way to know is to try it.
    ///
    /// `Ok(())` means a prune here can succeed; the error names the directory
    /// and the reason, suitable to fail a configure with.
    pub fn check_prunable(dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        let probe = dir.join(".orca-prune-probe");
        let staged = dir.join(".orca-prune-probe-staged");
        drop(fs::remove_dir_all(&probe));
        drop(fs::remove_dir_all(&staged));
        fs::create_dir(&probe).with_context(|| {
            format!(
                "cannot create in {} — a prune here will fail",
                dir.display()
            )
        })?;
        let renamed = fs::rename(&probe, &staged);
        let target = if renamed.is_ok() { &staged } else { &probe };
        let removed = fs::remove_dir_all(target);
        renamed.with_context(|| {
            format!(
                "cannot rename within {} — a prune here will fail",
                dir.display()
            )
        })?;
        removed.with_context(|| {
            format!(
                "cannot delete in {} — a prune here will fail",
                dir.display()
            )
        })
    }

    /// Apply `retention` to `(domain, instance)`, deleting the backups that fall
    /// outside the policy. Returns intent AND outcome — see [`PruneReport`].
    ///
    /// The full PBS/vzdump `prune-backups` model: every set axis independently
    /// selects survivors, and a backup kept by ANY axis survives (union) —
    /// `keep_last` keeps the N newest overall; each calendar axis
    /// (`keep_hourly`/`daily`/`weekly`/`monthly`/`yearly`) keeps the newest one
    /// backup in each of its most-recent N periods. An unbounded policy (no axis
    /// set) prunes nothing.
    pub fn prune(
        &self,
        domain: &str,
        instance: &str,
        retention: &Retention,
    ) -> Result<PruneReport> {
        if retention.is_unbounded() {
            return Ok(PruneReport::default());
        }
        let located = self.located(Some(domain), Some(instance))?; // newest first
        let records: Vec<&BackupRecord> = located.iter().map(|l| &l.rec).collect();
        let keep = retained(&records, retention);

        let mut report = PruneReport::default();
        for (i, l) in located.iter().enumerate() {
            if keep.contains(&i) {
                continue;
            }
            report.selected += 1;
            // Preflight once, on the first selection: an identity that cannot
            // unlink here fails every backup for one reason, and saying it once
            // in the store's own terms beats N identical per-snapshot errors.
            if let Some(parent) = l.slot.parent()
                && report.removed.is_empty()
                && report.failures.is_empty()
                && let Err(e) = Self::check_prunable(parent)
            {
                report.failures.push(PruneFailure {
                    id: l.rec.id.clone(),
                    path: l.rec.path.clone(),
                    error: format!("{e:#}"),
                });
                continue;
            }
            // Attempt EVERY selected record. Bailing on the first failure used
            // to leave the rest untried, so one unwritable snapshot hid however
            // many would have succeeded (#610).
            match remove_slot(&l.slot, &l.rec.id) {
                Ok(()) => report.removed.push(l.rec.clone()),
                Err(e) => report.failures.push(PruneFailure {
                    id: l.rec.id.clone(),
                    path: l.rec.path.clone(),
                    error: format!("{e:#}"),
                }),
            }
        }
        Ok(report)
    }
}

/// Remove one slot dir.
///
/// Takes the slot out of the store with a RENAME first, then deletes it.
/// `remove_dir_all` walks top-down: on a store whose parent directory refuses
/// the unlink it happily deletes the manifest and payload and only then fails to
/// remove the slot itself. The backup is destroyed, yet the prune reports it as
/// not removed — the inverse of #610's lie, and the one that loses data. A
/// rename needs exactly the same parent-directory write permission as the final
/// unlink, so a store we cannot prune fails here having changed nothing at all.
fn remove_slot(dir: &Path, id: &str) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let staged = dir.with_file_name(format!("{RESERVED_PREFIX}removing-{id}"));
    fs::rename(dir, &staged)
        .with_context(|| format!("stage backup {} for removal", dir.display()))?;
    fs::remove_dir_all(&staged)
        .with_context(|| format!("remove staged backup {}", staged.display()))?;
    Ok(())
}

/// The writer identity stamped on every backup this host commits.
pub(crate) fn local_writer() -> BackupWriter {
    BackupWriter {
        host: crate::host_identity::hostname().to_string(),
        machine_id: crate::host_identity::try_machine_id()
            .unwrap_or_default()
            .to_string(),
    }
}

/// A hostname reduced to an id-safe segment: lowercase ASCII alphanumerics,
/// anything else collapsed to `-`.
fn id_host_segment(host: &str) -> String {
    let mut out = String::new();
    for c in host.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "host".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A slot id must be one plain path component that cannot name a reserved or
/// traversal entry.
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 255 {
        bail!("invalid backup id length");
    }
    if id.starts_with('.') || id.chars().any(|c| std::path::is_separator(c) || c == '\0') {
        bail!("invalid backup id `{id}`");
    }
    Ok(())
}

/// Make one layout segment safe as a single path component: strip path
/// separators and `.`/`..` traversal so a provider-declared layout can never
/// escape the store root. Empty/degenerate segments collapse to `_`.
fn sanitize_segment(seg: &str) -> String {
    let cleaned: String = seg
        .trim()
        .chars()
        .map(|c| if std::path::is_separator(c) { '_' } else { c })
        .collect();
    match cleaned.as_str() {
        "" | "." | ".." => "_".to_string(),
        _ => cleaned,
    }
}

/// A reserved, in-progress backup slot. The provider fills [`payload_dir`] then
/// [`commit`]s (or [`abort`]s). Dropping without either leaves an incomplete
/// (manifest-less) slot that `list`/`resolve` ignore.
///
/// [`payload_dir`]: BackupSlot::payload_dir
/// [`commit`]: BackupSlot::commit
/// [`abort`]: BackupSlot::abort
#[derive(Debug)]
pub struct BackupSlot {
    pub id: String,
    pub domain: String,
    pub instance: String,
    dir: PathBuf,
    payload: PathBuf,
    created_ms: i64,
    writer: BackupWriter,
}

impl BackupSlot {
    /// The directory the provider writes payload files into.
    pub fn payload_dir(&self) -> &Path {
        &self.payload
    }

    /// Finalize: measure the payload, atomically write `manifest.json`, and
    /// return the record. `checksum`/`note` are provider-supplied metadata.
    pub fn commit(self, checksum: Option<String>, note: Option<String>) -> Result<BackupRecord> {
        let (size_bytes, file_count) = dir_size(&self.payload)?;
        let mut rec = BackupRecord {
            system: String::new(),
            id: self.id,
            kind: self.domain,
            instance: self.instance,
            created_ms: self.created_ms,
            path: PAYLOAD.to_string(),
            size_bytes,
            file_count,
            checksum,
            note,
            writer: Some(self.writer),
        };
        let json = serde_json::to_string_pretty(&rec).context("serialize backup manifest")?;
        write_atomic(&self.dir, &json)?;
        rec.path = self.payload.to_string_lossy().into_owned();
        Ok(rec)
    }

    /// Discard the slot (e.g. the provider's snapshot failed): remove its dir so
    /// no half-written backup lingers.
    pub fn abort(self) -> Result<()> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir)
                .with_context(|| format!("abort backup slot {}", self.dir.display()))?;
        }
        Ok(())
    }
}

/// Write `<dir>/manifest.json` so a reader (or a concurrent reconcile) sees
/// either no manifest or the whole one: temp file, fsync, rename, fsync dir.
fn write_atomic(dir: &Path, json: &str) -> Result<()> {
    let tmp = dir.join(MANIFEST_TMP);
    let manifest = dir.join(MANIFEST);
    let mut f =
        fs::File::create(&tmp).with_context(|| format!("create manifest {}", tmp.display()))?;
    f.write_all(json.as_bytes())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("write manifest {}", tmp.display()))?;
    drop(f);
    fs::rename(&tmp, &manifest)
        .with_context(|| format!("commit manifest {}", manifest.display()))?;
    // Network filesystems may refuse a directory fsync; the rename is already
    // visible, this only hardens it against a local crash.
    if let Ok(d) = fs::File::open(dir) {
        drop(d.sync_all());
    }
    Ok(())
}

/// The record indices `retention` keeps, given `records` newest-first. The
/// count and calendar axes union — a record kept by ANY of them survives — and
/// `max_total_bytes` then caps the result, trimming oldest until it fits. When
/// the size cap is the only bound, it keeps the newest backups that fit.
fn retained(records: &[&BackupRecord], retention: &Retention) -> HashSet<usize> {
    let has_count_axis = retention.keep_last.is_some()
        || retention.keep_hourly.is_some()
        || retention.keep_daily.is_some()
        || retention.keep_weekly.is_some()
        || retention.keep_monthly.is_some()
        || retention.keep_yearly.is_some();

    let mut keep = HashSet::new();
    if has_count_axis {
        // keep_last: the N newest overall, regardless of period.
        if let Some(n) = retention.keep_last {
            keep.extend(0..records.len().min(n as usize));
        }
        // Calendar axes: the newest backup in each of the N most-recent periods.
        keep_per_bucket(
            records,
            retention.keep_hourly,
            Timestamp::hour_bucket,
            &mut keep,
        );
        keep_per_bucket(records, retention.keep_daily, Timestamp::date, &mut keep);
        keep_per_bucket(
            records,
            retention.keep_weekly,
            Timestamp::iso_week_bucket,
            &mut keep,
        );
        keep_per_bucket(
            records,
            retention.keep_monthly,
            Timestamp::month_bucket,
            &mut keep,
        );
        keep_per_bucket(
            records,
            retention.keep_yearly,
            Timestamp::year_bucket,
            &mut keep,
        );
    } else {
        // Size cap alone: every backup is a candidate; the cap below trims it.
        keep.extend(0..records.len());
    }

    // Size cap: walk the kept records newest-first and drop the oldest that push
    // the total past the budget. The newest kept backup always survives.
    if let Some(cap) = retention.max_total_bytes {
        let mut total: u64 = 0;
        let mut first = true;
        for (i, rec) in records.iter().enumerate() {
            if !keep.contains(&i) {
                continue;
            }
            let next = total.saturating_add(rec.size_bytes);
            if first || next <= cap {
                total = next;
                first = false;
            } else {
                keep.remove(&i);
            }
        }
    }
    keep
}

/// For one calendar axis: keep the newest record in each of the `n` most-recent
/// distinct periods (period key from `bucket`). `records` MUST be newest-first,
/// so the first record seen for a period is that period's newest.
fn keep_per_bucket(
    records: &[&BackupRecord],
    n: Option<u32>,
    bucket: impl Fn(&Timestamp) -> String,
    keep: &mut HashSet<usize>,
) {
    let Some(n) = n else { return };
    if n == 0 {
        return;
    }
    let mut periods: Vec<String> = Vec::new(); // distinct periods, newest-first
    for (i, rec) in records.iter().enumerate() {
        let Some(ts) = Timestamp::from_unix_millis(rec.created_ms) else {
            continue;
        };
        let key = bucket(&ts);
        if periods.contains(&key) {
            continue; // an older backup in a period we already kept the newest of
        }
        if periods.len() >= n as usize {
            break; // the N most-recent periods are full; everything else is older
        }
        periods.push(key);
        keep.insert(i);
    }
}

/// Load a manifest found at `path`. The slot is the manifest's dir and the
/// payload dir is derived from it; the record's own `path` is discarded.
fn load_manifest(path: &Path) -> Result<Located> {
    let slot = path
        .parent()
        .ok_or_else(|| anyhow!("manifest has no parent dir"))?
        .to_path_buf();
    let slot_name = slot
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("slot dir name is not UTF-8"))?;
    let raw = fs::read_to_string(path).context("read manifest")?;
    let mut rec: BackupRecord = serde_json::from_str(&raw).context("parse manifest")?;
    validate_id(&rec.id)?;
    if rec.id != slot_name {
        bail!(
            "manifest id `{}` does not match its slot `{slot_name}`",
            rec.id
        );
    }
    rec.path = slot.join(PAYLOAD).to_string_lossy().into_owned();
    Ok(Located { rec, slot })
}

/// Total byte size and file count under `dir`, walked recursively. Symlinks are
/// counted by their own metadata (not followed) so a link can't inflate or loop.
fn dir_size(dir: &Path) -> Result<(u64, u64)> {
    let mut bytes = 0u64;
    let mut count = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).with_context(|| format!("walk {}", d.display()))? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else {
                bytes += entry.metadata()?.len();
                count += 1;
            }
        }
    }
    Ok((bytes, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store() -> (tempfile::TempDir, BackupStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("backups"));
        (tmp, store)
    }

    /// The default `[kind, instance]` collection layout used by most tests.
    fn coll(domain: &str, instance: &str) -> Vec<String> {
        vec![domain.to_string(), instance.to_string()]
    }

    /// Write one backup with `body` as the payload's `data.txt`, return its record.
    fn write_backup(store: &BackupStore, domain: &str, instance: &str, body: &str) -> BackupRecord {
        let slot = store
            .new_slot(&coll(domain, instance), domain, instance)
            .unwrap();
        fs::write(slot.payload_dir().join("data.txt"), body).unwrap();
        slot.commit(None, Some("test".into())).unwrap()
    }

    #[test]
    fn list_is_layout_agnostic_filtering_by_manifest_identity() {
        // File a backup under a rich taxonomy layout, but with identity
        // (kind=host, instance=thor) that differs from the directory names.
        let (_tmp, store) = store();
        let layout = vec![
            "hosts".to_string(),
            "labeled".to_string(),
            "thor".to_string(),
        ];
        let slot = store.new_slot(&layout, "host", "thor").unwrap();
        fs::write(slot.payload_dir().join("d.txt"), "x").unwrap();
        let rec = slot.commit(None, None).unwrap();

        // Physically filed under the layout…
        assert!(
            store
                .root()
                .join("hosts/labeled/thor")
                .join(&rec.id)
                .join(MANIFEST)
                .exists()
        );
        // …yet found by IDENTITY, not directory names.
        let by_identity = store.list(Some("host"), Some("thor")).unwrap();
        assert_eq!(by_identity.len(), 1);
        assert_eq!(by_identity[0].id, rec.id);
        // A directory-name segment is not an identity: listing by it finds nothing.
        assert!(
            store
                .list(Some("host"), Some("labeled"))
                .unwrap()
                .is_empty()
        );

        // resolve + prune also work off identity/record path.
        assert_eq!(
            store
                .resolve("host", "thor", &BackupSelector::Latest)
                .unwrap()
                .id,
            rec.id
        );
        let removed = store
            .prune("host", "thor", &Retention::keep_last(0))
            .unwrap();
        assert_eq!(removed.removed.len(), 1);
        assert!(removed.is_complete(), "every selected backup was removed");
        assert!(store.list(Some("host"), Some("thor")).unwrap().is_empty());
    }

    /// Write a raw manifest file into `<root>/<rel>/manifest.json`.
    fn plant(store: &BackupStore, rel: &str, json: &str) -> PathBuf {
        let dir = store.root().join(rel);
        fs::create_dir_all(dir.join(PAYLOAD)).unwrap();
        fs::write(dir.join(MANIFEST), json).unwrap();
        dir
    }

    fn manifest_json(id: &str, path: &str) -> String {
        format!(
            r#"{{"id":"{id}","kind":"host","instance":"default","createdMs":1,"path":"{path}"}}"#
        )
    }

    #[test]
    fn a_manifest_path_is_never_trusted_for_prune() {
        let (tmp, store) = store();
        let victim = tmp.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("precious"), "x").unwrap();
        let slot = plant(
            &store,
            "host/default/20260101-000000",
            &manifest_json("20260101-000000", &victim.join("payload").to_string_lossy()),
        );

        let listed = store.list(Some("host"), Some("default")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            Path::new(&listed[0].path),
            slot.join(PAYLOAD),
            "path is derived from where the manifest was found"
        );

        let report = store
            .prune("host", "default", &Retention::keep_last(0))
            .unwrap();
        assert!(report.is_complete());
        assert!(!slot.exists(), "the slot that held the manifest is removed");
        assert!(
            victim.join("precious").exists(),
            "the claimed path is untouched"
        );

        // `remove` likewise re-locates by identity instead of using rec.path.
        let slot = plant(
            &store,
            "host/default/20260102-000000",
            &manifest_json("20260102-000000", "/"),
        );
        let mut rec = store.list(Some("host"), Some("default")).unwrap().remove(0);
        rec.path = victim.join("payload").to_string_lossy().into_owned();
        store.remove(&rec).unwrap();
        assert!(!slot.exists());
        assert!(victim.join("precious").exists());
    }

    #[test]
    fn invalid_manifests_are_skipped_without_failing_the_listing() {
        let (_tmp, store) = store();
        let good = write_backup(&store, "host", "default", "ok");
        // id does not match its slot dir.
        plant(
            &store,
            "host/default/20260101-000000",
            &manifest_json("20260101-000001", ""),
        );
        // id with a separator / traversal.
        plant(&store, "host/default/x", &manifest_json("../x", ""));
        // unparsable.
        plant(&store, "host/default/20260103-000000", "{not json");
        // manifest at the root would make the root a slot.
        fs::write(store.root().join(MANIFEST), manifest_json("backups", "")).unwrap();

        let listed = store.list(None, None).unwrap();
        assert_eq!(listed.len(), 1, "only the valid backup lists: {listed:?}");
        assert_eq!(listed[0].id, good.id);
        let report = store
            .prune("host", "default", &Retention::keep_last(0))
            .unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(
            store.root().exists(),
            "a root manifest never makes the root prunable"
        );
    }

    #[test]
    fn reserved_entries_are_never_walked() {
        let (_tmp, store) = store();
        for reserved in [
            ".orca-incoming",
            ".orca-trash",
            ".orca-removing-20260101-000000",
        ] {
            plant(
                &store,
                &format!("{reserved}/host/default/20260101-000000"),
                &manifest_json("20260101-000000", ""),
            );
        }
        fs::write(store.root().join(".orca-smb-stage"), "").unwrap();
        let _lock = store.lock().unwrap();
        assert!(store.list(None, None).unwrap().is_empty());
    }

    #[test]
    fn commit_is_atomic_relative_and_stamps_the_writer() {
        let (_tmp, store) = store();
        let rec = write_backup(&store, "host", "default", "x");
        let dir = store.root().join("host/default").join(&rec.id);
        let on_disk: BackupRecord =
            serde_json::from_str(&fs::read_to_string(dir.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(on_disk.path, PAYLOAD, "the manifest stores a relative path");
        assert!(
            !dir.join(MANIFEST_TMP).exists(),
            "no temp file is left behind"
        );
        let writer = rec.writer.expect("writer stamped");
        assert_eq!(writer.host, crate::host_identity::hostname());
        assert_eq!(on_disk.writer, Some(writer));
        assert!(Path::new(&rec.path).is_absolute());
    }

    #[test]
    fn shared_ids_carry_the_host_and_sort_chronologically() {
        let (_tmp, store) = store();
        let store = store.shared(true);
        let rec = write_backup(&store, "game-saves", "elden", "x");
        let host = id_host_segment(crate::host_identity::hostname());
        assert!(
            rec.id.ends_with(&format!("-{host}")),
            "id {} carries {host}",
            rec.id
        );
        assert_eq!(rec.date().len(), 10, "the date still derives from the id");

        let mut ids = vec![
            "20260101-000000-zeta".to_string(),
            "20260102-000000-alpha".to_string(),
            "20260101-120000".to_string(),
        ];
        ids.sort();
        assert_eq!(
            ids,
            [
                "20260101-000000-zeta",
                "20260101-120000",
                "20260102-000000-alpha"
            ],
            "the stamp prefix orders writers chronologically"
        );
        assert_eq!(id_host_segment("Bragi.local"), "bragi-local");
        assert_eq!(id_host_segment("///"), "host");
        assert!(validate_id(&rec.id).is_ok());
    }

    #[test]
    fn the_stage_lock_is_exclusive_until_dropped() {
        let (_tmp, store) = store();
        let held = store.lock().unwrap();
        assert!(store.root().join(STAGE_LOCK_FILE).exists());
        let err = store.lock_within(Duration::from_millis(300)).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        drop(held);
        let again = store.lock_within(Duration::from_millis(300));
        assert!(again.is_ok());
        drop(again);
        assert!(
            store.root().join(STAGE_LOCK_FILE).exists(),
            "the lock file is never deleted"
        );
    }

    #[test]
    fn sanitize_segment_blocks_traversal_and_separators() {
        assert_eq!(sanitize_segment("thor"), "thor");
        assert_eq!(sanitize_segment(".."), "_");
        assert_eq!(sanitize_segment("."), "_");
        assert_eq!(sanitize_segment(""), "_");
        assert_eq!(sanitize_segment("a/b"), "a_b");
        // A malicious layout segment cannot escape the store root.
        let (_tmp, store) = store();
        let evil = vec!["..".to_string(), "..".to_string(), "etc".to_string()];
        let slot = store.new_slot(&evil, "host", "default").unwrap();
        assert!(
            slot.payload_dir().starts_with(store.root()),
            "slot stays under root: {}",
            slot.payload_dir().display()
        );
    }

    #[test]
    fn slot_commit_writes_manifest_and_measures_payload() {
        let (_tmp, store) = store();
        let slot = store
            .new_slot(&coll("host", "default"), "host", "default")
            .unwrap();
        fs::write(slot.payload_dir().join("a.txt"), "hello").unwrap();
        fs::write(slot.payload_dir().join("b.txt"), "world!").unwrap();
        let rec = slot
            .commit(Some("sha256:x".into()), Some("n".into()))
            .unwrap();

        assert_eq!(rec.kind, "host");
        assert_eq!(rec.instance, "default");
        assert_eq!(rec.file_count, 2);
        assert_eq!(rec.size_bytes, 11); // "hello" + "world!"
        assert_eq!(rec.checksum.as_deref(), Some("sha256:x"));
        assert!(Path::new(&rec.path).join("a.txt").exists());
        assert!(
            store
                .root()
                .join("host/default")
                .join(&rec.id)
                .join(MANIFEST)
                .exists()
        );
    }

    #[test]
    fn abort_removes_the_slot() {
        let (_tmp, store) = store();
        let slot = store
            .new_slot(&coll("host", "default"), "host", "default")
            .unwrap();
        let dir = store.root().join("host/default").join(&slot.id);
        fs::write(slot.payload_dir().join("x"), "y").unwrap();
        assert!(dir.exists());
        slot.abort().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn list_is_newest_first_and_skips_incomplete() {
        let (_tmp, store) = store();
        let r1 = write_backup(&store, "host", "default", "one");
        // Force distinct, later ids so ordering is deterministic regardless of clock.
        let r2 = BackupRecord {
            system: String::new(),
            id: "29990101-000000".into(),
            ..write_backup(&store, "host", "default", "two")
        };
        // Re-commit r2 under the forced id by moving the dir.
        let old = store
            .root()
            .join("host/default")
            .join(&store.list(Some("host"), Some("default")).unwrap()[0].id);
        let newdir = store.root().join("host/default").join(&r2.id);
        fs::rename(&old, &newdir).unwrap();
        // Fix the manifest's id to match its new location.
        let mut rec: BackupRecord =
            serde_json::from_str(&fs::read_to_string(newdir.join(MANIFEST)).unwrap()).unwrap();
        rec.id = r2.id.clone();
        fs::write(newdir.join(MANIFEST), serde_json::to_string(&rec).unwrap()).unwrap();

        // An incomplete slot (no manifest) must be invisible.
        let incomplete = store
            .new_slot(&coll("host", "default"), "host", "default")
            .unwrap();
        fs::write(incomplete.payload_dir().join("z"), "z").unwrap();
        // (do not commit)

        let listed = store.list(Some("host"), Some("default")).unwrap();
        assert_eq!(listed.len(), 2, "incomplete slot excluded");
        assert_eq!(listed[0].id, "29990101-000000", "newest first");
        assert_eq!(listed[1].id, r1.id);
    }

    #[test]
    fn list_across_domains_and_instances() {
        let (_tmp, store) = store();
        write_backup(&store, "host", "default", "h");
        write_backup(&store, "sonarr", "main", "s1");
        write_backup(&store, "sonarr", "second", "s2");

        assert_eq!(store.list(None, None).unwrap().len(), 3);
        assert_eq!(store.list(Some("sonarr"), None).unwrap().len(), 2);
        assert_eq!(store.list(Some("sonarr"), Some("main")).unwrap().len(), 1);
        assert_eq!(store.list(Some("nope"), None).unwrap().len(), 0);
    }

    #[test]
    fn resolve_latest_and_by_id() {
        let (_tmp, store) = store();
        let r1 = write_backup(&store, "host", "default", "one");
        let newer = store.root().join("host/default").join("29990101-000000");
        fs::create_dir_all(newer.join(PAYLOAD)).unwrap();
        let rec = BackupRecord {
            system: String::new(),
            id: "29990101-000000".into(),
            kind: "host".into(),
            instance: "default".into(),
            created_ms: 99,
            path: newer.join(PAYLOAD).to_string_lossy().into_owned(),
            size_bytes: 0,
            file_count: 0,
            checksum: None,
            note: None,
            writer: None,
        };
        fs::write(newer.join(MANIFEST), serde_json::to_string(&rec).unwrap()).unwrap();

        let latest = store
            .resolve("host", "default", &BackupSelector::Latest)
            .unwrap();
        assert_eq!(latest.id, "29990101-000000");

        let by_id = store
            .resolve("host", "default", &BackupSelector::Id(r1.id.clone()))
            .unwrap();
        assert_eq!(by_id.id, r1.id);

        assert!(
            store
                .resolve("host", "default", &BackupSelector::Id("missing".into()))
                .is_err()
        );
        assert!(
            store
                .resolve("host", "empty", &BackupSelector::Latest)
                .is_err()
        );
    }

    #[test]
    fn prune_keep_last_removes_oldest() {
        let (_tmp, store) = store();
        // Create 5 backups with forced ascending ids.
        for i in 0..5 {
            let dir = store
                .root()
                .join("host/default")
                .join(format!("2026010{i}-000000"));
            fs::create_dir_all(dir.join(PAYLOAD)).unwrap();
            let rec = BackupRecord {
                system: String::new(),
                id: format!("2026010{i}-000000"),
                kind: "host".into(),
                instance: "default".into(),
                created_ms: i,
                path: dir.join(PAYLOAD).to_string_lossy().into_owned(),
                size_bytes: 0,
                file_count: 0,
                checksum: None,
                note: None,
                writer: None,
            };
            fs::write(dir.join(MANIFEST), serde_json::to_string(&rec).unwrap()).unwrap();
        }
        let removed = store
            .prune("host", "default", &Retention::keep_last(2))
            .unwrap();
        assert_eq!(removed.removed.len(), 3);
        assert!(removed.is_complete());
        let kept = store.list(Some("host"), Some("default")).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].id, "20260104-000000");
        assert_eq!(kept[1].id, "20260103-000000");
    }

    fn sample_record(id: &str) -> BackupRecord {
        BackupRecord {
            system: String::new(),
            id: id.into(),
            kind: "host".into(),
            instance: "default".into(),
            created_ms: 0,
            path: format!("/store/host/{id}/payload"),
            size_bytes: 0,
            file_count: 0,
            checksum: None,
            note: None,
            writer: None,
        }
    }

    /// Can this environment actually deny THIS process an unlink via mode bits?
    ///
    /// Measured, not inferred: make a directory unwritable and try to create in
    /// it. Root ignores mode bits entirely — which is how both permission tests
    /// below passed locally and failed in CI, where the suite runs as root — and
    /// some filesystems do not enforce them either. Asking the filesystem covers
    /// every such case without encoding a guess about the runner.
    fn denial_is_enforceable() -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(probe) = tempfile::tempdir() else {
                return false;
            };
            let locked = probe.path().join("locked");
            if std::fs::create_dir(&locked).is_err() {
                return false;
            }
            let mut perms = match std::fs::metadata(&locked) {
                Ok(m) => m.permissions(),
                Err(_) => return false,
            };
            perms.set_mode(0o500);
            if std::fs::set_permissions(&locked, perms).is_err() {
                return false;
            }
            let denied = std::fs::write(locked.join("probe"), b"x").is_err();
            let mut restore = std::fs::Permissions::from_mode(0o700);
            restore.set_mode(0o700);
            drop(std::fs::set_permissions(&locked, restore));
            if !denied {
                eprintln!(
                    "skipped: mode bits do not deny this process (root, or a \
                     filesystem that ignores them)"
                );
            }
            denied
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    // #610, as pure logic: the property that makes a no-op prune visible is that
    // intent and outcome are carried separately and compared. This holds on every
    // platform and every uid, so the invariant stays covered where the
    // permission-based tests below cannot run.
    #[test]
    fn a_prune_report_is_complete_only_when_outcome_matches_intent() {
        let mut report = PruneReport::default();
        assert!(report.is_complete(), "nothing selected, nothing to do");

        report.selected = 2;
        assert!(
            !report.is_complete(),
            "selected but not removed is the #610 shape and must never read as done"
        );
        assert_eq!(report.summary(), "selected 2, removed 0");

        report.failures.push(PruneFailure {
            id: "20260101-000000".into(),
            path: "/store/host/20260101-000000/manifest.json".into(),
            error: "Permission denied (os error 13)".into(),
        });
        assert!(!report.is_complete());

        // Even with the count satisfied, a recorded failure keeps it incomplete —
        // a partial success must not round up to success.
        report.failures.clear();
        report.removed = vec![
            sample_record("20260101-000000"),
            sample_record("20260102-000000"),
        ];
        assert!(report.is_complete());
        assert_eq!(report.summary(), "selected 2, removed 2");
        report.failures.push(PruneFailure {
            id: "20260103-000000".into(),
            path: "p".into(),
            error: "boom".into(),
        });
        assert!(!report.is_complete());
    }

    // #610: a prune that selects work and removes nothing must be a failure, not
    // a warning. The live shape was 107 snapshots selected, 0 removed, reported
    // as "WARNINGS: 9" — indistinguishable from partial success.
    #[test]
    fn a_prune_that_cannot_remove_reports_failure_not_success() {
        if !denial_is_enforceable() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = BackupStore::new(dir.path());
        for _ in 0..3 {
            let slot = store.new_slot(&["host".into()], "host", "default").unwrap();
            std::fs::write(slot.payload_dir().join("f"), b"x").unwrap();
            slot.commit(None, None).unwrap();
            // Ids are second-granular; keep them distinct.
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }

        // Make the collection undeletable the way the real fault did: the
        // snapshot dirs are selected, but the unlink is refused.
        let coll = dir.path().join("host");
        let mut perms = std::fs::metadata(&coll).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o500); // r-x: entries readable, none removable
        }
        std::fs::set_permissions(&coll, perms.clone()).unwrap();

        let report = store
            .prune("host", "default", &Retention::keep_last(1))
            .unwrap();

        // Restore permissions before asserting so a failure still cleans up.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = perms;
            p.set_mode(0o700);
            std::fs::set_permissions(&coll, p).unwrap();
        }

        assert_eq!(report.selected, 2, "two backups fall outside keep_last(1)");
        assert!(
            report.removed.is_empty(),
            "nothing could actually be removed"
        );
        assert!(!report.is_complete(), "this must not read as success");
        assert_eq!(
            report.failures.len(),
            2,
            "EVERY selected backup is attempted and reported, not just the first"
        );
        assert_eq!(report.summary(), "selected 2, removed 0");
        assert_eq!(
            store.list(Some("host"), Some("default")).unwrap().len(),
            3,
            "a prune that could not remove must not have destroyed them either"
        );
    }

    // #610 ask 3: know at configure time, not at prune time.
    #[test]
    fn prunability_is_knowable_before_a_prune_is_attempted() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("writable");
        std::fs::create_dir(&good).unwrap();
        assert!(
            BackupStore::check_prunable(&good).is_ok(),
            "a writable datastore probes clean"
        );

        // A directory that cannot be written is exactly the NAS-identity fault.
        // Only meaningful where mode bits actually deny the caller.
        #[cfg(unix)]
        if denial_is_enforceable() {
            use std::os::unix::fs::PermissionsExt;
            let bad = dir.path().join("readonly");
            std::fs::create_dir(&bad).unwrap();
            let mut p = std::fs::metadata(&bad).unwrap().permissions();
            p.set_mode(0o500);
            std::fs::set_permissions(&bad, p.clone()).unwrap();

            let err = BackupStore::check_prunable(&bad).unwrap_err();
            let msg = format!("{err:#}");

            p.set_mode(0o700);
            std::fs::set_permissions(&bad, p).unwrap();

            assert!(
                msg.contains("a prune here will fail"),
                "the error says what it means for retention: {msg}"
            );
            assert!(msg.contains("readonly"), "and names the directory: {msg}");
        }

        // A store that does not exist yet is not a failure — nothing to prune.
        assert!(BackupStore::check_prunable(&dir.path().join("absent")).is_ok());
    }

    #[test]
    fn prune_unbounded_removes_nothing() {
        let (_tmp, store) = store();
        write_backup(&store, "host", "default", "a");
        write_backup(&store, "host", "default", "b");
        let unbounded = Retention {
            keep_last: None,
            ..Retention::default()
        };
        assert!(
            store
                .prune("host", "default", &unbounded)
                .unwrap()
                .removed
                .is_empty()
        );
    }

    #[test]
    fn new_slot_disambiguates_same_second() {
        let (_tmp, store) = store();
        // Two slots in quick succession: ids must differ.
        let a = store
            .new_slot(&coll("host", "default"), "host", "default")
            .unwrap();
        // Pre-create the base id dir to force the suffix path deterministically.
        let base = utils::time::now().compact();
        let forced = store.root().join("host/default").join(&base);
        if !forced.exists() {
            fs::create_dir_all(&forced).unwrap();
        }
        let b = store
            .new_slot(&coll("host", "default"), "host", "default")
            .unwrap();
        assert_ne!(a.id, b.id);
    }

    /// Seed a committed backup at a specific wall-clock time (its id + created_ms
    /// both derive from `rfc3339`), so calendar-bucketed prune is deterministic.
    fn seed_at(store: &BackupStore, rfc3339: &str) -> String {
        seed_bytes(store, rfc3339, 0)
    }

    fn seed_bytes(store: &BackupStore, rfc3339: &str, size_bytes: u64) -> String {
        let ts = Timestamp::parse_rfc3339(rfc3339).unwrap();
        let id = ts.compact();
        let dir = store.root().join("host/default").join(&id);
        fs::create_dir_all(dir.join(PAYLOAD)).unwrap();
        let rec = BackupRecord {
            system: String::new(),
            id: id.clone(),
            kind: "host".into(),
            instance: "default".into(),
            created_ms: ts.unix_millis(),
            path: dir.join(PAYLOAD).to_string_lossy().into_owned(),
            size_bytes,
            file_count: 0,
            checksum: None,
            note: None,
            writer: None,
        };
        fs::write(dir.join(MANIFEST), serde_json::to_string(&rec).unwrap()).unwrap();
        id
    }

    #[test]
    fn prune_unions_keep_last_and_keep_daily() {
        let (_tmp, store) = store();
        // Two backups on day A, one each on days B and C (newest-first: C,B,A2,A1).
        let a1 = seed_at(&store, "2026-01-01T01:00:00Z");
        let a2 = seed_at(&store, "2026-01-01T02:00:00Z");
        let b1 = seed_at(&store, "2026-01-02T01:00:00Z");
        let c1 = seed_at(&store, "2026-01-03T01:00:00Z");

        // keep_last=1 → {C}; keep_daily=2 → newest of the 2 most-recent days {C,B}.
        // Union keeps {C,B}; both day-A backups are pruned.
        let retention = Retention {
            keep_last: Some(1),
            keep_daily: Some(2),
            keep_hourly: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            max_total_bytes: None,
        };
        let removed = store.prune("host", "default", &retention).unwrap();
        let removed_ids: HashSet<&String> = removed.removed.iter().map(|r| &r.id).collect();
        assert_eq!(removed.removed.len(), 2, "both day-A backups pruned");
        assert!(removed_ids.contains(&a1));
        assert!(removed_ids.contains(&a2));

        let kept: HashSet<String> = store
            .list(Some("host"), Some("default"))
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(kept, HashSet::from([b1, c1]));
    }

    #[test]
    fn prune_keep_hourly_keeps_newest_per_hour() {
        let (_tmp, store) = store();
        // Two backups in hour 01, one in hour 02, one in hour 03 of the same day.
        let h1a = seed_at(&store, "2026-01-01T01:10:00Z");
        let h1b = seed_at(&store, "2026-01-01T01:50:00Z");
        let h2 = seed_at(&store, "2026-01-01T02:30:00Z");
        let h3 = seed_at(&store, "2026-01-01T03:30:00Z");

        // keep_hourly=2 → newest of the 2 most-recent hours (03,02) = {h3,h2}.
        let retention = Retention {
            keep_last: None,
            keep_hourly: Some(2),
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            max_total_bytes: None,
        };
        let removed = store.prune("host", "default", &retention).unwrap();
        let removed_ids: HashSet<&String> = removed.removed.iter().map(|r| &r.id).collect();
        // hour 01 entirely dropped (older than the 2 kept hours).
        assert!(removed_ids.contains(&h1a));
        assert!(removed_ids.contains(&h1b));
        let kept: HashSet<String> = store
            .list(Some("host"), Some("default"))
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(kept, HashSet::from([h2, h3]));
    }

    #[test]
    fn prune_size_cap_keeps_newest_that_fit() {
        let (_tmp, store) = store();
        // Four 10-byte backups, oldest→newest; cap at 25 bytes → newest 2 fit.
        let o1 = seed_bytes(&store, "2026-01-01T01:00:00Z", 10);
        let o2 = seed_bytes(&store, "2026-01-01T02:00:00Z", 10);
        let o3 = seed_bytes(&store, "2026-01-01T03:00:00Z", 10);
        let o4 = seed_bytes(&store, "2026-01-01T04:00:00Z", 10);

        let retention = Retention {
            keep_last: None,
            keep_hourly: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            max_total_bytes: Some(25),
        };
        let removed = store.prune("host", "default", &retention).unwrap();
        let removed_ids: HashSet<&String> = removed.removed.iter().map(|r| &r.id).collect();
        assert_eq!(
            removed.removed.len(),
            2,
            "two oldest dropped to fit the 25-byte cap"
        );
        assert!(removed_ids.contains(&o1));
        assert!(removed_ids.contains(&o2));
        let kept: HashSet<String> = store
            .list(Some("host"), Some("default"))
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(kept, HashSet::from([o3, o4]));
    }
}
