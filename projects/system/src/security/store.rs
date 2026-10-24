use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use contract::paging::{Page, PageParams};
use contract::security::{ScanReport, ScanRequest, ScannedFinding, Severity, scanners};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utils::id::Id;

pub const DAY_MS: i64 = 86_400_000;
pub const DEFAULT_RESOLVED_RETENTION_DAYS: i64 = 30;
pub const DEFAULT_MAX_RESOLVED_PER_UNIT: usize = 500;

/// A stored finding. Identity is (member, unit, scanner, id, package); the
/// member is the system that ran the scan and holds this row.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub member_id: String,
    pub unit_id: Id,
    pub scanner: String,
    #[serde(flatten)]
    pub finding: ScannedFinding,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reopened_ms: Option<i64>,
    #[serde(default)]
    pub reopen_count: u32,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MergeCounts {
    pub new: usize,
    pub updated: usize,
    pub resolved: usize,
    pub reopened: usize,
}

/// Bounds on resolved rows kept after a merge: resolved findings older than
/// `resolved_max_age_ms` are dropped, then at most `max_resolved_per_unit`
/// most-recently-resolved rows are kept per (member, unit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    pub resolved_max_age_ms: i64,
    pub max_resolved_per_unit: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            resolved_max_age_ms: DEFAULT_RESOLVED_RETENTION_DAYS * DAY_MS,
            max_resolved_per_unit: DEFAULT_MAX_RESOLVED_PER_UNIT,
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ScannerOutcome {
    pub scanner: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub counts: MergeCounts,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    pub member_id: String,
    pub unit_id: Id,
    pub scanners: Vec<ScannerOutcome>,
}

#[derive(Clone, Debug, Default)]
pub struct ListQuery {
    pub unit_id: Option<Id>,
    pub scanner: Option<String>,
    pub min_severity: Option<Severity>,
    pub include_resolved: bool,
    pub page: PageParams,
}

#[derive(Clone, Debug)]
pub struct DetailQuery {
    pub unit_id: Id,
    pub scanner: String,
    pub id: String,
    pub package: Option<String>,
}

type Key = (String, String, String, String, Option<String>);

fn key_of(f: &Finding) -> Key {
    (
        f.member_id.clone(),
        f.unit_id.to_string(),
        f.scanner.clone(),
        f.finding.id.clone(),
        f.finding.package.clone(),
    )
}

