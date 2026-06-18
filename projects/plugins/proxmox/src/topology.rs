//! Proxmox → TopologyClaim collector (API path).
//!
//! Replaces the file-reading collector in `system::topology::proxmox` for
//! every host with a registered Proxmox endpoint. Architectural rationale
//! per [[project-adapter-backends-api-first]]: runtime adapters speak the
//! native API, not host-local CLI / pmxcfs files.
//!
//! Why this matters for the systems tree: until claims propagate, the
//! inference layer has nothing to join against and every guest renders as
//! a top-level peer (e.g. baldur shows next to frigg instead of nested
//! under it).
//!
//! Two-step fetch per endpoint:
//!   1. `GET /cluster/resources?type=vm` — every VM + LXC in the cluster
//!      with vmid/node/type/name. One round-trip lists the whole fleet.
//!   2. `GET /nodes/{node}/{kind}/{vmid}/config` per guest — `netN` fields
//!      are parsed for MACs. Calls fan out concurrently to keep
//!      wall-clock flat.
//!
//! Errors are scoped per endpoint and per guest: a broken endpoint blanks
//! that endpoint's contribution but doesn't kill claims from others; a
//! guest whose config 404s is skipped silently (it may have been deleted
//! between the cluster-list and the config fetch).

use crate::{Client, Config, GuestKind};
use contract::TopologyClaim;
use db::pool::with_pooled_or_open;
use serde::Deserialize;
use serde_json as sj;
use std::collections::BTreeMap;

/// Walk every registered + enabled Proxmox endpoint and return the union
/// of TopologyClaims. Endpoints that fail are logged and skipped.
pub async fn collect_claims() -> anyhow::Result<Vec<TopologyClaim>> {
    let endpoints = with_pooled_or_open(crate::tools::endpoint_db::list)?;

    let mut all = Vec::new();
    for ep in endpoints.into_iter().filter(|e| e.enabled) {
        let provider_instance = ep.name.clone();
        let cfg = Config::new(ep.base_url, ep.token_id, ep.token_secret).insecure(ep.insecure);
        let client = Client::new(cfg);
        match collect_for_endpoint(&client, &provider_instance).await {
            Ok(mut v) => all.append(&mut v),
            Err(e) => {
                tracing::warn!(
                    endpoint = %provider_instance,
                    error = %e,
                    "proxmox topology: endpoint collector failed",
                );
            }
        }
    }
    Ok(all)
}

async fn collect_for_endpoint(
    client: &Client,
    provider_instance: &str,
) -> anyhow::Result<Vec<TopologyClaim>> {
    let raw = client.cluster_vm_resources().await?;
    let resp: ClusterResourcesResponse = sj::from_value(raw)
        .map_err(|e| anyhow::anyhow!("proxmox cluster resources: malformed JSON: {e}"))?;
    let guests = resp.guests();

    // Fan out config fetches. Each guest is one round-trip; sequential
    // would be O(N × RTT). At small fleet sizes (≤200 guests) `join_all`
    // is fine — switch to a bounded semaphore if it ever exceeds that.
    let futs = guests.into_iter().map(|g| async move {
        let kind = match g.kind.as_str() {
            "qemu" => GuestKind::Qemu,
            "lxc" => GuestKind::Lxc,
            _ => return None,
        };
        let raw_cfg = match client.guest_config(&g.node, g.vmid, kind).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    node = %g.node,
                    vmid = g.vmid,
                    error = %e,
                    "proxmox topology: guest_config failed (guest may have been deleted)",
                );
                return None;
            }
        };
        let cfg: GuestConfigResponse = sj::from_value(raw_cfg).ok()?;
        let macs = cfg.macs();
        if macs.is_empty() {
            return None;
        }
        Some(TopologyClaim {
            kind: kind_to_claim_kind(kind).to_string(),
            id: g.vmid.to_string(),
            name: g
                .name
                .unwrap_or_else(|| format!("{}-{}", kind_to_claim_kind(kind), g.vmid)),
            macs,
            provider: "proxmox".to_string(),
            provider_instance: provider_instance.to_string(),
        })
    });
    let claims = futures_util::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect();
    Ok(claims)
}

fn kind_to_claim_kind(k: GuestKind) -> &'static str {
    match k {
        GuestKind::Qemu => "vm",
        GuestKind::Lxc => "lxc",
    }
}

