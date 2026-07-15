//! External notification-source contract + registry.
//!
//! A `NotificationSource` lets a plugin feed notifications from an external
//! system (unraid, a NAS, a router, …) into orca's stateful notification plane
//! (`db::notifications_store`, driven by the `system` crate's ingestion
//! reconcile) and — where the source supports it — dismiss them back **at the
//! source** when the user dismisses them in orca.
//!
//! Shape mirrors [`crate::diagnostics`]: a provider registers into a
//! process-global registry, either in-process or, for a subprocess plugin, via
//! the [`register_from_def`] JSON proxy the plugin-loader installs for
//! `domain = "notification_source"`.
//!
//! Severity/fix here are contract-local types (this crate must not depend on
//! `db`); the ingestion reconcile in `system` maps them onto the store's own
//! `Severity`/`Fix`, the same way the diagnostics→notification bridge maps
//! `diagnostics::Severity`.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// Severity ladder for an ingested notification. Matches the store's ladder
/// (`info < warn < error < critical`); the `system` reconcile maps across.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
    Critical,
}

/// A remediation link carried by an ingested notification. All fields optional;
/// an all-`None` link means "no fix". Maps onto the store's `Fix`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FixLink {
    /// External page that documents or performs the fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Diagnostics provider that owns an in-orca repair (pairs with `repair_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// `RepairSpec` id to invoke via `diagnostics.repair`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_id: Option<String>,
    /// Canonical unit coordinate the fix acts on (pairs with `action`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Action verb to run against `unit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// One notification pulled from an external source by [`NotificationSource::poll`].
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Ingested {
    /// The source's own id for this notification — the handle passed back to
    /// [`NotificationSource::dismiss_at_source`]. Also the per-source dedup key.
    pub source_ref: String,
    pub severity: Severity,
    /// Whether the user can act on it (drives audience + surfaces the fix link).
    #[serde(default)]
    pub actionable: bool,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixLink>,
}

/// A source of external notifications — one per external system instance
/// (e.g. `unraid@<host>`). Registered into the process-global registry so the
/// ingestion reconcile can poll every source plugin-agnostically.
pub trait NotificationSource: Send + Sync {
    /// Registry name / notification `source` (e.g. `unraid@<host>`). Used to key
    /// stored rows to this source, to replace-in-place on re-register, and to
    /// route a dismiss back to the right source.
    fn name(&self) -> &str;

    /// Fetch the source's current active notifications. The reconcile raises
    /// each and auto-dismisses locally any previously-seen ref absent from this
    /// result. A poll error leaves this source's rows untouched (no false
    /// clears).
    fn poll(&self) -> BoxFuture<'_, Result<Vec<Ingested>>>;

    /// Dismiss / acknowledge a notification back at the source. Called when the
    /// user dismisses an orca notification that carries a `source_ref` for this
    /// source. Default: a no-op success for sources that cannot dismiss remotely.
    fn dismiss_at_source(&self, _source_ref: &str) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Whether this source can dismiss at the source. When `false`, orca dismisses
    /// locally only and never calls [`dismiss_at_source`](Self::dismiss_at_source).
    fn supports_dismiss_at_source(&self) -> bool {
        true
    }
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn NotificationSource>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a notification source. Re-registering the same `name()` replaces the
/// existing entry so a dev rebuild / plugin reload doesn't duplicate it.
pub fn register_source(source: Arc<dyn NotificationSource>) {
    let mut g = GLOBAL
        .write()
        .expect("notification_source registry poisoned");
    let name = source.name().to_string();
    if let Some(slot) = g.iter_mut().find(|s| s.name() == name) {
        *slot = source;
    } else {
        g.push(source);
    }
}

/// Snapshot of every registered source.
pub fn sources() -> Vec<Arc<dyn NotificationSource>> {
    GLOBAL
        .read()
        .expect("notification_source registry poisoned")
        .clone()
}

/// Look up one source by name.
pub fn source(name: &str) -> Option<Arc<dyn NotificationSource>> {
    sources().into_iter().find(|s| s.name() == name)
}

/// Deregister the source named `name`, if present. Returns `true` if removed.
pub fn deregister_source(name: &str) -> bool {
    let mut g = GLOBAL
        .write()
        .expect("notification_source registry poisoned");
    let before = g.len();
    g.retain(|s| s.name() != name);
    before != g.len()
}

// ── Host-side loaded-plugin proxy ─────────────────────────────────────────────

/// The synchronous invoke thunk a loaded plugin's source is driven through:
/// `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn` of strings
/// so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation names the [`SourceProxy`] invokes across the FFI boundary. The
/// plugin exposes tools `"{invoke_prefix}.{POLL_OP|DISMISS_OP}"`.
pub const POLL_OP: &str = "poll";
pub const DISMISS_OP: &str = "dismiss_at_source";

/// Build and register a [`NotificationSource`] from a plugin backend name plus an
/// [`InvokeThunk`]. The plugin-loader calls this from its domain dispatch table
/// for `domain = "notification_source"`.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_source(Arc::new(SourceProxy { name, invoke }));
    Ok(())
}

