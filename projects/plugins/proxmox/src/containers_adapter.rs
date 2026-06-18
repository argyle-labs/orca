//! LXC adapter for Proxmox VE via the HTTPS API.
//!
//! Per [[project-adapter-backends-api-first]]: the legacy `LxcProxmoxAdapter`
//! shells `pct list` and reads `/etc/pve/lxc/<vmid>.conf` directly, which
//! only works when orca runs ON a Proxmox host and bypasses Proxmox auth
//! entirely. This adapter talks the documented API
//! (`https://<node>:8006/api2/json`) through the progenitor-generated
//! `proxmox::generated::Client`, with PVE API-token auth. Works
//! remotely, respects Proxmox permissions, and gives cluster-aware
//! enumeration via `/cluster/resources?type=vm`.
//!
//! The `Container.host` field is the Proxmox node name for every
//! container this adapter returns. The breaker keys on
//! `(host, runtime, container_id)`, so multi-node enumeration flows
//! through unchanged.

use async_trait::async_trait;
use plugin_toolkit::containers::{
    AdapterError, Container, ContainerState, ListFilter, Liveness, LogTail, RestartPolicy,
    RuntimeAdapter, RuntimeKind, WedgeRecoverer,
};
use std::time::Duration;

use crate::generated::{self, types as gtypes};
use crate::{GuestKind, ProxmoxAction, fetch_guest_config};

/// Budget for the liveness probe. Tight on purpose — the reconciler
/// can call this every tick on every running LXC, so a hung probe
/// would block forward progress on the whole tick.
const PROBE_TIMEOUT_SECS: u64 = 5;

/// How long to wait after a `Stop` for the container to report
/// `stopped`, and after a `Start` for it to report `running`.
const RECOVERY_POLL_BUDGET_SECS: u64 = 15;

/// Poll cadence while waiting on status transitions.
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// LXC adapter that routes every operation through one Proxmox API
/// endpoint. Multi-endpoint orchestration (different clusters) lives
/// at the reconciler-entry level: one adapter per endpoint, both
/// registered.
pub struct LxcProxmoxApiAdapter {
    client: generated::Client,
    http: reqwest::Client,
    base_url: String,
    /// Display name for the endpoint — surfaced in tracing / error
    /// context, not in the typed surface.
    endpoint_name: String,
}

impl LxcProxmoxApiAdapter {
    pub fn new(
        client: generated::Client,
        http: reqwest::Client,
        base_url: impl Into<String>,
        endpoint_name: impl Into<String>,
    ) -> Self {
        Self {
            client,
            http,
            base_url: base_url.into(),
            endpoint_name: endpoint_name.into(),
        }
    }

    /// Endpoint label this adapter was constructed with.
    pub fn endpoint_name(&self) -> &str {
        &self.endpoint_name
    }
}