fn in_coverage(f: &ScannedFinding, coverage: &HashSet<&str>) -> bool {
    f.package.as_deref().is_some_and(|p| coverage.contains(p))
        || f.file.as_deref().is_some_and(|p| coverage.contains(p))
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct FindingsStore {
    findings: Vec<Finding>,
}

impl FindingsStore {
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    pub fn merge(
        &mut self,
        member_id: &str,
        unit_id: &Id,
        scanner: &str,
        report: ScanReport,
        now_ms: i64,
        retention: Retention,
    ) -> MergeCounts {
        let mut index: BTreeMap<Key, usize> = self
            .findings
            .iter()
            .enumerate()
            .map(|(i, f)| (key_of(f), i))
            .collect();
        let mut counts = MergeCounts::default();
        let mut seen = vec![false; self.findings.len()];
        for sf in report.findings {
            let k = (
                member_id.to_string(),
                unit_id.to_string(),
                scanner.to_string(),
                sf.id.clone(),
                sf.package.clone(),
            );
            match index.get(&k) {
                Some(&i) if seen[i] => self.findings[i].finding = sf,
                Some(&i) => {
                    let f = &mut self.findings[i];
                    if f.resolved_ms.take().is_some() {
                        f.reopened_ms = Some(now_ms);
                        f.reopen_count += 1;
                        counts.reopened += 1;
                    } else {
                        counts.updated += 1;
                    }
                    f.finding = sf;
                    f.last_seen_ms = now_ms;
                    seen[i] = true;
                }
                None => {
                    index.insert(k, self.findings.len());
                    seen.push(true);
                    self.findings.push(Finding {
                        member_id: member_id.to_string(),
                        unit_id: unit_id.clone(),
                        scanner: scanner.to_string(),
                        finding: sf,
                        first_seen_ms: now_ms,
                        last_seen_ms: now_ms,
                        resolved_ms: None,
                        reopened_ms: None,
                        reopen_count: 0,
                    });
                    counts.new += 1;
                }
            }
        }
        let coverage: Option<HashSet<&str>> = report
            .coverage
            .as_ref()
            .map(|c| c.iter().map(String::as_str).collect());
        if coverage.is_some() || report.complete {
            for (f, seen) in self.findings.iter_mut().zip(seen) {
                let scoped = coverage.as_ref().is_none_or(|c| in_coverage(&f.finding, c));
                if !seen
                    && scoped
                    && f.resolved_ms.is_none()
                    && f.member_id == member_id
                    && &f.unit_id == unit_id
                    && f.scanner == scanner
                {
                    f.resolved_ms = Some(now_ms);
                    counts.resolved += 1;
                }
            }
        }
        self.prune(now_ms, retention);
        counts
    }

    fn prune(&mut self, now_ms: i64, retention: Retention) {
        let cutoff = now_ms - retention.resolved_max_age_ms;
        self.findings
            .retain(|f| f.resolved_ms.is_none_or(|r| r >= cutoff));
        let mut resolved: HashMap<(String, String), Vec<i64>> = HashMap::new();
        for f in &self.findings {
            if let Some(r) = f.resolved_ms {
                resolved
                    .entry((f.member_id.clone(), f.unit_id.to_string()))
                    .or_default()
                    .push(r);
            }
        }
        let mut floor: HashMap<(String, String), (i64, usize)> = HashMap::new();
        for (k, mut times) in resolved {
            if times.len() > retention.max_resolved_per_unit {
                times.sort_unstable_by(|a, b| b.cmp(a));
                let edge = times[retention.max_resolved_per_unit - 1];
                let at_edge = times[..retention.max_resolved_per_unit]
                    .iter()
                    .filter(|t| **t == edge)
                    .count();
                floor.insert(k, (edge, at_edge));
            }
        }
        if floor.is_empty() {
            return;
        }
        self.findings
            .sort_by_key(|f| std::cmp::Reverse(f.resolved_ms));
        self.findings.retain(|f| {
            let Some(r) = f.resolved_ms else {
                return true;
            };
            match floor.get_mut(&(f.member_id.clone(), f.unit_id.to_string())) {
                None => true,
                Some((edge, _)) if r > *edge => true,
                Some((edge, left)) if r == *edge && *left > 0 => {
                    *left -= 1;
                    true
                }
                Some(_) => false,
            }
        });
    }

    pub fn list(&self, q: &ListQuery) -> Page<Finding> {
        let mut rows: Vec<Finding> = self
            .findings
            .iter()
            .filter(|f| q.include_resolved || f.resolved_ms.is_none())
            .filter(|f| q.unit_id.as_ref().is_none_or(|u| &f.unit_id == u))
            .filter(|f| q.scanner.as_deref().is_none_or(|s| f.scanner == s))
            .filter(|f| q.min_severity.is_none_or(|m| f.finding.severity >= m))
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            b.finding
                .severity
                .cmp(&a.finding.severity)
                .then_with(|| b.last_seen_ms.cmp(&a.last_seen_ms))
        });
        Page::from_slice(rows, &q.page)
    }

    pub fn detail(&self, q: &DetailQuery) -> Option<&Finding> {
        self.findings.iter().find(|f| {
            f.unit_id == q.unit_id
                && f.scanner == q.scanner
                && f.finding.id == q.id
                && f.finding.package == q.package
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse findings store {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read findings store {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).context("encode findings store")?;
        utils::atomic::write_mkdir(path, &bytes)
    }
}

static STORE_LOCK: Mutex<()> = Mutex::new(());

pub fn store_path() -> Result<PathBuf> {
    utils::home::orca_home()
        .map(|h| h.join("security").join("findings.json"))
        .ok_or_else(|| anyhow!("cannot resolve orca state dir"))
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

pub fn record(
    path: &Path,
    member_id: &str,
    unit_id: &Id,
    scanner: &str,
    report: ScanReport,
    retention: Retention,
) -> Result<MergeCounts> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create findings dir {}", dir.display()))?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(path))
        .with_context(|| format!("open findings lock for {}", path.display()))?;
    lock.lock()
        .with_context(|| format!("lock findings store {}", path.display()))?;
    let mut store = FindingsStore::load(path)?;
    let counts = store.merge(
        member_id,
        unit_id,
        scanner,
        report,
        utils::time::now_millis_since_epoch(),
        retention,
    );
    store.save(path)?;
    Ok(counts)
}

/// Run the selected scanners against one unit and record each report. A
/// failing scanner is reported in its outcome and leaves its findings untouched.
pub async fn scan_into(
    path: &Path,
    member_id: &str,
    unit_id: &Id,
    scanner: Option<&str>,
    retention: Retention,
) -> Result<ScanSummary> {
    let selected: Vec<_> = scanners()
        .into_iter()
        .filter(|s| scanner.is_none_or(|n| s.name() == n))
        .collect();
    if selected.is_empty() {
        match scanner {
            Some(n) => bail!("no security scanner named '{n}'"),
            None => bail!("no security scanners registered"),
        }
    }
    let mut outcomes = Vec::with_capacity(selected.len());
    for s in selected {
        let name = s.name().to_string();
        let result = match s
            .scan(ScanRequest {
                unit_id: unit_id.clone(),
            })
            .await
        {
            Ok(report) => record(path, member_id, unit_id, &name, report, retention),
            Err(e) => Err(e),
        };
        outcomes.push(match result {
            Ok(counts) => ScannerOutcome {
                scanner: name,
                ok: true,
                error: None,
                counts,
            },
            Err(e) => ScannerOutcome {
                scanner: name,
                ok: false,
                error: Some(format!("{e:#}")),
                counts: MergeCounts::default(),
            },
        });
    }
    Ok(ScanSummary {
        member_id: member_id.to_string(),
        unit_id: unit_id.clone(),
        scanners: outcomes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::BoxFuture;
    use contract::security::{SecurityScanner, deregister_scanner, register_scanner};
    use std::sync::Arc;

    const M: &str = "member-a";

    fn u(n: u8) -> Id {
        format!("00000000-0000-7000-8000-0000000000{n:02}")
            .parse()
            .unwrap()
    }

    fn sf(id: &str, pkg: Option<&str>, sev: Severity) -> ScannedFinding {
        ScannedFinding {
            id: id.into(),
            severity: sev,
            title: None,
            package: pkg.map(str::to_string),
            file: None,
            installed_version: Some("1.0".into()),
            fixed_version: None,
        }
    }

    fn report(findings: Vec<ScannedFinding>, complete: bool) -> ScanReport {
        ScanReport {
            findings,
            complete,
            coverage: None,
        }
    }

    fn merge(
        s: &mut FindingsStore,
        unit: u8,
        scanner: &str,
        r: ScanReport,
        now: i64,
    ) -> MergeCounts {
        s.merge(M, &u(unit), scanner, r, now, Retention::default())
    }

    fn temp_store() -> PathBuf {
        utils::home::orca_home()
            .expect("sandboxed orca home")
            .join(format!("security-test-{}", utils::id::new()))
            .join("findings.json")
    }

    fn counts(new: usize, updated: usize, resolved: usize, reopened: usize) -> MergeCounts {
        MergeCounts {
            new,
            updated,
            resolved,
            reopened,
        }
    }

    #[test]
    fn merge_dedupes_and_updates_last_seen() {
        let mut s = FindingsStore::default();
        let c = merge(
            &mut s,
            1,
            "trivy",
            report(vec![sf("CVE-1", Some("openssl"), Severity::High)], true),
            10,
        );
        assert_eq!(c, counts(1, 0, 0, 0));
        let mut again = sf("CVE-1", Some("openssl"), Severity::Critical);
        again.fixed_version = Some("1.1".into());
        let c = merge(&mut s, 1, "trivy", report(vec![again], true), 20);
        assert_eq!(c, counts(0, 1, 0, 0));
        assert_eq!(s.findings().len(), 1);
        let f = &s.findings()[0];
        assert_eq!((f.first_seen_ms, f.last_seen_ms), (10, 20));
        assert_eq!(f.finding.severity, Severity::Critical);
        assert_eq!(f.finding.fixed_version.as_deref(), Some("1.1"));
    }

    #[test]
    fn duplicate_key_in_one_report_counts_once() {
        let mut s = FindingsStore::default();
        let c = merge(
            &mut s,
            1,
            "trivy",
            report(
                vec![
                    sf("CVE-1", Some("a"), Severity::Low),
                    sf("CVE-1", Some("a"), Severity::High),
                ],
                true,
            ),
            1,
        );
        assert_eq!(c, counts(1, 0, 0, 0));
        assert_eq!(s.findings().len(), 1);
        assert_eq!(s.findings()[0].finding.severity, Severity::High);
        let c = merge(
            &mut s,
            1,
            "trivy",
            report(
                vec![
                    sf("CVE-1", Some("a"), Severity::Low),
                    sf("CVE-1", Some("a"), Severity::Low),
                ],
                true,
            ),
            2,
        );
        assert_eq!(c, counts(0, 1, 0, 0));
    }

    #[test]
    fn identity_includes_member_unit_scanner_and_package() {
        let mut s = FindingsStore::default();
        merge(
            &mut s,
            1,
            "trivy",
            report(vec![sf("CVE-1", Some("a"), Severity::Low)], false),
            1,
        );
        merge(
            &mut s,
            1,
            "trivy",
            report(vec![sf("CVE-1", Some("b"), Severity::Low)], false),
            1,
        );
        merge(
            &mut s,
            2,
            "trivy",
            report(vec![sf("CVE-1", Some("a"), Severity::Low)], false),
            1,
        );
        merge(
            &mut s,
            1,
            "other",
            report(vec![sf("CVE-1", Some("a"), Severity::Low)], false),
            1,
        );
        s.merge(
            "member-b",
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", Some("a"), Severity::Low)], false),
            1,
            Retention::default(),
        );
        assert_eq!(s.findings().len(), 5);
    }

    #[test]
    fn complete_scan_resolves_absent_and_reopen_is_distinct_from_new() {
        let mut s = FindingsStore::default();
        merge(
            &mut s,
            1,
            "trivy",
            report(
                vec![
                    sf("CVE-1", None, Severity::High),
                    sf("CVE-2", None, Severity::Low),
                ],
                true,
            ),
            10,
        );
        merge(
            &mut s,
            2,
            "trivy",
            report(vec![sf("CVE-2", None, Severity::Low)], true),
            10,
        );
        merge(
            &mut s,
            1,
            "exposure",
            report(vec![sf("port-32400", None, Severity::High)], true),
            10,
        );

        let c = merge(
            &mut s,
            1,
            "trivy",
            report(vec![sf("CVE-1", None, Severity::High)], false),
            20,
        );
        assert_eq!(c.resolved, 0);

        let c = merge(
            &mut s,
            1,
            "trivy",
            report(vec![sf("CVE-1", None, Severity::High)], true),
            30,
        );
        assert_eq!(c, counts(0, 1, 1, 0));
        let open = s.list(&ListQuery::default()).items;
        assert_eq!(open.len(), 3);

        let c = merge(
            &mut s,
            1,
            "trivy",
            report(
                vec![
                    sf("CVE-1", None, Severity::High),
                    sf("CVE-2", None, Severity::Low),
                    sf("CVE-3", None, Severity::Low),
                ],
                true,
            ),
            40,
        );
        assert_eq!(c, counts(1, 1, 0, 1));
        let back = s
            .detail(&DetailQuery {
                unit_id: u(1),
                scanner: "trivy".into(),
                id: "CVE-2".into(),
                package: None,
            })
            .unwrap();
        assert_eq!(
            (
                back.first_seen_ms,
                back.resolved_ms,
                back.reopened_ms,
                back.reopen_count
            ),
            (10, None, Some(40), 1)
        );
    }

    #[test]
    fn partial_coverage_resolves_only_covered_targets() {
        let mut s = FindingsStore::default();
        let mut in_file = sf("CVE-3", None, Severity::Low);
        in_file.file = Some("/app/bin".into());
        merge(
            &mut s,
            1,
            "trivy",
            report(
                vec![
                    sf("CVE-1", Some("openssl"), Severity::High),
                    sf("CVE-2", Some("zlib"), Severity::High),
                    in_file,
                ],
                true,
            ),
            10,
        );
        let c = merge(
            &mut s,
            1,
            "trivy",
            ScanReport {
                findings: vec![],
                complete: true,
                coverage: Some(vec!["openssl".into(), "/app/bin".into()]),
            },
            20,
        );
        assert_eq!(c.resolved, 2);
        let open: Vec<_> = s
            .list(&ListQuery::default())
            .items
            .into_iter()
            .map(|f| f.finding.id)
            .collect();
        assert_eq!(open, ["CVE-2"]);
    }

    #[test]
    fn retention_prunes_old_resolved_and_caps_per_unit() {
        let mut s = FindingsStore::default();
        let retention = Retention {
            resolved_max_age_ms: 100,
            max_resolved_per_unit: 2,
        };
        let all: Vec<_> = (0..4)
            .map(|i| sf(&format!("CVE-{i}"), None, Severity::Low))
            .collect();
        s.merge(M, &u(1), "trivy", report(all, true), 0, retention);
        for (i, now) in [(0, 10), (1, 20), (2, 30), (3, 40)] {
            let keep: Vec<_> = (i + 1..4)
                .map(|j| sf(&format!("CVE-{j}"), None, Severity::Low))
                .collect();
            s.merge(M, &u(1), "trivy", report(keep, true), now, retention);
        }
        let ids: HashSet<_> = s.findings().iter().map(|f| f.finding.id.clone()).collect();
        assert_eq!(
            ids,
            HashSet::from(["CVE-2".to_string(), "CVE-3".to_string()])
        );

        s.merge(M, &u(1), "trivy", report(vec![], false), 200, retention);
        assert!(s.findings().is_empty());
    }

    #[test]
    fn list_filters_and_orders_by_severity() {
        let mut s = FindingsStore::default();
        merge(
            &mut s,
            1,
            "trivy",
            report(
                vec![
                    sf("low", None, Severity::Low),
                    sf("crit", None, Severity::Critical),
                    sf("med", None, Severity::Medium),
                ],
                true,
            ),
            1,
        );
        let page = s.list(&ListQuery {
            min_severity: Some(Severity::Medium),
            ..Default::default()
        });
        let ids: Vec<_> = page.items.iter().map(|f| f.finding.id.as_str()).collect();
        assert_eq!(ids, ["crit", "med"]);
        assert!(
            s.list(&ListQuery {
                unit_id: Some(u(9)),
                ..Default::default()
            })
            .items
            .is_empty()
        );
    }

    #[test]
    fn store_round_trips_through_file_under_orca_home() {
        let path = temp_store();
        assert!(FindingsStore::load(&path).unwrap().findings().is_empty());
        record(
            &path,
            M,
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", Some("a"), Severity::High)], true),
            Retention::default(),
        )
        .unwrap();
        let loaded = FindingsStore::load(&path).unwrap();
        assert_eq!(loaded.findings().len(), 1);
        assert_eq!(loaded.list(&ListQuery::default()).items.len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_records_never_drop_writes() {
        let path = temp_store();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    record(
                        &path,
                        M,
                        &u(i),
                        "trivy",
                        report(vec![sf("CVE-1", None, Severity::High)], true),
                        Retention::default(),
                    )
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(FindingsStore::load(&path).unwrap().findings().len(), 8);
        let lock = lock_path(&path);
        assert!(lock.exists());
        let held = OpenOptions::new().write(true).open(&lock).unwrap();
        held.lock().unwrap();
        assert!(
            OpenOptions::new()
                .write(true)
                .open(&lock)
                .unwrap()
                .try_lock()
                .is_err()
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    struct FakeScanner {
        name: &'static str,
        fail: bool,
    }

    impl SecurityScanner for FakeScanner {
        fn name(&self) -> &str {
            self.name
        }
        fn scan(&self, request: ScanRequest) -> BoxFuture<'_, Result<ScanReport>> {
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    bail!("scanner down");
                }
                Ok(report(
                    vec![sf(
                        &format!("CVE-{}", request.unit_id),
                        None,
                        Severity::High,
                    )],
                    true,
                ))
            })
        }
    }

    #[tokio::test]
    async fn scan_dispatches_to_registered_scanners_and_isolates_failures() {
        register_scanner(Arc::new(FakeScanner {
            name: "sec-store-ok",
            fail: false,
        }));
        register_scanner(Arc::new(FakeScanner {
            name: "sec-store-bad",
            fail: true,
        }));
        let path = temp_store();
        let unit = u(3);

        let one = scan_into(&path, M, &unit, Some("sec-store-ok"), Retention::default())
            .await
            .unwrap();
        assert_eq!(one.scanners.len(), 1);
        assert!(one.scanners[0].ok);
        assert_eq!(one.scanners[0].counts.new, 1);

        let all = scan_into(&path, M, &unit, None, Retention::default())
            .await
            .unwrap();
        let ok = all
            .scanners
            .iter()
            .find(|o| o.scanner == "sec-store-ok")
            .unwrap();
        let bad = all
            .scanners
            .iter()
            .find(|o| o.scanner == "sec-store-bad")
            .unwrap();
        assert!(ok.ok && ok.counts.updated == 1);
        assert!(!bad.ok);
        assert!(bad.error.as_deref().unwrap().contains("scanner down"));

        let stored = FindingsStore::load(&path).unwrap();
        assert!(
            stored
                .findings()
                .iter()
                .all(|f| f.scanner != "sec-store-bad")
        );
        assert!(
            stored
                .findings()
                .iter()
                .any(|f| f.scanner == "sec-store-ok" && f.unit_id == unit)
        );

        assert!(
            scan_into(&path, M, &unit, Some("nope"), Retention::default())
                .await
                .is_err()
        );
        assert!(deregister_scanner("sec-store-ok"));
        assert!(deregister_scanner("sec-store-bad"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
