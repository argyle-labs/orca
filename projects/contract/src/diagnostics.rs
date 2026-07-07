//! Cross-crate diagnostics/repair types and provider registry.
//!
//! A diagnostics provider (raccoon for gaming, later bazzite/cachyos/beaver for
//! OS state) inspects a subsystem and emits typed [`Finding`]s, each optionally
//! carrying a [`RepairSpec`] describing how to remediate. The surface layer
//! (`dispatch::diagnostics_surface`) fans `diagnose` across every provider and
//! routes `repair` to the owning one, so the surface stays plugin-agnostic — the
//! same shape the `topology` and `storage` domains use.
//!
//! ## Provider registry
//!
//! A provider registers into a process-global registry — either in-process or,
//! for an external cdylib plugin, via the [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "diagnostics"`.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// How serious a [`Finding`] is. Ordered least→most severe.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Healthy — the checked state matches known-good.
    Ok,
    /// Informational nudge; nothing wrong, an optional improvement.
    Info,
    /// Degraded — works but should be fixed (the common actionable case).
    Warn,
    /// Broken — needs attention.
    Crit,
}

/// How to remediate a [`Finding`]. The `id` is passed back to `repair` to run it.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct RepairSpec {
    /// Stable id for this repair, unique within its provider. Passed to
    /// `repair` to execute it (e.g. `"alsa-headroom"`, `"cpu-mode"`).
    pub id: String,
    /// Human-readable description of what running the repair does.
    pub description: String,
    /// Safe to run automatically (no privilege, no destructive side effects).
    /// `false` = surface it but require an explicit user action.
    pub automatic: bool,
    /// Needs elevated privilege (sudo/root) to apply.
    pub privileged: bool,
}

/// One diagnosed condition. Providers return a `Vec<Finding>` from `diagnose`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct Finding {
    /// Stable check id, unique within its provider (e.g. `"scx"`, `"cpu-mode"`).
    pub id: String,
    /// Provider that emitted this finding (registry key, e.g. `"raccoon"`).
    pub provider: String,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    /// How to fix it, when fixable. `None` for `Ok`/`Info` or unfixable states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairSpec>,
}

/// Result of running a repair.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct RepairOutcome {
    /// The `RepairSpec.id` that was run.
    pub id: String,
    /// Provider that ran it.
    pub provider: String,
    /// Whether the repair applied successfully.
    pub ok: bool,
    /// Human-readable result (what was done, or why it couldn't be).
    pub message: String,
}

/// Filter for a `diagnose` fan-out. Empty = diagnose everything.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct DiagnoseArgs {
    /// Restrict to a single provider by registry name. `None` = all providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Arguments to run one repair, routed to the provider that owns `repair_id`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct RepairArgs {
    /// Provider registry name that owns the repair.
    pub provider: String,
    /// The `RepairSpec.id` to run.
    pub repair_id: String,
}

// ── Provider registry ───────────────────────────────────────────────────────

/// A diagnostics/repair source — one per provider (raccoon, bazzite, …).
/// Registered into the process-global registry so the surface layer can fan out
/// across providers plugin-agnostically.
pub trait DiagnosticsProvider: Send + Sync {
    /// Provider/registry name (e.g. `"raccoon"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    /// Inspect this provider's subsystem and return typed findings.
    fn diagnose(&self, args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>>;

    /// Run one repair by its `RepairSpec.id`.
    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn DiagnosticsProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a diagnostics provider. Re-registering the same `name()` replaces
/// the existing entry so a dev rebuild / plugin reload doesn't duplicate it.
pub fn register_provider(provider: Arc<dyn DiagnosticsProvider>) {
    let mut g = GLOBAL.write().expect("diagnostics registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn DiagnosticsProvider>> {
    GLOBAL
        .read()
        .expect("diagnostics registry poisoned")
        .clone()
}

/// Deregister the provider named `name`, if present. Returns `true` if removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("diagnostics registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

// ── Surface entry points (used by dispatch::diagnostics_surface) ─────────────

/// Fan `diagnose` across all registered providers (or one, if `args.provider`
/// is set), concatenating their findings. A provider that errors is skipped —
/// one broken provider must not blank the whole report.
pub async fn diagnose(args: DiagnoseArgs) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in providers() {
        if let Some(want) = &args.provider
            && p.name() != want
        {
            continue;
        }
        match p.diagnose(args.clone()).await {
            Ok(mut findings) => out.append(&mut findings),
            Err(e) => out.push(Finding {
                id: "provider-error".to_string(),
                provider: p.name().to_string(),
                severity: Severity::Crit,
                title: format!("Provider '{}' failed to diagnose", p.name()),
                detail: e.to_string(),
                repair: None,
            }),
        }
    }
    out
}

/// Route one repair to the provider that owns it.
pub async fn repair(args: RepairArgs) -> Result<RepairOutcome> {
    let provider = providers()
        .into_iter()
        .find(|p| p.name() == args.provider)
        .ok_or_else(|| anyhow::anyhow!("no diagnostics provider named '{}'", args.provider))?;
    provider.repair(args).await
}

// ── cdylib FFI proxy ─────────────────────────────────────────────────────────

/// The synchronous invoke thunk a cdylib plugin's provider is driven through:
/// `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn` of
/// strings so `contract` stays free of any ABI/loader dependency (no cycle).
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation names the [`DiagnosticsProxy`] invokes across the FFI boundary. The
/// plugin exposes tools `"{invoke_prefix}.{DIAGNOSE_OP|REPAIR_OP}"`.
pub const DIAGNOSE_OP: &str = "diagnose";
pub const REPAIR_OP: &str = "repair";

/// Build and register a [`DiagnosticsProvider`] from a plugin backend descriptor
/// plus an [`InvokeThunk`]. The plugin-loader calls this from its domain
/// dispatch table for `domain = "diagnostics"`.
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(DiagnosticsProxy { name, invoke }));
    Ok(())
}

/// A [`DiagnosticsProvider`] backed by a cdylib plugin reached over the
/// JSON-proxy FFI boundary. Each op offloads the synchronous [`InvokeThunk`]
/// onto `spawn_blocking` and (de)serializes JSON at the seam.
struct DiagnosticsProxy {
    name: String,
    invoke: InvokeThunk,
}

impl DiagnosticsProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args_json: String,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
                .await
                .map_err(|e| anyhow::anyhow!("diagnostics '{name}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("diagnostics '{name}' {op} failed: {e}"))?;
            serde_json::from_str(&out).map_err(|e| {
                anyhow::anyhow!("diagnostics '{name}' {op} returned invalid JSON: {e}")
            })
        })
    }
}

impl DiagnosticsProvider for DiagnosticsProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn diagnose(&self, args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        self.call(DIAGNOSE_OP, args_json)
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        self.call(REPAIR_OP, args_json)
    }
}