#[async_trait]
impl RuntimeAdapter for LxcProxmoxApiAdapter {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Lxc
    }

    async fn list(&self, filter: &ListFilter) -> Result<Vec<Container>, AdapterError> {
        let rows = self.fetch_lxc_rows().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let container = self.build_container(&row).await?;
            if labels_match(&container.labels, &filter.labels) {
                out.push(container);
            }
        }
        Ok(out)
    }

    async fn inspect(&self, id: &str) -> Result<Container, AdapterError> {
        let _: u64 = id
            .parse()
            .map_err(|_| AdapterError::NotFound(format!("vmid `{id}` is not a u64")))?;
        let all = self.list(&ListFilter::default()).await?;
        all.into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| AdapterError::NotFound(format!("lxc vmid `{id}`")))
    }

    async fn start(&self, id: &str) -> Result<(), AdapterError> {
        self.lifecycle(id, ProxmoxAction::Start).await
    }

    async fn stop(&self, id: &str) -> Result<(), AdapterError> {
        // The Proxmox API distinguishes `shutdown` (graceful) from
        // `stop` (hard). Map `RuntimeAdapter::stop` to `shutdown` to
        // match the docker adapter's graceful default.
        self.lifecycle(id, ProxmoxAction::Shutdown).await
    }

    async fn restart(&self, id: &str) -> Result<(), AdapterError> {
        self.lifecycle(id, ProxmoxAction::Reboot).await
    }

    async fn logs(&self, _id: &str, _tail: LogTail) -> Result<String, AdapterError> {
        Err(AdapterError::Refused(
            "LxcProxmoxApiAdapter::logs requires the syslog endpoint (not yet wired)".into(),
        ))
    }

    /// Local-subprocess probe: `pct exec <vmid> -- true` with a tight
    /// timeout. See [[feedback-api-first-liveness-exception]].
    async fn probe_liveness(&self, container: &Container) -> Liveness {
        if container.runtime != RuntimeKind::Lxc {
            return Liveness::NotApplicable;
        }
        let mut cmd = tokio::process::Command::new("pct");
        cmd.arg("exec").arg(&container.id).arg("--").arg("true");
        cmd.kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return Liveness::Unknown,
        };
        match tokio::time::timeout(Duration::from_secs(PROBE_TIMEOUT_SECS), child.wait()).await {
            Ok(Ok(status)) if status.success() => Liveness::Live,
            Ok(Ok(_)) => Liveness::Unknown,
            Ok(Err(_)) => Liveness::Unknown,
            Err(_) => {
                drop(child.start_kill());
                Liveness::Wedged
            }
        }
    }

    fn wedge_recoverer(&self) -> Option<&dyn WedgeRecoverer> {
        Some(self)
    }
}

#[async_trait]
impl WedgeRecoverer for LxcProxmoxApiAdapter {
    /// API-only recovery: hard `Stop` then `Start`, polling status
    /// between transitions. `Stop` is the hard variant — `Shutdown`
    /// would hang on exactly the wedged PID-1 case this exists to
    /// recover.
    async fn attempt_unwedge(&self, container: &Container) -> Result<(), AdapterError> {
        let vmid: u64 = container
            .id
            .parse()
            .map_err(|_| AdapterError::NotFound(format!("vmid `{}` is not a u64", container.id)))?;
        let node = container.host.clone();

        self.do_lifecycle(&node, vmid, ProxmoxAction::Stop).await?;
        wait_for_status(&self.client, &node, vmid, "stopped").await?;

        self.do_lifecycle(&node, vmid, ProxmoxAction::Start).await?;
        wait_for_status(&self.client, &node, vmid, "running").await?;

        Ok(())
    }
}

/// Poll the LXC current-status endpoint until `data.status` matches
/// `expected`, up to [`RECOVERY_POLL_BUDGET_SECS`].
async fn wait_for_status(
    client: &generated::Client,
    node: &str,
    vmid: u64,
    expected: &str,
) -> Result<(), AdapterError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(RECOVERY_POLL_BUDGET_SECS);
    loop {
        let st = client
            .get_vm_status_nodes_node_lxc_vmid_status_current(node, vmid as i64)
            .await
            .map_err(|e| AdapterError::Transport(format!("status {vmid}: {e}")))?
            .into_inner();
        if st.status.to_string() == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AdapterError::Refused(format!(
                "container {vmid} did not reach `{expected}` within {RECOVERY_POLL_BUDGET_SECS}s"
            )));
        }
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
}

impl LxcProxmoxApiAdapter {
    /// Fetch the cluster resource list and return only LXC rows.
    async fn fetch_lxc_rows(&self) -> Result<Vec<LxcRow>, AdapterError> {
        let items = self
            .client
            .get_resources_cluster_resources(Some(gtypes::GetResourcesClusterResourcesType::Vm))
            .await
            .map_err(|e| AdapterError::Transport(format!("cluster resources: {e}")))?
            .into_inner();
        Ok(items
            .into_iter()
            .filter_map(|r| {
                if !matches!(
                    r.type_,
                    gtypes::GetResourcesClusterResourcesResponseItemType::Lxc
                ) {
                    return None;
                }
                let node = r.node?;
                let vmid = r.vmid?;
                if vmid <= 0 {
                    return None;
                }
                Some(LxcRow {
                    vmid: vmid as u64,
                    node,
                    name: r.name,
                    status: r.status,
                })
            })
            .collect())
    }

