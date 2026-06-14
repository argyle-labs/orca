//! LXC adapter for Proxmox VE via the HTTPS API.
//!
//! Per [[project-adapter-backends-api-first]]: the legacy `LxcProxmoxAdapter`
//! shells `pct list` and reads `/etc/pve/lxc/<vmid>.conf` directly, which
//! only works when orca runs ON a Proxmox host and bypasses Proxmox auth
//! entirely. This adapter talks the documented API
//! (`https://<node>:8006/api2/json`) through the `proxmox::Client`
//! crate, with PVE API-token auth. It works remotely, respects
//! Proxmox permissions, and gives us cluster-aware enumeration in one
//! request via `/cluster/resources?type=vm`.
//!
//! The `Container.host` field is the Proxmox node name (e.g. `"thor"`,
//! `"frigg"`) for every container this adapter returns. The breaker
//! keys on `(host, runtime, container_id)`, so multi-node enumeration
//! flows through unchanged.
//!
//! **Why the round-trip serialize:** the workspace bans opaque JSON
//! types in product code — every shape that crosses this file's
//! boundary is a typed struct deriving `Deserialize`. The
//! `proxmox::Client` is the upstream-shape boundary; it returns the
//! raw envelope, and we immediately parse it through
//! `to_string`/`from_str` into the typed views below. The double
//! serialization cost is acceptable for the reconciler's tick
//! cadence; if it ever shows up in profiles, the fix is to push
//! typed wrappers into the `proxmox` crate.

use async_trait::async_trait;
use plugin_toolkit::containers::{
    AdapterError, Container, ContainerState, ListFilter, LogTail, RestartPolicy, RuntimeAdapter,
    RuntimeKind,
};
use serde::{Deserialize, Serialize};

use crate::{Client as ProxmoxClient, ProxmoxAction};

/// LXC adapter that routes every operation through one Proxmox API
/// endpoint. Multi-endpoint orchestration (different clusters) lives
/// at the reconciler-entry level: one adapter per endpoint, both
/// registered.
pub struct LxcProxmoxApiAdapter {
    client: ProxmoxClient,
    /// Display name for the endpoint — surfaced in tracing / error
    /// context, not in the typed surface. Matches the
    /// `db::proxmox::EndpointRow::name` the tool layer uses.
    endpoint_name: String,
}

impl LxcProxmoxApiAdapter {
    pub fn new(client: ProxmoxClient, endpoint_name: impl Into<String>) -> Self {
        Self {
            client,
            endpoint_name: endpoint_name.into(),
        }
    }

    /// Endpoint label this adapter was constructed with. Used by the
    /// reconciler entry to keep error rows aligned with their source.
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
        // `stop` (hard). We map `RuntimeAdapter::stop` to `shutdown`
        // to match the docker adapter's graceful default (bollard's
        // `stop` has a timeout). Operators wanting a hard stop can
        // call `proxmox.update --action stop` directly.
        self.lifecycle(id, ProxmoxAction::Shutdown).await
    }

    async fn restart(&self, id: &str) -> Result<(), AdapterError> {
        self.lifecycle(id, ProxmoxAction::Reboot).await
    }

    async fn logs(&self, _id: &str, _tail: LogTail) -> Result<String, AdapterError> {
        // Proxmox doesn't expose container-internal logs cleanly over
        // the API — `pct exec` runs commands but doesn't stream
        // journalctl in a structured shape. Deferred until we either
        // add a typed syslog endpoint to the `proxmox` crate or route
        // through a node-resident agent.
        Err(AdapterError::Refused(
            "LxcProxmoxApiAdapter::logs requires the syslog endpoint (not yet wired)".into(),
        ))
    }

    // `observe()` falls through to the trait default
    // (`HostObservation::default()`) for now. Real journal-tail fetch
    // lands once a typed syslog endpoint exists on the proxmox client.
}

impl LxcProxmoxApiAdapter {
    /// Fetch the cluster resource list and return only LXC rows.
    async fn fetch_lxc_rows(&self) -> Result<Vec<ClusterResource>, AdapterError> {
        let raw = self
            .client
            .cluster_vm_resources()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        let envelope = reparse::<ClusterResourcesEnvelope>(&raw, "cluster_vm_resources")?;
        Ok(envelope
            .data
            .into_iter()
            .filter(|r| r.kind == "lxc")
            .collect())
    }

