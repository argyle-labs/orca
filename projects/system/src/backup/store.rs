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
//! compact UTC stamp (`YYYYMMDD-HHMMSS`). A backup's identity (`kind`+`instance`)
//! lives in its manifest; listing and selection filter on that manifest identity,
//! so a provider may file backups under any layout. The store owns dating,
//! listing, selection, and retention pruning ([[service-backup-restore-location-agnostic]]).
//! A slot dir with a `manifest.json` is a complete backup; one without is
//! in-progress and is skipped by `list`/`resolve`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use contract::backup::{BackupRecord, BackupSelector, Retention};
use utils::time::Timestamp;

const MANIFEST: &str = "manifest.json";
const PAYLOAD: &str = "payload";

/// A filesystem-backed store of dated backups.
#[derive(Debug, Clone)]
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    /// A store rooted at `root`. The directory is created lazily on first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
    /// stamp, disambiguated with a `-N` suffix if a slot for this second already
    /// exists so ids stay unique and sortable.
    pub fn new_slot(
        &self,
        collection: &[String],
        kind: &str,
        instance: &str,
    ) -> Result<BackupSlot> {
        let now = utils::time::now();
        let created_ms = now.unix_millis();
        let base = now.compact();
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
        })
    }

    /// List backups, newest first. `domain` (kind) / `instance` match against the
    /// manifest identity, so a backup filed under any layout
    /// (`hosts/thor`) is found by `list(Some("host"), Some("thor"))`.
    /// `None` matches any value on that axis. In-progress slots (no manifest) are
    /// skipped; a missing tree lists as empty.
    pub fn list(&self, domain: Option<&str>, instance: Option<&str>) -> Result<Vec<BackupRecord>> {
        let mut out = self.all_records()?;
        out.retain(|r| {
            domain.is_none_or(|k| r.kind == k) && instance.is_none_or(|i| r.instance == i)
        });
        // Newest first; the id stamp sorts chronologically.
        out.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(out)
    }

    /// Every complete backup record in the store, in arbitrary order. Walks the
    /// whole tree for `manifest.json` files, matching any layout a provider chose.
    /// A missing root lists as empty.
    fn all_records(&self) -> Result<Vec<BackupRecord>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(anyhow!("read dir {}: {e}", dir.display())),
            };
            for entry in rd {
                let entry = entry?;
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    // Never descend into a slot's payload — only slot metadata.
                    if entry.file_name() != PAYLOAD {
                        stack.push(entry.path());
                    }
                } else if entry.file_name() == MANIFEST
                    && let Some(rec) = read_manifest(&entry.path())?
                {
                    out.push(rec);
                }
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

    /// Delete the backup identified by `rec` (its whole slot dir). The slot dir is
    /// the parent of the record's payload path, so removal is layout-agnostic.
    pub fn remove(&self, rec: &BackupRecord) -> Result<()> {
        let Some(dir) = Path::new(&rec.path).parent() else {
            return Ok(());
        };
        if dir.exists() {
            fs::remove_dir_all(dir).with_context(|| format!("remove backup {}", dir.display()))?;
        }
        Ok(())
    }

    /// Apply `retention` to `(domain, instance)`, deleting the backups that fall
    /// outside the policy. Returns the records that were removed (newest first).
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
    ) -> Result<Vec<BackupRecord>> {
        if retention.is_unbounded() {
            return Ok(Vec::new());
        }
        let records = self.list(Some(domain), Some(instance))?; // newest first
        let keep_ids = retained_ids(&records, retention);

        let mut removed = Vec::new();
        for rec in records {
            if !keep_ids.contains(&rec.id) {
                self.remove(&rec)?;
                removed.push(rec);
            }
        }
        Ok(removed)
    }
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
}

impl BackupSlot {
    /// The directory the provider writes payload files into.
    pub fn payload_dir(&self) -> &Path {
        &self.payload
    }