    async fn build_container(&self, row: &LxcRow) -> Result<Container, AdapterError> {
        // Per-container config fetch populates restart_policy from
        // `onboot`. Raw URL because progenitor can't model indexed
        // keys; see `fetch_guest_config` rationale.
        let cfg = fetch_guest_config(
            &self.http,
            &self.base_url,
            &row.node,
            GuestKind::Lxc,
            row.vmid,
        )
        .await
        .map_err(|e| AdapterError::Transport(format!("guest_config: {e}")))?;
        let restart_policy = if cfg.data.onboot() {
            RestartPolicy::Always
        } else {
            RestartPolicy::No
        };

        let name = row
            .name
            .clone()
            .unwrap_or_else(|| format!("ct-{}", row.vmid));
        let state = row
            .status
            .as_deref()
            .map(map_proxmox_status)
            .unwrap_or(ContainerState::Unknown);

        Ok(Container {
            id: row.vmid.to_string(),
            name,
            runtime: RuntimeKind::Lxc,
            host: row.node.clone(),
            state,
            restart_policy,
            image: None,
            labels: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            exit_code: None,
            startup: None,
        })
    }

    async fn lifecycle(&self, id: &str, action: ProxmoxAction) -> Result<(), AdapterError> {
        let vmid: u64 = id
            .parse()
            .map_err(|_| AdapterError::NotFound(format!("vmid `{id}` is not a u64")))?;
        let rows = self.fetch_lxc_rows().await?;
        let node = rows
            .into_iter()
            .find(|r| r.vmid == vmid)
            .map(|r| r.node)
            .ok_or_else(|| AdapterError::NotFound(format!("lxc vmid `{vmid}` not in cluster")))?;
        self.do_lifecycle(&node, vmid, action).await
    }

    async fn do_lifecycle(
        &self,
        node: &str,
        vmid: u64,
        action: ProxmoxAction,
    ) -> Result<(), AdapterError> {
        let vmid_i = vmid as i64;
        let res = match action {
            ProxmoxAction::Start => self
                .client
                .post_vm_start_nodes_node_lxc_vmid_status_start(node, vmid_i, &Default::default())
                .await
                .map(|_| ()),
            ProxmoxAction::Stop => self
                .client
                .post_vm_stop_nodes_node_lxc_vmid_status_stop(node, vmid_i, &Default::default())
                .await
                .map(|_| ()),
            ProxmoxAction::Shutdown => self
                .client
                .post_vm_shutdown_nodes_node_lxc_vmid_status_shutdown(
                    node,
                    vmid_i,
                    &Default::default(),
                )
                .await
                .map(|_| ()),
            ProxmoxAction::Reboot => self
                .client
                .post_vm_reboot_nodes_node_lxc_vmid_status_reboot(node, vmid_i, &Default::default())
                .await
                .map(|_| ()),
        };
        res.map_err(|e| AdapterError::Transport(format!("{} {vmid}: {e}", action.as_str())))
    }
}

#[derive(Debug, Clone)]
struct LxcRow {
    vmid: u64,
    node: String,
    name: Option<String>,
    status: Option<String>,
}

fn map_proxmox_status(s: &str) -> ContainerState {
    match s {
        "running" => ContainerState::Running,
        "stopped" => ContainerState::Exited,
        "paused" => ContainerState::Paused,
        _ => ContainerState::Unknown,
    }
}

fn labels_match(have: &[(String, String)], wanted: &[(String, String)]) -> bool {
    wanted
        .iter()
        .all(|w| have.iter().any(|h| h.0 == w.0 && h.1 == w.1))
}
