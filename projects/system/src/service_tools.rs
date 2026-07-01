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
pub struct ServiceListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ServiceListOutput {
    pub providers: Vec<ServiceProvider>,
}

/// Every service backend registered with this daemon, with the runtimes and
/// lifecycle capabilities each advertises. Empty before any service plugin loads.
#[orca_tool(domain = "service", verb = "list")]
async fn service_list(
    _args: ServiceListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ServiceListOutput> {
    Ok(ServiceListOutput {
        providers: service::providers(),
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

// ── deploy (composes deploy_target) ──────────────────────────────────

/// Build the service's `WorkloadSpec` and place it on a matching deploy target.
/// The service backend describes *what* to run; the deploy target runs it. The
/// runtime comes from the shared `--runtime` flag on the endpoint args.
#[orca_tool(domain = "service", verb = "deploy")]
async fn service_deploy(
    args: EndpointArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DeployOutcome> {
    let backend = backend_for(&args.service)?;
    let runtime_str = args
        .runtime
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--runtime is required for deploy"))?;
    let runtime = parse_runtime(&runtime_str)?;
    let ep = args.endpoint();

    let spec = backend.workload_spec(runtime, &ep).await?;

    // Resolve a deploy target on this host + runtime that can launch.
    let target = deploy_target::targets()
        .into_iter()
        .find(|t| {
            t.host() == args.host && t.runtime() == runtime && t.supports(DeployCapability::Launch)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no deploy target on host `{}` with runtime `{}` that can launch",
                args.host,
                runtime_str
            )
        })?;

    Ok(target.launch(&spec).await?)
}

// ── backup / restore / configure / status ────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupOutput {
    pub artifact: BackupArtifact,
}

/// Snapshot a service instance's config/data into a restorable artifact.
#[orca_tool(domain = "service", verb = "backup")]
async fn service_backup(
    args: EndpointArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<BackupOutput> {
    let backend = backend_for(&args.service)?;
    Ok(BackupOutput {
        artifact: backend.backup(&args.endpoint()).await?,
    })
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceRestoreArgs {
    #[command(flatten)]
    pub endpoint: EndpointArgs,
    /// Path of the backup artifact to restore from.
    #[arg(long)]
    pub from: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OkOutput {
    pub ok: bool,
}

/// Restore a service instance from a backup artifact path.
#[orca_tool(domain = "service", verb = "restore")]
async fn service_restore(
    args: ServiceRestoreArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<OkOutput> {
    let backend = backend_for(&args.endpoint.service)?;
    let artifact = BackupArtifact {
        service: args.endpoint.service.clone(),
        instance: args.endpoint.instance.clone(),
        path: args.from.clone(),
        ..Default::default()
    };
    backend
        .restore(&args.endpoint.endpoint(), &artifact)
        .await?;
    Ok(OkOutput { ok: true })
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceConfigureArgs {
    #[command(flatten)]
    pub endpoint: EndpointArgs,
    /// Service-specific configuration payload (JSON the backend interprets).
    #[arg(long, default_value = "{}")]
    pub config: String,
}

/// Apply service-specific configuration to an instance idempotently.
#[orca_tool(domain = "service", verb = "configure")]
async fn service_configure(
    args: ServiceConfigureArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<OkOutput> {
    let backend = backend_for(&args.endpoint.service)?;
    backend
        .configure(&args.endpoint.endpoint(), &args.config)
        .await?;
    Ok(OkOutput { ok: true })
}

/// Health/diagnostics for a service instance.
#[orca_tool(domain = "service", verb = "status")]
async fn svc_status(args: EndpointArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<ServiceStatus> {
    let backend = backend_for(&args.service)?;
    Ok(backend.status(&args.endpoint()).await?)
}
