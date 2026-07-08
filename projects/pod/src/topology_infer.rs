//! Read-time `parent_peer_id` inference from TopologyClaims.
//!
//! Every host's snapshot carries `interfaces[].mac` plus a `claims` list
//! describing children it runs (VMs, LXCs, containers). The claim is the
//! join key: when peer A claims child C with MAC `aa:bb:cc:…` and peer B
//! reports an interface with that same MAC, B is the child — B's
//! `parent_peer_id` = A.peer_id.
//!
//! Pure function, no IO. Called from `server_pod::list_enriched_impl`
//! after `system` fields are hydrated so the topology tree falls out of
//! a single locally-mirrored read.

use crate::PodPeerDto;
use std::collections::HashMap;

/// Walk `peers` and fill in `system.parent_peer_id` + `parent_kind` for
/// every peer that any other peer claims via a matching MAC. Idempotent —
/// re-running on the same slice produces the same result.
pub fn infer(peers: &mut [PodPeerDto]) {
    // MAC → peer_id index from every peer's reported interfaces. Skip
    // loopback and zero/empty MACs (sysinfo on alpine LXC returns those;
    // the sysfs fallback fills real ones but old snapshots may still leak
    // through). Lowercased so the claim side can match without normalizing.
    let mut mac_owner: HashMap<String, String> = HashMap::new();
    for p in peers.iter() {
        let Some(sys) = &p.system else { continue };
        for iface in &sys.interfaces {
            if iface.loopback {
                continue;
            }
            let Some(mac) = &iface.mac else { continue };
            let key = mac.to_ascii_lowercase();
            if key.is_empty() || key == "00:00:00:00:00:00" {
                continue;
            }
            mac_owner.insert(key, p.peer_id.clone());
        }
    }

    // For each emitter peer, walk its claims; any claim whose MAC matches
    // a child peer's interface MAC pins that child's parent_peer_id to the
    // emitter. First match wins — multiple emitters claiming the same MAC
    // is a misconfiguration; the loop is deterministic over slice order.
    let mut assignments: Vec<(usize, String, String)> = Vec::new();
    for emitter in peers.iter() {
        let Some(sys) = &emitter.system else {
            continue;
        };
        for claim in &sys.claims {
            for mac in &claim.macs {
                let key = mac.to_ascii_lowercase();
                if let Some(child_id) = mac_owner.get(&key)
                    && child_id != &emitter.peer_id
                    && let Some(idx) = peers.iter().position(|p| &p.peer_id == child_id)
                {
                    let kind = parent_kind_for(&claim.kind);
                    assignments.push((idx, emitter.peer_id.clone(), kind.to_string()));
                }
            }
        }
    }

    for (idx, parent_id, kind) in assignments {
        let Some(sys) = peers[idx].system.as_mut() else {
            continue;
        };
        if sys.parent_peer_id.is_none() {
            sys.parent_peer_id = Some(parent_id);
            sys.parent_kind = Some(kind);
        }
    }
}

fn parent_kind_for(claim_kind: &str) -> &'static str {
    match claim_kind {
        "vm" | "lxc" => "hypervisor",
        "container" => "host",
        _ => "host",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::TopologyClaim;
    use system::system_info_types::{NetIfaceDto, SystemInfoReport};

    fn peer(id: &str, mac: Option<&str>, claims: Vec<TopologyClaim>) -> PodPeerDto {
        let sys = SystemInfoReport {
            interfaces: vec![NetIfaceDto {
                name: "eth0".into(),
                mac: mac.map(|m| m.to_string()),
                ipv4: vec![],
                ipv6: vec![],
                loopback: false,
            }],
            claims,
            ..Default::default()
        };
        PodPeerDto {
            peer_id: id.into(),
            hostname: id.into(),
            addr: String::new(),
            port: 0,
            last_seen_at: 0,
            local_secure: false,
            peer_secure: false,
            status: "active".into(),
            addresses: vec![],
            local: false,
            reachable: None,
            latency_ms: None,
            probe_error: None,
            version: None,
            target: None,
            frontend: None,
            mode: None,
            channel: None,
            pinned_to: None,
            update_latest: None,
            update_available: None,
            update_checked_secs: None,
            system: Some(sys),
            pubkey_fp: None,
        }
    }

    fn claim(kind: &str, mac: &str) -> TopologyClaim {
        TopologyClaim {
            kind: kind.into(),
            id: "100".into(),
            name: "child".into(),
            macs: vec![mac.into()],
            provider: "proxmox".into(),
            provider_instance: "p1".into(),
            runs_on: None,
            ..Default::default()
        }
    }

    #[test]
    fn vm_nests_under_hypervisor_via_mac() {
        let mut peers = vec![
            peer(
                "hyp1",
                Some("aa:bb:cc:00:00:01"),
                vec![claim("lxc", "AA:BB:CC:00:00:02")],
            ),
            peer("guest1", Some("aa:bb:cc:00:00:02"), vec![]),
        ];
        infer(&mut peers);
        let child = peers.iter().find(|p| p.peer_id == "guest1").unwrap();
        assert_eq!(
            child.system.as_ref().unwrap().parent_peer_id.as_deref(),
            Some("hyp1")
        );
        assert_eq!(
            child.system.as_ref().unwrap().parent_kind.as_deref(),
            Some("hypervisor")
        );
    }

    #[test]
    fn no_match_leaves_parent_none() {
        let mut peers = vec![
            peer(
                "hyp1",
                Some("aa:bb:cc:00:00:01"),
                vec![claim("vm", "ff:ff:ff:ff:ff:ff")],
            ),
            peer("guest1", Some("aa:bb:cc:00:00:02"), vec![]),
        ];
        infer(&mut peers);
        let child = peers.iter().find(|p| p.peer_id == "guest1").unwrap();
        assert!(child.system.as_ref().unwrap().parent_peer_id.is_none());
    }

    #[test]
    fn self_claim_does_not_self_parent() {
        let mut peers = vec![peer(
            "hyp1",
            Some("aa:bb:cc:00:00:01"),
            vec![claim("vm", "aa:bb:cc:00:00:01")],
        )];
        infer(&mut peers);
        assert!(peers[0].system.as_ref().unwrap().parent_peer_id.is_none());
    }

    #[test]
    fn container_kind_maps_to_host() {
        let mut peers = vec![
            peer(
                "hyp1",
                Some("aa:bb:cc:00:00:01"),
                vec![claim("container", "aa:bb:cc:00:00:02")],
            ),
            peer("svc", Some("aa:bb:cc:00:00:02"), vec![]),
        ];
        infer(&mut peers);
        let svc = peers.iter().find(|p| p.peer_id == "svc").unwrap();
        assert_eq!(
            svc.system.as_ref().unwrap().parent_kind.as_deref(),
            Some("host")
        );
    }
}
