//! Generic service tool surface.
//!
//! orca does not mint a tool namespace per service. These few verbs take the
//! service *name* as a parameter and iterate the process-global `service`
//! registry ([`plugin_toolkit::service`]) that each backend plugin registers
//! itself against at load:
//!
//! * `service.list`      — every registered service backend + its capabilities
//! * `service.deploy`    — build the backend's `WorkloadSpec` and place it on a
//!   matching deploy target (composition, not duplication)
//! * `service.backup`    — snapshot a service instance's config/data
//! * `service.restore`   — restore from a backup artifact
//! * `service.configure` — apply service-specific config
//! * `service.status`    — health/diagnostics
//!
//! `service.deploy` is the composition seam: a service describes *what* to run
//! (its `WorkloadSpec`); `deploy_target` owns *where/how* to run it. The service
//! domain never drives `pct`/`docker` itself.
//!
//! Dispatched through the single daemon handler so CLI / REST / MCP / UI share
//! one path ([[feedback-cli-api-mcp-one-path]]).

use derive::orca_tool;
use plugin_toolkit::deploy_target::{self, DeployCapability, DeployOutcome};
use plugin_toolkit::service::{
    self, BackupArtifact, Endpoint, ServiceProvider, ServiceStatus, parse_runtime,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── list ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceListArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ServiceListOutput {
    pub providers: Vec<ServiceProvider>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total providers across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Every service backend registered with this daemon, with the runtimes and
/// lifecycle capabilities each advertises. Empty before any service plugin loads.
#[orca_tool(domain = "service", verb = "list")]
async fn service_list(
    args: ServiceListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ServiceListOutput> {
    let mut providers = service::providers();
    providers.sort_by(|a, b| a.name.cmp(&b.name));
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(providers, &params);
    Ok(ServiceListOutput {
        providers: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

// ── shared endpoint args ─────────────────────────────────────────────
// The instance an op targets. Carried inline for now; `service.connect` will
// persist these (reusing the replicated endpoint registry) in a follow-up so
// the creds need not be repeated per call.

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointArgs {
    /// Service provider name, e.g. `audiobookshelf`.
    #[arg(long)]
    pub service: String,
    /// Instance name, unique within the provider.
    #[arg(long)]
    pub instance: String,
    /// Base URL the instance is reached at.
    #[arg(long, default_value = "")]
    pub base_url: String,
    /// Deploy-target host the instance runs on.
    #[arg(long, default_value = "")]
    pub host: String,
    /// Runtime the instance runs as (`docker`/`podman`/`lxc`/`vm`). Drives the
    /// backup path; absent = the backend's first declared runtime.
    #[arg(long)]
    pub runtime: Option<String>,
    /// Backup method override (`tar`/`pbs`/…). Absent = auto-select (a Proxmox
    /// LXC/VM with PBS available routes to `pbs`, else `tar`).
    #[arg(long)]
    pub method: Option<String>,
    /// API token / credential.
    #[arg(long, default_value = "")]
    pub token: String,
}

impl EndpointArgs {
    fn endpoint(&self) -> Endpoint {
        Endpoint {
            name: self.instance.clone(),
            base_url: self.base_url.clone(),
            target_host: self.host.clone(),
            runtime: self.runtime.as_deref().and_then(|s| parse_runtime(s).ok()),
            backup_method: self.method.clone(),
            token: self.token.clone(),
        }
    }
}

fn backend_for(name: &str) -> anyhow::Result<std::sync::Arc<dyn service::ServiceBackend>> {
    service::backend(name).ok_or_else(|| anyhow::anyhow!("no service backend named `{name}`"))
}

// ── create{action=deploy|backup} ─────────────────────────────────────

/// The `service.create` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum ServiceCreateAction {
    /// Build the backend's `WorkloadSpec` and place it on a deploy target.
    Deploy,
    /// Snapshot a service instance's config/data into a restorable artifact.
    Backup,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceCreateArgs {
    /// Which create action to run: `deploy` or `backup`.
    #[arg(long, value_enum)]
    pub action: Option<ServiceCreateAction>,
    #[command(flatten)]
    pub endpoint: EndpointArgs,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupOutput {
    pub artifact: BackupArtifact,
}

/// Untagged so each variant serializes as its bare payload.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ServiceCreateOutput {
    Deploy(DeployOutcome),
    Backup(BackupOutput),
}

/// Create a service artifact. `action=deploy` builds the backend's
/// `WorkloadSpec` and places it on a matching deploy target — the service
/// backend describes *what* to run; the deploy target runs it (composition, not
/// duplication). `action=backup` snapshots a service instance's config/data,
/// delegating to the backend's own backup engine (no duplicate logic here).
#[orca_tool(domain = "service", verb = "create")]
async fn service_create(
    args: ServiceCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ServiceCreateOutput> {
    let action = args.action.ok_or_else(|| {
        anyhow::anyhow!("`action` is required for service.create (deploy|backup)")
    })?;
    let backend = backend_for(&args.endpoint.service)?;
    match action {
        ServiceCreateAction::Deploy => {
            let ep = &args.endpoint;
            let runtime_str = ep
                .runtime
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--runtime is required for action=deploy"))?;
            let runtime = parse_runtime(&runtime_str)?;
            let spec = backend.workload_spec(runtime, &ep.endpoint()).await?;

            // Resolve a deploy target on this host + runtime that can launch.
            let target = deploy_target::targets()
                .into_iter()
                .find(|t| {
                    t.host() == ep.host
                        && t.runtime() == runtime
                        && t.supports(DeployCapability::Launch)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no deploy target on host `{}` with runtime `{}` that can launch",
                        ep.host,
                        runtime_str
                    )
                })?;
            Ok(ServiceCreateOutput::Deploy(target.launch(&spec).await?))
        }
        ServiceCreateAction::Backup => Ok(ServiceCreateOutput::Backup(BackupOutput {
            artifact: backend.backup(&args.endpoint.endpoint()).await?,
        })),
    }
}

// ── update{action=configure|restore} ─────────────────────────────────

/// The `service.update` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum ServiceUpdateAction {
    /// Apply service-specific configuration to an instance idempotently.
    Configure,
    /// Restore a service instance from a backup artifact path.
    Restore,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceUpdateArgs {
    /// Which update action to run: `configure` or `restore`.
    #[arg(long, value_enum)]
    pub action: Option<ServiceUpdateAction>,
    #[command(flatten)]
    pub endpoint: EndpointArgs,
    /// `configure`: service-specific configuration payload (JSON the backend
    /// interprets). Defaults to `{}`.
    #[arg(long, default_value = "{}")]
    #[serde(default)]
    pub config: String,
    /// `restore`: path of the backup artifact to restore from.
    #[arg(long)]
    pub from: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OkOutput {
    pub ok: bool,
}

/// Update a running service instance. `action=configure` applies a
/// service-specific config payload idempotently; `action=restore` restores the
/// instance from a backup artifact path (`--from`).
#[orca_tool(domain = "service", verb = "update")]
async fn service_update(
    args: ServiceUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<OkOutput> {
    let action = args.action.ok_or_else(|| {
        anyhow::anyhow!("`action` is required for service.update (configure|restore)")
    })?;
    let backend = backend_for(&args.endpoint.service)?;
    match action {
        ServiceUpdateAction::Configure => {
            backend
                .configure(&args.endpoint.endpoint(), &args.config)
                .await?;
        }
        ServiceUpdateAction::Restore => {
            let from = args
                .from
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`from` is required for action=restore"))?;
            let artifact = BackupArtifact {
                service: args.endpoint.service.clone(),
                instance: args.endpoint.instance.clone(),
                path: from.to_string(),
                ..Default::default()
            };
            backend
                .restore(&args.endpoint.endpoint(), &artifact)
                .await?;
        }
    }
    Ok(OkOutput { ok: true })
}

// ── detail{view=status} ──────────────────────────────────────────────

/// Which facet `service.detail` reports. `status` (health/diagnostics) is the
/// first; more views fold in here as the enum grows.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum ServiceDetailView {
    #[default]
    Status,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceDetailArgs {
    /// Which facet to report. Defaults to `status`.
    #[arg(long, value_enum, default_value = "status")]
    #[serde(default)]
    pub view: ServiceDetailView,
    #[command(flatten)]
    pub endpoint: EndpointArgs,
}

/// Detail for a service instance. `view=status` returns health/diagnostics.
#[orca_tool(domain = "service", verb = "detail")]
async fn service_detail(
    args: ServiceDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ServiceStatus> {
    let ServiceDetailView::Status = args.view;
    let backend = backend_for(&args.endpoint.service)?;
    Ok(backend.status(&args.endpoint.endpoint()).await?)
}

// ── health (fleet-wide aggregate) ────────────────────────────────────
// An explicit, on-demand fan-out: probe every registered backend once and
// project each into the generic `contract::health::Health` enum. This is NOT a
// cached poll and is deliberately kept OFF the hot read paths — `service.list`,
// `containers.list`, and `pod.list` must not trigger a live fan-out (the
// ≤500ms read-budget / no-live-fan-out-on-read-paths rule). Callers ask for the
// fleet health picture only when they want it, through this dedicated verb.

const HEALTH_PROBE_DEFAULT_MS: u64 = 1000;

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceHealthArgs {
    /// Probe only this provider. Omit for the fleet-wide aggregate across every
    /// registered backend.
    #[arg(long)]
    pub service: Option<String>,
    /// Instance name for a single-provider probe.
    #[arg(long, default_value = "")]
    pub instance: String,
    /// Base URL the instance is reached at, for a single-provider probe.
    #[arg(long, default_value = "")]
    pub base_url: String,
    /// Deploy-target host the instance runs on, for a single-provider probe.
    #[arg(long, default_value = "")]
    pub host: String,
    /// API token / credential for a single-provider probe.
    #[arg(long, default_value = "")]
    pub token: String,
    /// Per-backend probe timeout in milliseconds (clamped to [100, 5000];
    /// default 1000). Bounds the aggregate so one slow backend can't stall it.
    #[arg(long)]
    pub timeout_ms: Option<u64>,
}

/// One backend's projected health in the fleet aggregate.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ServiceHealthRow {
    pub provider: String,
    pub health: contract::health::Health,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ServiceHealthOutput {
    /// One row per probed backend, sorted by provider name.
    #[serde(default)]
    pub services: Vec<ServiceHealthRow>,
    /// Per-backend probe failures (timeouts, transport errors). Recorded, not
    /// fatal — a failing backend still appears in `services` as `unknown`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Probe one backend's health under a hard per-backend timeout. A slow, hung, or
/// erroring backend maps to [`Health::Unknown`](contract::health::Health::Unknown)
/// and its failure is returned to be recorded — never fatal — so one bad backend
/// can't stall the fleet aggregate. `healthy` → `Healthy`, `!healthy` →
/// `Unhealthy`; richer per-backend `Health` is a follow-up.
async fn probe(
    backend: std::sync::Arc<dyn service::ServiceBackend>,
    ep: Endpoint,
    timeout: std::time::Duration,
) -> (contract::health::Health, Option<String>, Option<String>) {
    use contract::health::Health;
    match tokio::time::timeout(timeout, backend.status(&ep)).await {
        Ok(Ok(status)) => {
            let health = if status.healthy {
                Health::Healthy
            } else {
                Health::Unhealthy
            };
            let detail = (!status.detail.is_empty()).then_some(status.detail);
            (health, detail, None)
        }
        Ok(Err(e)) => (Health::Unknown, None, Some(e.to_string())),
        Err(_) => (
            Health::Unknown,
            None,
            Some(format!(
                "status probe timed out after {}ms",
                timeout.as_millis()
            )),
        ),
    }
}

/// Fleet-wide service health. With no `--service`, probes every registered
/// backend concurrently (each under a short timeout) and returns a typed row per
/// provider projected into the generic `contract::health::Health` enum. With
/// `--service`, probes just that named provider (preserving the per-instance
/// path). This is an explicit on-demand aggregate — not a cached poll, and never
/// wired into `service.list`/`containers.list`/`pod.list` hot reads.
#[orca_tool(domain = "service", verb = "health")]
async fn service_health(
    args: ServiceHealthArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ServiceHealthOutput> {
    let timeout = std::time::Duration::from_millis(
        args.timeout_ms
            .unwrap_or(HEALTH_PROBE_DEFAULT_MS)
            .clamp(100, 5000),
    );

    // Single named provider: probe just that backend, with the caller's endpoint.
    if let Some(name) = args.service.as_deref().filter(|s| !s.is_empty()) {
        let backend = backend_for(name)?;
        let ep = Endpoint {
            name: args.instance.clone(),
            base_url: args.base_url.clone(),
            target_host: args.host.clone(),
            token: args.token.clone(),
            ..Default::default()
        };
        let (health, detail, error) = probe(backend, ep, timeout).await;
        let mut errors = Vec::new();
        if let Some(e) = error {
            errors.push(format!("{name}: {e}"));
        }
        return Ok(ServiceHealthOutput {
            services: vec![ServiceHealthRow {
                provider: name.to_string(),
                health,
                detail,
            }],
            errors,
        });
    }

    // Fleet-wide: probe every backend concurrently, each bounded by `timeout`.
    let handles: Vec<_> = service::backends()
        .into_iter()
        .map(|backend| {
            let provider = backend.provider().to_string();
            tokio::spawn(async move {
                let (health, detail, error) = probe(backend, Endpoint::default(), timeout).await;
                (provider, health, detail, error)
            })
        })
        .collect();

    let mut services = Vec::new();
    let mut errors = Vec::new();
    for handle in handles {
        match handle.await {
            Ok((provider, health, detail, error)) => {
                if let Some(e) = error {
                    errors.push(format!("{provider}: {e}"));
                }
                services.push(ServiceHealthRow {
                    provider,
                    health,
                    detail,
                });
            }
            Err(join_err) => errors.push(format!("probe task failed to join: {join_err}")),
        }
    }
    services.sort_by(|a, b| a.provider.cmp(&b.provider));

    Ok(ServiceHealthOutput { services, errors })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::deploy_target::Runtime;

    fn sample_args() -> EndpointArgs {
        EndpointArgs {
            service: "audiobookshelf".into(),
            instance: "main".into(),
            base_url: "http://host:13378".into(),
            host: "node-a".into(),
            runtime: Some("docker".into()),
            method: Some("tar".into()),
            token: "secret".into(),
        }
    }

    #[test]
    fn endpoint_maps_fields_and_parses_runtime() {
        let ep = sample_args().endpoint();
        assert_eq!(ep.name, "main");
        assert_eq!(ep.base_url, "http://host:13378");
        assert_eq!(ep.target_host, "node-a");
        assert_eq!(ep.runtime, Some(Runtime::Docker));
        assert_eq!(ep.backup_method.as_deref(), Some("tar"));
        assert_eq!(ep.token, "secret");
    }

    #[test]
    fn endpoint_runtime_none_when_absent() {
        let mut args = sample_args();
        args.runtime = None;
        assert!(args.endpoint().runtime.is_none());
    }

    #[test]
    fn endpoint_runtime_none_when_unparseable() {
        // An unknown runtime string is silently dropped to None by `endpoint()`.
        let mut args = sample_args();
        args.runtime = Some("bogus".into());
        assert!(args.endpoint().runtime.is_none());
    }

    #[test]
    fn endpoint_runtime_variants_parse() {
        for (s, want) in [
            ("docker", Runtime::Docker),
            ("podman", Runtime::Podman),
            ("lxc", Runtime::Lxc),
            ("vm", Runtime::Vm),
        ] {
            let mut args = sample_args();
            args.runtime = Some(s.into());
            assert_eq!(args.endpoint().runtime, Some(want), "runtime {s}");
        }
    }

    #[test]
    fn backend_for_unknown_errors() {
        // No service plugin is loaded in a unit-test process, so any lookup
        // fails with a descriptive error.
        let err = match backend_for("does-not-exist") {
            Ok(_) => panic!("expected error for unknown backend"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("does-not-exist"), "got: {err}");
    }

    #[test]
    fn endpoint_args_deserialize_defaults() {
        // Only the required fields; the rest fall back via serde `default`.
        let args: EndpointArgs =
            serde_json::from_str(r#"{"service":"svc","instance":"i"}"#).unwrap();
        assert_eq!(args.service, "svc");
        assert_eq!(args.instance, "i");
        assert_eq!(args.base_url, "");
        assert_eq!(args.host, "");
        assert!(args.runtime.is_none());
        assert!(args.method.is_none());
        assert_eq!(args.token, "");
    }

    #[test]
    fn list_output_serializes_camel_case() {
        let out = ServiceListOutput {
            providers: vec![],
            next_cursor: None,
            total: None,
        };
        let v = serde_json::to_value(&out).unwrap();
        assert!(v["providers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn ok_output_serializes() {
        let v = serde_json::to_value(OkOutput { ok: true }).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn update_args_nest_endpoint_and_from() {
        let args: ServiceUpdateArgs = serde_json::from_str(
            r#"{"action":"restore","endpoint":{"service":"svc","instance":"i"},"from":"/tmp/backup.tar"}"#,
        )
        .unwrap();
        assert_eq!(args.action, Some(ServiceUpdateAction::Restore));
        assert_eq!(args.endpoint.service, "svc");
        assert_eq!(args.endpoint.instance, "i");
        assert_eq!(args.from.as_deref(), Some("/tmp/backup.tar"));
    }

    #[test]
    fn update_args_default_config_is_empty() {
        let args: ServiceUpdateArgs =
            serde_json::from_str(r#"{"endpoint":{"service":"svc","instance":"i"}}"#).unwrap();
        assert_eq!(args.config, "");
        assert_eq!(args.endpoint.service, "svc");
    }

    #[test]
    fn create_args_default_action_is_none() {
        let args: ServiceCreateArgs =
            serde_json::from_str(r#"{"endpoint":{"service":"svc","instance":"i"}}"#).unwrap();
        assert!(args.action.is_none());
        assert_eq!(args.endpoint.service, "svc");
    }

    #[test]
    fn health_args_default_to_fleet_wide() {
        // No `service` field → fleet-wide aggregate; endpoint fields default empty.
        let args: ServiceHealthArgs = serde_json::from_str(r#"{}"#).unwrap();
        assert!(args.service.is_none());
        assert_eq!(args.base_url, "");
        assert!(args.timeout_ms.is_none());
    }

    #[test]
    fn health_output_serializes_and_hides_empty_errors() {
        let out = ServiceHealthOutput {
            services: vec![ServiceHealthRow {
                provider: "abs".into(),
                health: contract::health::Health::Healthy,
                detail: None,
            }],
            errors: vec![],
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["services"][0]["provider"], "abs");
        assert_eq!(v["services"][0]["health"], "healthy");
        assert!(v.get("errors").is_none(), "empty errors must be skipped");
        assert!(
            v["services"][0].get("detail").is_none(),
            "absent detail must be skipped"
        );
    }

    #[test]
    fn health_output_round_trips_empty_services_and_errors() {
        // Regression guard: the healthy, no-peer case serializes without an
        // `errors` (and possibly `services`) field, and must deserialize back.
        let out = ServiceHealthOutput {
            services: vec![],
            errors: vec![],
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: ServiceHealthOutput =
            serde_json::from_str(&json).expect("empty output must round-trip");
        assert!(back.services.is_empty());
        assert!(back.errors.is_empty());
        // Also tolerate a payload that omits `services` entirely.
        let back: ServiceHealthOutput = serde_json::from_str("{}").unwrap();
        assert!(back.services.is_empty());
        assert!(back.errors.is_empty());
    }

    // A backend whose `status()` behavior is fully scripted for probe tests.
    struct FakeBackend {
        name: &'static str,
        outcome: Outcome,
    }
    enum Outcome {
        Healthy,
        Unhealthy,
        Error,
        Hang,
    }
    impl service::ServiceBackend for FakeBackend {
        fn provider(&self) -> &str {
            self.name
        }
        fn runtimes(&self) -> Vec<Runtime> {
            vec![]
        }
        fn default_port(&self) -> u16 {
            0
        }
        fn status<'a>(
            &'a self,
            _ep: &'a service::Endpoint,
        ) -> service::BoxFuture<'a, Result<ServiceStatus, service::ServiceError>> {
            Box::pin(async move {
                match self.outcome {
                    Outcome::Healthy => Ok(ServiceStatus {
                        healthy: true,
                        detail: "up".into(),
                        ..Default::default()
                    }),
                    Outcome::Unhealthy => Ok(ServiceStatus {
                        healthy: false,
                        ..Default::default()
                    }),
                    Outcome::Error => Err(service::ServiceError::Transport("boom".into())),
                    Outcome::Hang => {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        Ok(ServiceStatus::default())
                    }
                }
            })
        }
    }

    fn probe_now(outcome: Outcome) -> (contract::health::Health, Option<String>, Option<String>) {
        let backend = std::sync::Arc::new(FakeBackend {
            name: "fake",
            outcome,
        });
        tokio::runtime::Runtime::new().unwrap().block_on(probe(
            backend,
            service::Endpoint::default(),
            std::time::Duration::from_millis(200),
        ))
    }

    #[test]
    fn probe_maps_healthy_to_healthy_with_detail() {
        let (health, detail, error) = probe_now(Outcome::Healthy);
        assert_eq!(health, contract::health::Health::Healthy);
        assert_eq!(detail.as_deref(), Some("up"));
        assert!(error.is_none());
    }

    #[test]
    fn probe_maps_unhealthy_to_unhealthy() {
        let (health, detail, error) = probe_now(Outcome::Unhealthy);
        assert_eq!(health, contract::health::Health::Unhealthy);
        assert!(detail.is_none());
        assert!(error.is_none());
    }

    #[test]
    fn probe_error_maps_to_unknown_and_records() {
        let (health, _detail, error) = probe_now(Outcome::Error);
        assert_eq!(health, contract::health::Health::Unknown);
        assert!(error.unwrap().contains("boom"));
    }

    #[test]
    fn probe_timeout_maps_to_unknown_and_records() {
        let (health, _detail, error) = probe_now(Outcome::Hang);
        assert_eq!(health, contract::health::Health::Unknown);
        assert!(error.unwrap().contains("timed out"));
    }

    // ── async tool handlers (no backend loaded in a unit-test process) ─────────

    fn test_ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orca-svc-ctx-{}-{}", std::process::id(), n));
        contract::ToolCtx::new(std::sync::Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("svc-test.db"),
            ports: Default::default(),
        }))
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
    }

    #[test]
    fn service_list_empty_without_backends() {
        // No service plugin registers in a unit-test process, so the list is
        // empty and paging reports no cursor and a zero total.
        let out = rt()
            .block_on(service_list(ServiceListArgs::default(), &test_ctx()))
            .unwrap();
        assert!(out.providers.is_empty());
        assert!(out.next_cursor.is_none());
        assert_eq!(out.total, Some(0));
    }

    #[test]
    fn service_create_requires_action() {
        let args = ServiceCreateArgs {
            action: None,
            endpoint: sample_args(),
        };
        // ServiceCreateOutput is not Debug, so unwrap the error via a match.
        let err = match rt().block_on(service_create(args, &test_ctx())) {
            Ok(_) => panic!("expected an error when action is absent"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("`action` is required"),
            "got: {err}"
        );
    }

    #[test]
    fn service_create_unknown_backend_errors_after_action_check() {
        // With a valid action but no registered backend, the backend lookup is
        // what fails (proving the action guard passed first).
        let args = ServiceCreateArgs {
            action: Some(ServiceCreateAction::Backup),
            endpoint: sample_args(),
        };
        let err = match rt().block_on(service_create(args, &test_ctx())) {
            Ok(_) => panic!("expected an error for an unregistered backend"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("no service backend named"),
            "got: {err}"
        );
    }

    #[test]
    fn service_update_requires_action() {
        let args = ServiceUpdateArgs {
            action: None,
            endpoint: sample_args(),
            config: "{}".into(),
            from: None,
        };
        let err = rt()
            .block_on(service_update(args, &test_ctx()))
            .unwrap_err();
        assert!(
            err.to_string().contains("`action` is required"),
            "got: {err}"
        );
    }

    #[test]
    fn service_update_unknown_backend_errors() {
        let args = ServiceUpdateArgs {
            action: Some(ServiceUpdateAction::Configure),
            endpoint: sample_args(),
            config: "{}".into(),
            from: None,
        };
        let err = rt()
            .block_on(service_update(args, &test_ctx()))
            .unwrap_err();
        assert!(
            err.to_string().contains("no service backend named"),
            "got: {err}"
        );
    }

    #[test]
    fn service_detail_unknown_backend_errors() {
        let args = ServiceDetailArgs {
            view: ServiceDetailView::Status,
            endpoint: sample_args(),
        };
        let err = rt()
            .block_on(service_detail(args, &test_ctx()))
            .unwrap_err();
        assert!(
            err.to_string().contains("no service backend named"),
            "got: {err}"
        );
    }

    #[test]
    fn service_health_single_unknown_backend_errors() {
        // A named provider that isn't registered fails the whole call (there is a
        // concrete backend to probe, so an unknown name is a hard error).
        let args = ServiceHealthArgs {
            service: Some("does-not-exist".into()),
            ..Default::default()
        };
        let err = rt()
            .block_on(service_health(args, &test_ctx()))
            .unwrap_err();
        assert!(
            err.to_string().contains("no service backend named"),
            "got: {err}"
        );
    }

    #[test]
    fn service_health_fleet_wide_empty_when_no_backends() {
        // Fleet-wide with no registered backends returns no rows and no errors.
        let out = rt()
            .block_on(service_health(ServiceHealthArgs::default(), &test_ctx()))
            .unwrap();
        assert!(out.services.is_empty());
        assert!(out.errors.is_empty());
    }

    #[test]
    fn service_health_timeout_ms_clamps_below_floor() {
        // A sub-100ms request is clamped up to the 100ms floor; with no backends
        // the aggregate is still empty, but the clamp path is exercised.
        let args = ServiceHealthArgs {
            timeout_ms: Some(1),
            ..Default::default()
        };
        let out = rt().block_on(service_health(args, &test_ctx())).unwrap();
        assert!(out.services.is_empty());
    }

    #[test]
    fn backup_output_wraps_artifact() {
        let out = BackupOutput {
            artifact: BackupArtifact {
                service: "svc".into(),
                instance: "i".into(),
                path: "/p".into(),
                ..Default::default()
            },
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["artifact"]["service"], "svc");
        assert_eq!(v["artifact"]["path"], "/p");
    }
}
