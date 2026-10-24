#![allow(clippy::disallowed_types)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
#[cfg(feature = "in-process")]
use anyhow::anyhow;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utils::id::Id;

use crate::BoxFuture;

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

impl Severity {
    pub const ALL: [Severity; 5] = [
        Severity::Unknown,
        Severity::Low,
        Severity::Medium,
        Severity::High,
        Severity::Critical,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Unknown => "unknown",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Severity::ALL
            .into_iter()
            .find(|v| v.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown severity `{s}`"))
    }
}

/// One finding as a scanner reports it for a unit. `id` is a CVE or check id;
/// `package`/`file` name what it was found in.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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

/// What a scanner returns for one unit.
///
/// Resolution contract — which previously-open findings from this scanner on
/// this unit are marked resolved when absent from `findings`:
/// - `coverage: Some(targets)`: only findings whose `package` or `file` is in
///   `targets`. Findings outside the declared coverage are left open.
/// - `coverage: None` and `complete: true`: every open finding for
///   (unit, scanner).
/// - `coverage: None` and `complete: false`: none; the report only adds and
///   refreshes.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    pub findings: Vec<ScannedFinding>,
    #[serde(default)]
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ScanRequest {
    pub unit_id: Id,
}

pub trait SecurityScanner: Send + Sync {
    fn name(&self) -> &str;

    fn scan(&self, request: ScanRequest) -> BoxFuture<'_, Result<ScanReport>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn SecurityScanner>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Re-registering the same `name()` replaces the existing entry (plugin
/// reload). The plugin loader binds a `security` backend name to its plugin
/// id, so a different plugin cannot take over an existing name through it.
pub fn register_scanner(scanner: Arc<dyn SecurityScanner>) {
    let mut g = GLOBAL.write().expect("security registry poisoned");
    let name = scanner.name().to_string();
    if let Some(slot) = g.iter_mut().find(|s| s.name() == name) {
        tracing::warn!(scanner = %name, "security scanner re-registered; replacing");
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

    struct Fake;

    impl SecurityScanner for Fake {
        fn name(&self) -> &str {
            "sec-contract-fake"
        }
        fn scan(&self, request: ScanRequest) -> BoxFuture<'_, Result<ScanReport>> {
            Box::pin(async move {
                Ok(ScanReport {
                    findings: vec![ScannedFinding {
                        id: format!("CVE-{}", request.unit_id),
                        severity: Severity::High,
                        title: None,
                        package: None,
                        file: None,
                        installed_version: None,
                        fixed_version: None,
                    }],
                    complete: true,
                    coverage: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn dispatch_op_round_trips_scan_and_rejects_bad_ids() {
        let unit = utils::id::new();
        let out = dispatch_op(&Fake, SCAN_OP, serde_json::json!({ "unitId": unit }))
            .await
            .unwrap();
        let r: ScanReport = serde_json::from_value(out).unwrap();
        assert_eq!(r.findings[0].id, format!("CVE-{unit}"));
        assert!(
            dispatch_op(&Fake, SCAN_OP, serde_json::json!({ "unitId": "mimir" }))
                .await
                .is_err()
        );
        assert!(
            dispatch_op(&Fake, "nope", serde_json::json!({}))
                .await
                .is_err()
        );
    }

    #[test]
    fn registry_replaces_by_name_and_deregisters() {
        register_scanner(Arc::new(Fake));
        register_scanner(Arc::new(Fake));
        assert_eq!(
            scanners()
                .iter()
                .filter(|s| s.name() == "sec-contract-fake")
                .count(),
            1
        );
        assert!(deregister_scanner("sec-contract-fake"));
        assert!(!deregister_scanner("sec-contract-fake"));
    }

    #[test]
    fn severity_parses_its_own_names() {
        for s in Severity::ALL {
            assert_eq!(s.as_str().parse::<Severity>().unwrap(), s);
        }
        assert!("nope".parse::<Severity>().is_err());
    }
}
