#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use utils::id::Id;

use crate::BoxFuture;
use crate::paging::{Page, PageParams};

#[derive(
    Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Unknown,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ScannedFinding {
    pub id: String,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_version: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct ScanReport {
    pub findings: Vec<ScannedFinding>,
    #[serde(default)]
    pub complete: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ScanRequest {
    pub unit_id: Id,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub unit_id: Id,
    pub scanner: String,
    #[serde(flatten)]
    pub finding: ScannedFinding,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ms: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ScanArgs {
    pub unit_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanner: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeCounts {
    pub new: usize,
    pub updated: usize,
    pub resolved: usize,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ScannerOutcome {
    pub scanner: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub counts: MergeCounts,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ScanSummary {
    pub unit_id: Id,
    pub scanners: Vec<ScannerOutcome>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct FindingsListArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<Severity>,
    #[serde(default)]
    pub include_resolved: bool,
    #[serde(flatten)]
    pub page: PageParams,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct FindingsDetailArgs {
    pub unit_id: Id,
    pub scanner: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

type Key = (String, String, String, Option<String>);

fn key(unit_id: &str, scanner: &str, id: &str, package: Option<&str>) -> Key {
    (
        unit_id.to_string(),
        scanner.to_string(),
        id.to_string(),
        package.map(str::to_string),
    )
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
        unit_id: &Id,
        scanner: &str,
        report: ScanReport,
        now_ms: i64,
    ) -> MergeCounts {
        let mut index: BTreeMap<Key, usize> = self
            .findings
            .iter()
            .enumerate()
            .map(|(i, f)| {
                (
                    key(
                        &f.unit_id,
                        &f.scanner,
                        &f.finding.id,
                        f.finding.package.as_deref(),
                    ),
                    i,
                )
            })
            .collect();
        let mut counts = MergeCounts::default();
        let mut seen = vec![false; self.findings.len()];
        for sf in report.findings {
            let k = key(unit_id, scanner, &sf.id, sf.package.as_deref());
            match index.get(&k) {
                Some(&i) => {
                    let f = &mut self.findings[i];
                    if f.resolved_ms.take().is_some() {
                        counts.new += 1;
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
                        unit_id: unit_id.clone(),
                        scanner: scanner.to_string(),
                        finding: sf,
                        first_seen_ms: now_ms,
                        last_seen_ms: now_ms,
                        resolved_ms: None,
                    });
                    counts.new += 1;
                }
            }
        }
        if report.complete {
            for (f, seen) in self.findings.iter_mut().zip(seen) {
                if !seen && &f.unit_id == unit_id && f.scanner == scanner && f.resolved_ms.is_none()
                {
                    f.resolved_ms = Some(now_ms);
                    counts.resolved += 1;
                }
            }
        }
        counts
    }

    pub fn list(&self, args: &FindingsListArgs) -> Page<Finding> {
        let mut rows: Vec<Finding> = self
            .findings
            .iter()
            .filter(|f| args.include_resolved || f.resolved_ms.is_none())
            .filter(|f| args.unit_id.as_ref().is_none_or(|u| &f.unit_id == u))
            .filter(|f| args.scanner.as_deref().is_none_or(|s| f.scanner == s))
            .filter(|f| args.min_severity.is_none_or(|m| f.finding.severity >= m))
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            b.finding
                .severity
                .cmp(&a.finding.severity)
                .then_with(|| b.last_seen_ms.cmp(&a.last_seen_ms))
        });
        Page::from_slice(rows, &args.page)
    }

    pub fn detail(&self, args: &FindingsDetailArgs) -> Option<&Finding> {
        self.findings.iter().find(|f| {
            f.unit_id == args.unit_id
                && f.scanner == args.scanner
                && f.finding.id == args.id
                && f.finding.package == args.package
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

fn record(path: &Path, unit_id: &Id, scanner: &str, report: ScanReport) -> Result<MergeCounts> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = FindingsStore::load(path)?;
    let counts = store.merge(
        unit_id,
        scanner,
        report,
        utils::time::now_millis_since_epoch(),
    );
    store.save(path)?;
    Ok(counts)
}

pub trait SecurityScanner: Send + Sync {
    fn name(&self) -> &str;

    fn scan(&self, request: ScanRequest) -> BoxFuture<'_, Result<ScanReport>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn SecurityScanner>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub fn register_scanner(scanner: Arc<dyn SecurityScanner>) {
    let mut g = GLOBAL.write().expect("security registry poisoned");
    let name = scanner.name().to_string();
    if let Some(slot) = g.iter_mut().find(|s| s.name() == name) {
        *slot = scanner;
    } else {
        g.push(scanner);
    }
}

pub fn scanners() -> Vec<Arc<dyn SecurityScanner>> {
    GLOBAL.read().expect("security registry poisoned").clone()
}

pub fn deregister_scanner(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("security registry poisoned");
    let before = g.len();
    g.retain(|s| s.name() != name);
    before != g.len()
}

pub async fn scan(args: ScanArgs) -> Result<ScanSummary> {
    scan_into(&store_path()?, args).await
}

pub async fn scan_into(path: &Path, args: ScanArgs) -> Result<ScanSummary> {
    let selected: Vec<_> = scanners()
        .into_iter()
        .filter(|s| args.scanner.as_deref().is_none_or(|n| s.name() == n))
        .collect();
    if selected.is_empty() {
        match &args.scanner {
            Some(n) => bail!("no security scanner named '{n}'"),
            None => bail!("no security scanners registered"),
        }
    }
    let mut outcomes = Vec::with_capacity(selected.len());
    for s in selected {
        let name = s.name().to_string();
        let result = match s
            .scan(ScanRequest {
                unit_id: args.unit_id.clone(),
            })
            .await
        {
            Ok(report) => record(path, &args.unit_id, &name, report),
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
        unit_id: args.unit_id,
        scanners: outcomes,
    })
}

pub fn findings_list(args: &FindingsListArgs) -> Result<Page<Finding>> {
    Ok(FindingsStore::load(&store_path()?)?.list(args))
}

pub fn findings_detail(args: &FindingsDetailArgs) -> Result<Finding> {
    FindingsStore::load(&store_path()?)?
        .detail(args)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no finding {} from {} on unit {}",
                args.id,
                args.scanner,
                args.unit_id
            )
        })
}

#[cfg(feature = "in-process")]
pub type InvokeThunk = Arc<
    dyn Fn(
            &str,
            serde_json::Value,
            Option<crate::invocation_scope::BackendCall>,
        ) -> std::result::Result<serde_json::Value, serde_json::Value>
        + Send
        + Sync
        + 'static,
>;

pub const SCAN_OP: &str = "scan";

pub async fn dispatch_op(
    scanner: &dyn SecurityScanner,
    op: &str,
    args: serde_json::Value,
) -> std::result::Result<serde_json::Value, serde_json::Value> {
    fn err(msg: impl Into<String>) -> serde_json::Value {
        serde_json::Value::String(msg.into())
    }
    match op {
        SCAN_OP => {
            let req: ScanRequest =
                serde_json::from_value(args).map_err(|e| err(format!("decode {op} args: {e}")))?;
            let out = scanner.scan(req).await.map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        other => Err(err(format!("unknown security op: {other}"))),
    }
}

#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_scanner(Arc::new(ScannerProxy { name, invoke }));
    Ok(())
}

#[cfg(feature = "in-process")]
struct ScannerProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl SecurityScanner for ScannerProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn scan(&self, request: ScanRequest) -> BoxFuture<'_, Result<ScanReport>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let args = serde_json::to_value(&request)?;
            let call = crate::invocation_scope::capture("security", &name);
            let out = tokio::task::spawn_blocking(move || invoke(SCAN_OP, args, call))
                .await
                .map_err(|e| anyhow!("security '{name}' scan task panicked: {e}"))?
                .map_err(|e| {
                    anyhow!(
                        "security '{name}' scan failed: {}",
                        crate::render_invoke_error(&e)
                    )
                })?;
            serde_json::from_value(out)
                .map_err(|e| anyhow!("security '{name}' scan returned invalid JSON: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ScanReport { findings, complete }
    }

    #[test]
    fn merge_dedupes_and_updates_last_seen() {
        let mut s = FindingsStore::default();
        let c = s.merge(
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", Some("openssl"), Severity::High)], true),
            10,
        );
        assert_eq!(
            c,
            MergeCounts {
                new: 1,
                updated: 0,
                resolved: 0
            }
        );
        let mut again = sf("CVE-1", Some("openssl"), Severity::Critical);
        again.fixed_version = Some("1.1".into());
        let c = s.merge(&u(1), "trivy", report(vec![again], true), 20);
        assert_eq!(
            c,
            MergeCounts {
                new: 0,
                updated: 1,
                resolved: 0
            }
        );
        assert_eq!(s.findings().len(), 1);
        let f = &s.findings()[0];
        assert_eq!((f.first_seen_ms, f.last_seen_ms), (10, 20));
        assert_eq!(f.finding.severity, Severity::Critical);
        assert_eq!(f.finding.fixed_version.as_deref(), Some("1.1"));
    }

    #[test]
    fn same_id_differs_by_unit_scanner_and_package() {
        let mut s = FindingsStore::default();
        s.merge(
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", Some("a"), Severity::Low)], false),
            1,
        );
        s.merge(
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", Some("b"), Severity::Low)], false),
            1,
        );
        s.merge(
            &u(2),
            "trivy",
            report(vec![sf("CVE-1", Some("a"), Severity::Low)], false),
            1,
        );
        s.merge(
            &u(1),
            "other",
            report(vec![sf("CVE-1", Some("a"), Severity::Low)], false),
            1,
        );
        assert_eq!(s.findings().len(), 4);
    }

    #[test]
    fn complete_scan_resolves_absent_and_reopens_on_return() {
        let mut s = FindingsStore::default();
        s.merge(
            &u(1),
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
        s.merge(
            &u(2),
            "trivy",
            report(vec![sf("CVE-2", None, Severity::Low)], true),
            10,
        );
        s.merge(
            &u(1),
            "exposure",
            report(vec![sf("port-32400", None, Severity::High)], true),
            10,
        );

        let c = s.merge(
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", None, Severity::High)], false),
            20,
        );
        assert_eq!(c.resolved, 0);

        let c = s.merge(
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", None, Severity::High)], true),
            30,
        );
        assert_eq!(
            c,
            MergeCounts {
                new: 0,
                updated: 1,
                resolved: 1
            }
        );
        let open = s.list(&FindingsListArgs::default()).items;
        assert_eq!(open.len(), 3);
        assert!(
            !open
                .iter()
                .any(|f| f.unit_id == u(1) && f.finding.id == "CVE-2")
        );
        let all = s.list(&FindingsListArgs {
            include_resolved: true,
            ..Default::default()
        });
        let gone = all
            .items
            .iter()
            .find(|f| f.unit_id == u(1) && f.finding.id == "CVE-2")
            .unwrap();
        assert_eq!(gone.resolved_ms, Some(30));

        let c = s.merge(
            &u(1),
            "trivy",
            report(
                vec![
                    sf("CVE-1", None, Severity::High),
                    sf("CVE-2", None, Severity::Low),
                ],
                true,
            ),
            40,
        );
        assert_eq!(
            c,
            MergeCounts {
                new: 1,
                updated: 1,
                resolved: 0
            }
        );
        let back = s
            .detail(&FindingsDetailArgs {
                unit_id: u(1),
                scanner: "trivy".into(),
                id: "CVE-2".into(),
                package: None,
            })
            .unwrap();
        assert_eq!((back.first_seen_ms, back.resolved_ms), (10, None));
    }

    #[test]
    fn list_filters_and_orders_by_severity() {
        let mut s = FindingsStore::default();
        s.merge(
            &u(1),
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
        let page = s.list(&FindingsListArgs {
            min_severity: Some(Severity::Medium),
            ..Default::default()
        });
        let ids: Vec<_> = page.items.iter().map(|f| f.finding.id.as_str()).collect();
        assert_eq!(ids, ["crit", "med"]);
        assert!(
            s.list(&FindingsListArgs {
                unit_id: Some(u(9)),
                ..Default::default()
            })
            .items
            .is_empty()
        );
    }

    #[test]
    fn store_round_trips_through_file() {
        let path = std::env::temp_dir()
            .join(format!("orca-security-{}", utils::id::new()))
            .join("findings.json");
        assert!(FindingsStore::load(&path).unwrap().findings().is_empty());
        let mut s = FindingsStore::default();
        s.merge(
            &u(1),
            "trivy",
            report(vec![sf("CVE-1", Some("a"), Severity::High)], true),
            5,
        );
        s.save(&path).unwrap();
        assert_eq!(FindingsStore::load(&path).unwrap().findings(), s.findings());
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
    async fn scan_dispatches_to_registered_scanners_and_records() {
        register_scanner(Arc::new(FakeScanner {
            name: "sec-test-ok",
            fail: false,
        }));
        register_scanner(Arc::new(FakeScanner {
            name: "sec-test-bad",
            fail: true,
        }));
        let path = std::env::temp_dir()
            .join(format!("orca-security-{}", utils::id::new()))
            .join("findings.json");
        let unit = u(3);

        let one = scan_into(
            &path,
            ScanArgs {
                unit_id: unit.clone(),
                scanner: Some("sec-test-ok".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(one.scanners.len(), 1);
        assert!(one.scanners[0].ok);
        assert_eq!(one.scanners[0].counts.new, 1);

        let bad = scan_into(
            &path,
            ScanArgs {
                unit_id: unit.clone(),
                scanner: Some("sec-test-bad".into()),
            },
        )
        .await
        .unwrap();
        assert!(!bad.scanners[0].ok);
        assert!(
            bad.scanners[0]
                .error
                .as_deref()
                .unwrap()
                .contains("scanner down")
        );

        let stored = FindingsStore::load(&path).unwrap();
        assert_eq!(stored.findings().len(), 1);
        assert_eq!(stored.findings()[0].scanner, "sec-test-ok");

        assert!(
            scan_into(
                &path,
                ScanArgs {
                    unit_id: unit.clone(),
                    scanner: Some("nope".into()),
                },
            )
            .await
            .is_err()
        );
        assert!(deregister_scanner("sec-test-ok"));
        assert!(deregister_scanner("sec-test-bad"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn dispatch_op_round_trips_scan() {
        let s = FakeScanner {
            name: "sec-test-op",
            fail: false,
        };
        let out = dispatch_op(&s, SCAN_OP, serde_json::json!({ "unit_id": u(7) }))
            .await
            .unwrap();
        let r: ScanReport = serde_json::from_value(out).unwrap();
        assert_eq!(r.findings[0].id, format!("CVE-{}", u(7)));
        assert!(
            dispatch_op(&s, SCAN_OP, serde_json::json!({ "unit_id": "mimir" }))
                .await
                .is_err()
        );
        assert!(
            dispatch_op(&s, "nope", serde_json::json!({}))
                .await
                .is_err()
        );
    }
}
