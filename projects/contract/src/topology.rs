//! Cross-crate topology types.
//!
//! `TopologyClaim` is emitted by colocated provider plugins (proxmox,
//! unraid, docker, ...) describing "this host runs that child" and consumed
//! by the system crate's inference task to derive parent_peer_id edges via
//! MAC matching. Lives here (not in `system`) so plugins can produce claims
//! without depending on `system`.
//!
//! ## Collector registry
//!
//! A colocated provider contributes claims through a [`TopologyCollector`]
//! registered into a process-global registry — either in-process or, for an
//! external subprocess plugin, a [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "topology"`. The system crate's
//! `collect_claims()` walks [`collectors`] so it stays plugin-agnostic, the
//! same way the `storage`/`notifications` domains already work.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A network endpoint a claimed workload listens on. Enables service-identity
/// correlation: a runtime [`crate::service_identity::ServiceRegistration`] keyed
/// by `(host, port)` joins to the claim whose `endpoints` contain that port on a
/// matching host.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ClaimEndpoint {
    /// Container/guest-internal listening port.
    pub port: u16,
    /// Host-published port when the runtime maps one (docker `-p`). `None` = not
    /// published to the host (reachable only on the workload's own address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_port: Option<u16>,
    /// `"tcp"` | `"udp"`. Defaults to `"tcp"`.
    #[serde(default = "default_protocol", skip_serializing_if = "is_tcp")]
    pub protocol: String,
    /// Bind address the runtime reported (e.g. `"0.0.0.0"`, `"127.0.0.1"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
}

/// One network address a claimed workload is reachable at. Mirrors a peer's
/// `pod_peer_addresses` row so claim nodes carry the same address channels
/// peers do. `kind` uses the same vocabulary as peer addresses — `"lan_v4"`,
/// `"lan_v6"`, `"tailscale_v4"`, `"tailscale_v6"`, `"fqdn"` (the constants in
/// `pod::dialer`).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ClaimAddress {
    /// Channel kind: `"lan_v4"` | `"lan_v6"` | `"tailscale_v4"` |
    /// `"tailscale_v6"` | `"fqdn"`.
    pub kind: String,
    /// The address value (IP literal or hostname).
    pub value: String,
    /// Where the provider learned this address (e.g. `"proxmox"`, `"docker"`).
    pub source: String,
}

fn default_protocol() -> String {
    "tcp".to_string()
}

fn is_tcp(p: &str) -> bool {
    p == "tcp"
}