    /// Finalize: measure the payload, write `manifest.json`, and return the
    /// record. `checksum`/`note` are provider-supplied metadata.
    pub fn commit(self, checksum: Option<String>, note: Option<String>) -> Result<BackupRecord> {
        let (size_bytes, file_count) = dir_size(&self.payload)?;
        let rec = BackupRecord {
            id: self.id,
            kind: self.domain,
            instance: self.instance,
            created_ms: self.created_ms,
            path: self.payload.to_string_lossy().into_owned(),
            size_bytes,
            file_count,
            checksum,
            note,
        };
        let manifest = self.dir.join(MANIFEST);
        let json = serde_json::to_string_pretty(&rec).context("serialize backup manifest")?;
        fs::write(&manifest, json)
            .with_context(|| format!("write manifest {}", manifest.display()))?;
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

/// The set of backup ids `retention` keeps, given `records` newest-first. The
/// count and calendar axes union — a record kept by ANY of them survives — and
/// `max_total_bytes` then caps the result, trimming oldest until it fits. When
/// the size cap is the only bound, it keeps the newest backups that fit.
fn retained_ids(records: &[BackupRecord], retention: &Retention) -> HashSet<String> {
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
            for rec in records.iter().take(n as usize) {
                keep.insert(rec.id.clone());
            }
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
        for rec in records {
            keep.insert(rec.id.clone());
        }
    }

    // Size cap: walk the kept records newest-first and drop the oldest that push
    // the total past the budget. The newest kept backup always survives.
    if let Some(cap) = retention.max_total_bytes {
        let mut total: u64 = 0;
        let mut first = true;
        for rec in records {
            if !keep.contains(&rec.id) {
                continue;
            }
            let next = total.saturating_add(rec.size_bytes);
            if first || next <= cap {
                total = next;
                first = false;
            } else {
                keep.remove(&rec.id);
            }
        }
    }
    keep
}

/// For one calendar axis: keep the newest record in each of the `n` most-recent
/// distinct periods (period key from `bucket`). `records` MUST be newest-first,
/// so the first record seen for a period is that period's newest.
fn keep_per_bucket(
    records: &[BackupRecord],
    n: Option<u32>,
    bucket: impl Fn(&Timestamp) -> String,
    keep: &mut HashSet<String>,
) {
    let Some(n) = n else { return };
    if n == 0 {
        return;
    }
    let mut periods: Vec<String> = Vec::new(); // distinct periods, newest-first
    for rec in records {
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
        keep.insert(rec.id.clone());
    }
}

/// Read a manifest file into a record. `Ok(None)` when the file is absent (an
/// incomplete slot); an error only on a present-but-unreadable/corrupt manifest.
fn read_manifest(path: &Path) -> Result<Option<BackupRecord>> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let rec = serde_json::from_str(&s)
                .with_context(|| format!("parse manifest {}", path.display()))?;
            Ok(Some(rec))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("read manifest {}: {e}", path.display())),
    }
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
        assert_eq!(removed.len(), 1);
        assert!(store.list(Some("host"), Some("thor")).unwrap().is_empty());
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
            id: "29990101-000000".into(),
            kind: "host".into(),
            instance: "default".into(),
            created_ms: 99,
            path: newer.join(PAYLOAD).to_string_lossy().into_owned(),
            size_bytes: 0,
            file_count: 0,
            checksum: None,
            note: None,
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
                id: format!("2026010{i}-000000"),
                kind: "host".into(),
                instance: "default".into(),
                created_ms: i,
                path: dir.join(PAYLOAD).to_string_lossy().into_owned(),
                size_bytes: 0,
                file_count: 0,
                checksum: None,
                note: None,
            };
            fs::write(dir.join(MANIFEST), serde_json::to_string(&rec).unwrap()).unwrap();
        }
        let removed = store
            .prune("host", "default", &Retention::keep_last(2))
            .unwrap();
        assert_eq!(removed.len(), 3);
        let kept = store.list(Some("host"), Some("default")).unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].id, "20260104-000000");
        assert_eq!(kept[1].id, "20260103-000000");
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
            id: id.clone(),
            kind: "host".into(),
            instance: "default".into(),
            created_ms: ts.unix_millis(),
            path: dir.join(PAYLOAD).to_string_lossy().into_owned(),
            size_bytes,
            file_count: 0,
            checksum: None,
            note: None,
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
        let removed_ids: HashSet<&String> = removed.iter().map(|r| &r.id).collect();
        assert_eq!(removed.len(), 2, "both day-A backups pruned");
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
        let removed_ids: HashSet<&String> = removed.iter().map(|r| &r.id).collect();
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
        let removed_ids: HashSet<&String> = removed.iter().map(|r| &r.id).collect();
        assert_eq!(
            removed.len(),
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