    async fn build_container(&self, row: &ClusterResource) -> Result<Container, AdapterError> {
        // Per-container config fetch populates restart_policy from
        // `onboot`. Cluster resources don't include it; one GET per
        // container. For large clusters this should batch via
        // `try_join_all` — TODO once we see scale.
        let raw_cfg = self
            .client
            .guest_config(&row.node, row.vmid, crate::GuestKind::Lxc)
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        let cfg = reparse::<LxcConfigEnvelope>(&raw_cfg, "guest_config")?;
        let restart_policy = if cfg.data.onboot != 0 {
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
            // C-series: mount enumeration via guest_config's mp*
            // entries. The pct-shell adapter parses the same format
            // off-disk; reusing that parser here is a follow-up.
            mounts: Vec::new(),
            ports: Vec::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            // Proxmox exposes last task exit codes via
            // `/nodes/<node>/tasks` — wiring deferred (task #5).
            exit_code: None,
            startup: None,
        })
    }

    async fn lifecycle(&self, id: &str, action: ProxmoxAction) -> Result<(), AdapterError> {
        let vmid: u64 = id
            .parse()
            .map_err(|_| AdapterError::NotFound(format!("vmid `{id}` is not a u64")))?;
        // We need to know which node the container lives on. One
        // cluster query gives us the mapping; cache opportunity if
        // this becomes hot. For now: re-query each tick.
        let rows = self.fetch_lxc_rows().await?;
        let node = rows
            .into_iter()
            .find(|r| r.vmid == vmid)
            .map(|r| r.node)
            .ok_or_else(|| AdapterError::NotFound(format!("lxc vmid `{vmid}` not in cluster")))?;

        self.client
            .container_action(&node, vmid, action)
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        Ok(())
    }
}

// ── Typed upstream views ───────────────────────────────────────────────────
//
// Mirror the subset of Proxmox API JSON we actually consume. Fields the
// API omits use `#[serde(default)]` so a missing key is the safe value
// (rather than a parse failure that wedges the whole tick).

#[derive(Debug, Clone, Deserialize)]
struct ClusterResourcesEnvelope {
    #[serde(default)]
    data: Vec<ClusterResource>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClusterResource {
    /// `"lxc"`, `"qemu"`, `"storage"`, `"node"`, `"pool"` — the
    /// non-guest variants get filtered out at the call site.
    #[serde(rename = "type")]
    kind: String,
    vmid: u64,
    node: String,
    #[serde(default)]
    name: Option<String>,
    /// Proxmox state string. Mapped to `ContainerState` via
    /// [`map_proxmox_status`]; unknowns survive as
    /// `ContainerState::Unknown(_)` rather than getting rejected.
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct LxcConfigEnvelope {
    #[serde(default)]
    data: LxcConfigData,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LxcConfigData {
    /// `0` or `1`. Treated as the restart_policy signal: nonzero =
    /// Always, zero = No.
    #[serde(default)]
    onboot: u64,
}

/// Re-parse a value the proxmox client returned into a typed view.
///
/// The `proxmox::Client` returns an opaque upstream envelope; this
/// crate's policy disallows raw dynamic-JSON types in product code
/// (per the file-level comment), so we round-trip through a JSON
/// string to get back into the typed world. `context` lands in the
/// error message so a malformed response names the endpoint.
fn reparse<T: for<'de> Deserialize<'de>>(
    raw: &impl Serialize,
    context: &'static str,
) -> Result<T, AdapterError> {
    let s = serde_json::to_string(raw)
        .map_err(|e| AdapterError::Malformed(format!("{context}: re-encode: {e}")))?;
    serde_json::from_str(&s)
        .map_err(|e| AdapterError::Malformed(format!("{context}: typed parse: {e}")))
}

fn map_proxmox_status(s: &str) -> ContainerState {
    match s {
        "running" => ContainerState::Running,
        "stopped" => ContainerState::Exited,
        "paused" => ContainerState::Paused,
        // `ContainerState::Unknown` is unit (no carried string); the
        // raw status is lost here. If operators need to see it,
        // `inspect()` can surface it via a separate notes field —
        // pending a Container model extension.
        _ => ContainerState::Unknown,
    }
}

fn labels_match(have: &[(String, String)], wanted: &[(String, String)]) -> bool {
    wanted
        .iter()
        .all(|w| have.iter().any(|h| h.0 == w.0 && h.1 == w.1))
}