/// One child entity a host claims to run. The inference layer matches each
/// claim's `macs` against other peers' `interfaces[].mac` to derive
/// `parent_peer_id`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct TopologyClaim {
    /// `"vm"`, `"container"`, `"lxc"`.
    pub kind: String,
    /// Provider-native id (proxmox vmid, docker container id short, ...). A
    /// searchable ATTRIBUTE used to correlate the claim across reporters —
    /// NOT the node's identity. The stable orca id is `uuid`.
    pub id: String,
    /// Stable orca UUIDv7 for this claim, minted once by the source peer (the
    /// one holding the provider creds) and persisted in `db::claim_identity`,
    /// keyed by the natural attributes (provider/provider_instance/kind/id).
    /// Every viewer of the tree uses this as the node id so they agree. Empty
    /// only from a pre-uuid reporter mid-rollout; the inventory layer guards.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uuid: String,
    pub name: String,
    /// MAC addresses associated with this child (lowercase, colon-separated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Provider that emitted this claim (`"proxmox"`, `"docker"`,
    /// `"unraid"`, ...).
    pub provider: String,
    /// Provider instance id. For docker = `"local"`; for proxmox = the
    /// endpoint name from `db::proxmox`; for secret-keyed providers = the
    /// `<instance>` segment of `<provider>.<instance>.<field>`.
    pub provider_instance: String,
    /// Hostname of the fleet node this child actually runs on, when the
    /// provider can determine it. Needed for cluster-shared config sources
    /// (proxmox pmxcfs is cluster-wide, so every cluster peer reports every
    /// guest) — the inventory layer parents a non-peer claim node to the
    /// peer whose hostname matches `runs_on`, falling back to the reporting
    /// peer when unset. Single-host providers (docker, dockge, standalone)
    /// leave this `None`; the reporting peer *is* the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<String>,
    /// Endpoints (ports) this workload listens on, when the provider can see
    /// them (docker publishes container ports; proxmox/dockge often can't).
    /// The join key for service-identity correlation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<ClaimEndpoint>,
    /// Network addresses this workload is reachable at, when the provider can
    /// resolve them. Same channel vocabulary peers carry (`lan_v4`/`lan_v6`/
    /// `tailscale_*`/`fqdn`); lets claim nodes surface addresses like peers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<ClaimAddress>,
    /// Container image / template ref, when known (docker inspect
    /// `Config.Image`). Informational; not used for role guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Provider labels/metadata (docker labels, PVE tags). Sorted key→value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Optional cheap service-role hint the provider derives from a well-known
    /// label (e.g. `orca.role`). The authoritative role comes from a runtime
    /// [`crate::service_identity::ServiceRegistration`], which overrides this at
    /// correlation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_role: Option<String>,
    /// Host-scoped compose-stack correlation key. A DESCRIPTIVE ATTRIBUTE that
    /// groups the containers of one logical service/stack together — NOT an id
    /// (respects the pure-uuidv7 identity rule; the stack node's identity is a
    /// uuidv7 minted via `unit_identity::resolve_or_mint`, keyed BY this string).
    ///
    /// Canonical form is produced by [`TopologyClaim::normalize_service_identity`]
    /// so docker and dockge — which see the same stack from different angles —
    /// emit byte-identical keys and dedup onto one stack node. Preferred source
    /// is the compose working-directory path (`/opt/stacks/<project>`); the
    /// fallback is the bare compose project name. Both are host-scoped by
    /// prefixing the host so the same stack name on two hosts stays distinct.
    /// `None` = the provider can't attribute the workload to a stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_identity: Option<String>,
    /// Normalized runtime run-state, when the provider can observe it. Cross-
    /// provider vocabulary — providers map their native status onto it:
    /// `"running"` (docker `running`, PVE `running`), `"stopped"` (docker
    /// `exited`/`created`/`dead`, PVE `stopped`), `"paused"` (docker `paused`,
    /// PVE `paused`/`suspended`). `None` = the provider can't tell (e.g. the
    /// pmxcfs conf-reader sees config, not runtime), which the inventory layer
    /// renders as `Unknown` rather than assuming down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl TopologyClaim {
    /// Build the canonical, host-scoped `service_identity` correlation key from
    /// the raw inputs a compose-aware provider (docker, dockge) can see.
    ///
    /// The output is DESCRIPTIVE, not an identity — it is the join key that lets
    /// two providers observing the same stack dedup onto one stack node. It is
    /// deliberately deterministic and reporter-agnostic: docker (which learns
    /// the stack from container labels `com.docker.compose.project.working_dir`
    /// / `com.docker.compose.project`) and dockge (which learns it from the
    /// stack directory it manages) MUST yield byte-identical strings for the
    /// same real stack, so callers route both through this helper.
    ///
    /// Precedence:
    /// 1. `working_dir` — the compose project working directory, when known.
    ///    This is the strongest signal (`/opt/stacks/<project>`); it is
    ///    path-normalized (trailing slashes stripped, collapsed).
    /// 2. `project` — the bare compose project name, when no working dir is
    ///    available.
    ///
    /// Both are host-scoped by prefixing `host` so the same stack name on two
    /// hosts stays distinct. Returns `None` when neither signal is present or
    /// both are blank after trimming.
    ///
    /// Canonical shape: `"<host>\u{1f}<normalized-signal>"` — the `\u{1f}` unit
    /// separator can't occur in a hostname or a filesystem path, so the two
    /// segments never collide.
    pub fn normalize_service_identity(
        host: &str,
        working_dir: Option<&str>,
        project: Option<&str>,
    ) -> Option<String> {
        let host = normalize_host_scope(host);

        // Prefer the working-directory path signal.
        if let Some(wd) = working_dir {
            let wd = normalize_path_key(wd);
            if !wd.is_empty() {
                return Some(format!("{host}\u{1f}{wd}"));
            }
        }
        // Fall back to the bare compose project name.
        if let Some(p) = project {
            let p = p.trim().trim_matches('/');
            if !p.is_empty() {
                return Some(format!("{host}\u{1f}{p}"));
            }
        }
        None
    }
}