/// A [`NotificationSource`] backed by a subprocess plugin reached over the
/// JSON-proxy FFI boundary. Each op offloads the synchronous [`InvokeThunk`] onto
/// `spawn_blocking` and (de)serializes JSON at the seam.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct SourceProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl NotificationSource for SourceProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn poll(&self) -> BoxFuture<'_, Result<Vec<Ingested>>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(POLL_OP, "{}".to_string()))
                .await
                .map_err(|e| {
                    anyhow::anyhow!("notification_source '{name}' poll task panicked: {e}")
                })?
                .map_err(|e| anyhow::anyhow!("notification_source '{name}' poll failed: {e}"))?;
            serde_json::from_str(&out).map_err(|e| {
                anyhow::anyhow!("notification_source '{name}' poll returned invalid JSON: {e}")
            })
        })
    }

    fn dismiss_at_source(&self, source_ref: &str) -> BoxFuture<'_, Result<()>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let args = serde_json::json!({ "sourceRef": source_ref }).to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || invoke(DISMISS_OP, args))
                .await
                .map_err(|e| {
                    anyhow::anyhow!("notification_source '{name}' dismiss task panicked: {e}")
                })?
                .map_err(|e| {
                    anyhow::anyhow!("notification_source '{name}' dismiss_at_source failed: {e}")
                })?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingested_round_trips_camel_case_and_skips_absent_optionals() {
        let ing = Ingested {
            source_ref: "42".into(),
            severity: Severity::Error,
            actionable: true,
            title: "array degraded".into(),
            body: None,
            fix: None,
        };
        let v = serde_json::to_value(&ing).unwrap();
        assert_eq!(v["sourceRef"], "42");
        assert_eq!(v["severity"], "error");
        assert_eq!(v["actionable"], true);
        assert!(v.get("body").is_none());
        assert!(v.get("fix").is_none());
        let back: Ingested = serde_json::from_value(v).unwrap();
        assert_eq!(back, ing);
    }

    #[test]
    fn registry_replaces_in_place_and_deregisters() {
        struct S(&'static str);
        impl NotificationSource for S {
            fn name(&self) -> &str {
                self.0
            }
            fn poll(&self) -> BoxFuture<'_, Result<Vec<Ingested>>> {
                Box::pin(async { Ok(vec![]) })
            }
        }
        register_source(Arc::new(S("src-a@test")));
        register_source(Arc::new(S("src-a@test"))); // same name → replace
        assert_eq!(
            sources()
                .iter()
                .filter(|s| s.name() == "src-a@test")
                .count(),
            1,
            "re-register must replace in place, not duplicate"
        );
        assert!(source("src-a@test").is_some());
        assert!(deregister_source("src-a@test"));
        assert!(source("src-a@test").is_none());
    }

    #[test]
    fn default_supports_dismiss_at_source_is_true() {
        struct S;
        impl NotificationSource for S {
            fn name(&self) -> &str {
                "noop"
            }
            fn poll(&self) -> BoxFuture<'_, Result<Vec<Ingested>>> {
                Box::pin(async { Ok(vec![]) })
            }
        }
        assert!(S.supports_dismiss_at_source());
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn default_dismiss_at_source_is_ok_noop() {
        struct S;
        impl NotificationSource for S {
            fn name(&self) -> &str {
                "noop"
            }
            fn poll(&self) -> BoxFuture<'_, Result<Vec<Ingested>>> {
                Box::pin(async { Ok(vec![]) })
            }
        }
        S.dismiss_at_source("x").await.unwrap();
    }
}