// ── Typed cluster_vm_resources response ─────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct ClusterResourcesResponse {
    #[serde(default)]
    data: Vec<ClusterResourceEntry>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct ClusterResourceEntry {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    node: String,
    #[serde(default)]
    vmid: u64,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestRef {
    node: String,
    vmid: u64,
    /// `"qemu"` or `"lxc"`.
    kind: String,
    name: Option<String>,
}

impl ClusterResourcesResponse {
    fn guests(self) -> Vec<GuestRef> {
        self.data
            .into_iter()
            .filter_map(|e| {
                if e.kind != "qemu" && e.kind != "lxc" {
                    return None;
                }
                if e.node.is_empty() || e.vmid == 0 {
                    return None;
                }
                Some(GuestRef {
                    node: e.node,
                    vmid: e.vmid,
                    kind: e.kind,
                    name: e.name,
                })
            })
            .collect()
    }
}

// ── Typed guest_config response ─────────────────────────────────────────────
//
// The full config has many keys with mixed scalar types (strings, ints,
// bools). We care about exactly one shape: `netN` keys, which are always
// strings. `ConfigField::Str` captures those; `Skip` consumes everything
// else without allocating.

#[derive(Debug, Deserialize, Default)]
struct GuestConfigResponse {
    #[serde(default)]
    data: BTreeMap<String, ConfigField>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfigField {
    Str(String),
    Skip(serde::de::IgnoredAny),
}

impl GuestConfigResponse {
    fn macs(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (k, v) in &self.data {
            if !k.starts_with("net") {
                continue;
            }
            let ConfigField::Str(s) = v else { continue };
            if let Some(mac) = extract_mac(s) {
                out.push(mac);
            }
        }
        out
    }
}

/// Copy of `system::topology::proxmox::extract_mac`. Kept inline rather
/// than reaching across crate boundaries — the parser is small + stable.
fn extract_mac(line: &str) -> Option<String> {
    let lower = line.to_lowercase();
    let bytes = lower.as_bytes();
    for start in 0..bytes.len().saturating_sub(17) {
        let win = &lower[start..start + 17];
        if is_mac(win) {
            if start > 0 && lower.as_bytes()[start - 1].is_ascii_hexdigit() {
                continue;
            }
            return Some(win.to_string());
        }
    }
    None
}

fn is_mac(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 17 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        if (i + 1) % 3 == 0 {
            if *b != b':' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_resources_filters_to_qemu_and_lxc() {
        let raw = sj::json!({
            "data": [
                {"type": "qemu", "node": "frigg", "vmid": 100, "name": "tyr"},
                {"type": "lxc",  "node": "frigg", "vmid": 110, "name": "baldur"},
                {"type": "storage", "node": "frigg", "storage": "local"}
            ]
        });
        let resp: ClusterResourcesResponse = sj::from_value(raw).unwrap();
        let guests = resp.guests();
        assert_eq!(guests.len(), 2);
        assert_eq!(guests[0].kind, "qemu");
        assert_eq!(guests[0].name.as_deref(), Some("tyr"));
        assert_eq!(guests[1].kind, "lxc");
        assert_eq!(guests[1].vmid, 110);
    }

    #[test]
    fn cluster_resources_skips_rows_missing_node_or_vmid() {
        let raw = sj::json!({
            "data": [
                {"type": "qemu", "node": "", "vmid": 100, "name": "no-node"},
                {"type": "lxc",  "node": "frigg", "vmid": 0, "name": "zero-vmid"},
                {"type": "lxc",  "node": "frigg", "vmid": 110, "name": "baldur"}
            ]
        });
        let resp: ClusterResourcesResponse = sj::from_value(raw).unwrap();
        assert_eq!(resp.guests().len(), 1);
    }

    #[test]
    fn guest_config_extracts_lxc_mac() {
        let raw = sj::json!({
            "data": {
                "hostname": "baldur",
                "net0": "name=eth0,bridge=vmbr0,hwaddr=BC:24:11:F8:0F:AC,ip=dhcp",
                "memory": 4096
            }
        });
        let resp: GuestConfigResponse = sj::from_value(raw).unwrap();
        assert_eq!(resp.macs(), vec!["bc:24:11:f8:0f:ac"]);
    }

    #[test]
    fn guest_config_extracts_multiple_nics() {
        let raw = sj::json!({
            "data": {
                "name": "tyr",
                "net0": "virtio=AA:BB:CC:DD:EE:01,bridge=vmbr0",
                "net1": "virtio=AA:BB:CC:DD:EE:02,bridge=vmbr1"
            }
        });
        let resp: GuestConfigResponse = sj::from_value(raw).unwrap();
        assert_eq!(resp.macs(), vec!["aa:bb:cc:dd:ee:01", "aa:bb:cc:dd:ee:02"]);
    }

    #[test]
    fn guest_config_ignores_non_net_keys() {
        let raw = sj::json!({
            "data": {
                "network": "vmbr0",
                "memory": 4096,
                "cores": 4
            }
        });
        let resp: GuestConfigResponse = sj::from_value(raw).unwrap();
        assert!(resp.macs().is_empty());
    }
}