/// Host-scope prefix: trimmed, lowercased so casing differences between
/// reporters (`Freyr` vs `freyr`) don't fork the key.
fn normalize_host_scope(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

/// Normalize a compose working-directory path into a stable key segment:
/// trim whitespace, strip a trailing slash, and collapse any runs of `/`.
/// Case is preserved (POSIX paths are case-sensitive). Empty in → empty out.
fn normalize_path_key(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return String::new();
    }
    let trimmed = path.trim_end_matches('/');
    // Collapse duplicate slashes without allocating unless needed.
    if !trimmed.contains("//") {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_slash = false;
    for ch in trimmed.chars() {
        if ch == '/' {
            if prev_slash {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        out.push(ch);
    }
    out
}

// ── Collector registry ──────────────────────────────────────────────────────

/// A source of [`TopologyClaim`]s — one per provider (proxmox, docker, …).
/// Registered into the process-global registry so the system crate's
/// `collect_claims()` can fan out across providers plugin-agnostically.
#[async_trait::async_trait]
pub trait TopologyCollector: Send + Sync {
    /// Provider/registry name (e.g. `"proxmox"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    async fn collect_claims(&self) -> Result<Vec<TopologyClaim>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn TopologyCollector>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a topology collector with the process-global registry.
/// Re-registering the same `name()` replaces the existing entry so a dev
/// rebuild / plugin reload doesn't duplicate collectors.
pub fn register_collector(collector: Arc<dyn TopologyCollector>) {
    let mut g = GLOBAL.write().expect("topology registry poisoned");
    let name = collector.name().to_string();
    if let Some(slot) = g.iter_mut().find(|c| c.name() == name) {
        *slot = collector;
    } else {
        g.push(collector);
    }
}

/// Snapshot of every registered collector.
pub fn collectors() -> Vec<Arc<dyn TopologyCollector>> {
    GLOBAL.read().expect("topology registry poisoned").clone()
}

/// Deregister the collector named `name`, if present. The reversal path a
/// plugin unload needs. Returns `true` if a collector was removed.
pub fn deregister_collector(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("topology registry poisoned");
    let before = g.len();
    g.retain(|c| c.name() != name);
    before != g.len()
}

/// The synchronous invoke thunk a loaded plugin's topology collector is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn`
/// of strings so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation name the [`TopologyCollectorProxy`] invokes across the FFI
/// boundary. The plugin exposes a tool `"{invoke_prefix}.{COLLECT_OP}"`
/// returning a JSON `Vec<TopologyClaim>`.
pub const COLLECT_OP: &str = "collect_claims";

/// Build and register a [`TopologyCollector`] from a plugin backend descriptor
/// plus an [`InvokeThunk`]. The plugin-loader calls this from its domain
/// dispatch table for `domain = "topology"`.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_collector(Arc::new(TopologyCollectorProxy { name, invoke }));
    Ok(())
}

/// A [`TopologyCollector`] backed by a subprocess plugin reached over the
/// JSON-proxy FFI boundary. `collect_claims()` offloads the synchronous
/// [`InvokeThunk`] onto `spawn_blocking` and deserializes the JSON result.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct TopologyCollectorProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
#[async_trait::async_trait]
impl TopologyCollector for TopologyCollectorProxy {
    fn name(&self) -> &str {
        &self.name
    }

    async fn collect_claims(&self) -> Result<Vec<TopologyClaim>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let out = tokio::task::spawn_blocking(move || invoke(COLLECT_OP, "{}".to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("topology '{name}' invoke task panicked: {e}"))?
            .map_err(|e| anyhow::anyhow!("topology '{name}' invoke failed: {e}"))?;
        let claims: Vec<TopologyClaim> = serde_json::from_str(&out)
            .map_err(|e| anyhow::anyhow!("topology '{name}' returned invalid JSON: {e}"))?;
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (c) docker and dockge see the same stack from different angles but MUST
    /// produce byte-identical service_identity keys. Docker learns the working
    /// dir from a container label; dockge learns it from the stack directory it
    /// manages — both route through the same normalizer with the same inputs.
    #[test]
    fn normalization_identical_for_equivalent_docker_and_dockge_inputs() {
        // Docker path: label value may carry a trailing slash and casing on host.
        let from_docker = TopologyClaim::normalize_service_identity(
            "Freyr",
            Some("/opt/stacks/jellyfin/"),
            Some("jellyfin"),
        );
        // Dockge path: manages `/opt/stacks/jellyfin`, reports lowercase host.
        let from_dockge = TopologyClaim::normalize_service_identity(
            "freyr",
            Some("/opt/stacks/jellyfin"),
            Some("jellyfin"),
        );
        assert_eq!(from_docker, from_dockge);
        assert!(from_docker.is_some());
    }

    #[test]
    fn working_dir_takes_precedence_over_project_name() {
        let wd = TopologyClaim::normalize_service_identity(
            "host1",
            Some("/opt/stacks/arr"),
            Some("different-project-name"),
        );
        let proj_only = TopologyClaim::normalize_service_identity(
            "host1",
            None,
            Some("different-project-name"),
        );
        assert_ne!(wd, proj_only);
        assert_eq!(wd, Some("host1\u{1f}/opt/stacks/arr".to_string()));
    }

    #[test]
    fn falls_back_to_project_name_without_working_dir() {
        let key = TopologyClaim::normalize_service_identity("host1", None, Some("media"));
        assert_eq!(key, Some("host1\u{1f}media".to_string()));
    }

    #[test]
    fn blank_working_dir_falls_through_to_project() {
        let key = TopologyClaim::normalize_service_identity("host1", Some("   "), Some("media"));
        assert_eq!(key, Some("host1\u{1f}media".to_string()));
    }

    #[test]
    fn none_when_no_signal() {
        assert_eq!(
            TopologyClaim::normalize_service_identity("host1", None, None),
            None
        );
        assert_eq!(
            TopologyClaim::normalize_service_identity("host1", Some("/"), Some("  ")),
            None
        );
    }

    #[test]
    fn same_stack_name_on_two_hosts_stays_distinct() {
        let a = TopologyClaim::normalize_service_identity("hosta", Some("/opt/stacks/db"), None);
        let b = TopologyClaim::normalize_service_identity("hostb", Some("/opt/stacks/db"), None);
        assert_ne!(a, b);
    }

    #[test]
    fn path_normalization_collapses_slashes_and_trailing() {
        let a = TopologyClaim::normalize_service_identity("h", Some("/opt//stacks///app//"), None);
        let b = TopologyClaim::normalize_service_identity("h", Some("/opt/stacks/app"), None);
        assert_eq!(a, b);
    }
}
