//! Proxmox → TopologyClaim collector.
//!
//! Reads pmxcfs directly on the Proxmox host. No API client, no credentials —
//! the cluster filesystem is already mounted and world-readable. Each conf
//! contains `name`/`hostname` plus one or more `netN:` lines with the MAC,
//! which is exactly the join key the inference layer matches against other
//! peers' `interfaces[].mac`.
//!
//! Attribution comes from the pmxcfs path itself. The cluster-shared tree
//! `/etc/pve/nodes/<node>/{qemu-server,lxc}/<vmid>.conf` encodes the running
//! node in the path — so a single healthy PVE daemon can register *every*
//! guest in the cluster with an authoritative `runs_on = <node>`, resilient
//! to any one node's daemon/permission gap. The legacy `/etc/pve/qemu-server`
//! and `/etc/pve/lxc` entries are symlinks to the *local* node's dir only, so
//! they're used solely as a fallback when the `nodes/` tree is unavailable
//! (older/standalone installs); there `runs_on` stays `None`.
//!
//! Runs only when `/etc/pve/` exists (the pmxcfs marker also used by
//! `system_info::detect_proxmox_role`). Non-Proxmox hosts return an empty
//! list silently — collector failure must not blank out the snapshot.

use contract::TopologyClaim;

pub async fn collect_all() -> anyhow::Result<Vec<TopologyClaim>> {
    if !std::path::Path::new("/etc/pve").is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();

    // Preferred source: cluster-shared per-node tree. Path carries the node,
    // so every guest gets an authoritative `runs_on` from any single daemon.
    let mut scanned_nodes = false;
    if let Ok(nodes) = std::fs::read_dir("/etc/pve/nodes") {
        for node in nodes.flatten() {
            let Some(node_name) = node.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let base = node.path();
            scanned_nodes = true;
            out.extend(scan_dir(
                &base.join("qemu-server").to_string_lossy(),
                "vm",
                Some(&node_name),
            ));
            out.extend(scan_dir(
                &base.join("lxc").to_string_lossy(),
                "lxc",
                Some(&node_name),
            ));
        }
    }

    // Fallback for standalone/older layouts without a readable `nodes/` tree:
    // the local-node symlinks. No node in the path → `runs_on` stays None and
    // attribution falls back to the reporting peer.
    if !scanned_nodes {
        out.extend(scan_dir("/etc/pve/qemu-server", "vm", None));
        out.extend(scan_dir("/etc/pve/lxc", "lxc", None));
    }

    Ok(out)
}

fn scan_dir(dir: &str, kind: &str, runs_on: Option<&str>) -> Vec<TopologyClaim> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some(fname) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let Some(id) = fname.strip_suffix(".conf") else {
            continue;
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                // EPERM here means the orca daemon isn't in `www-data`
                // (pve confs are mode 640 root:www-data). Silent skip
                // makes the topology tree look empty for no apparent
                // reason — surface it so the install fix is obvious.
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "topology: pve conf unreadable (orca needs www-data group?)"
                );
                continue;
            }
        };
        if let Some(mut claim) = parse_conf(id, kind, &content) {
            claim.runs_on = runs_on.map(str::to_string);
            out.push(claim);
        }
    }
    out
}

/// Parse one `.conf`. Stops at the first snapshot section (`[snap-name]`) so
/// snapshot-frozen state doesn't pollute the live MAC set.
fn parse_conf(id: &str, kind: &str, content: &str) -> Option<TopologyClaim> {
    let mut name = String::new();
    let mut macs = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        if let Some(rest) = line
            .strip_prefix("name:")
            .or_else(|| line.strip_prefix("hostname:"))
        {
            name = rest.trim().to_string();
        } else if line.starts_with("net")
            && line.contains(':')
            && let Some(mac) = extract_mac(line)
        {
            macs.push(mac);
        }
    }
    if macs.is_empty() {
        return None;
    }
    if name.is_empty() {
        name = format!("{kind}-{id}");
    }
    Some(TopologyClaim {
        kind: kind.to_string(),
        id: id.to_string(),
        name,
        macs,
        provider: "proxmox".to_string(),
        provider_instance: "local".to_string(),
        // Set by `scan_dir` from the pmxcfs `nodes/<node>/` path when
        // available; left None on the standalone-symlink fallback.
        runs_on: None,
        ..Default::default()
    })
}

/// Pull a colon-separated MAC out of a Proxmox `netN:` line.
/// Examples:
///   `net0: virtio=02:00:00:00:00:01,bridge=vmbr0`
///   `net0: name=eth0,bridge=vmbr0,hwaddr=02:00:00:00:00:01,ip=dhcp`
fn extract_mac(line: &str) -> Option<String> {
    // Lowercased; the inference layer also lowercases before matching.
    let lower = line.to_lowercase();
    // Walk char-by-char looking for the MAC pattern XX:XX:XX:XX:XX:XX.
    let bytes = lower.as_bytes();
    for start in 0..bytes.len().saturating_sub(17) {
        let win = &lower[start..start + 17];
        if is_mac(win) {
            // Reject if preceded by hex (avoids matching the tail of a longer hex run).
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
    fn vm_conf_with_virtio_mac() {
        let s = "name: charlie\nnet0: virtio=02:00:00:00:00:01,bridge=vmbr0\nmemory: 8192\n";
        let c = parse_conf("100", "vm", s).unwrap();
        assert_eq!(c.name, "charlie");
        assert_eq!(c.macs, vec!["02:00:00:00:00:01"]);
        assert_eq!(c.id, "100");
        assert_eq!(c.kind, "vm");
    }

    #[test]
    fn lxc_conf_with_hwaddr() {
        let s = "hostname: echo\nnet0: name=eth0,bridge=vmbr0,hwaddr=02:00:00:00:00:01,ip=dhcp\n";
        let c = parse_conf("200", "lxc", s).unwrap();
        assert_eq!(c.name, "echo");
        assert_eq!(c.macs, vec!["02:00:00:00:00:01"]);
    }

    #[test]
    fn multiple_nics() {
        let s = "name: r\nnet0: virtio=AA:BB:CC:DD:EE:01,bridge=vmbr0\nnet1: virtio=AA:BB:CC:DD:EE:02,bridge=vmbr1\n";
        let c = parse_conf("1", "vm", s).unwrap();
        assert_eq!(c.macs, vec!["aa:bb:cc:dd:ee:01", "aa:bb:cc:dd:ee:02"]);
    }

    #[test]
    fn snapshot_section_ignored() {
        let s = "name: r\nnet0: virtio=AA:BB:CC:DD:EE:01,bridge=vmbr0\n[snap1]\nnet0: virtio=11:11:11:11:11:11,bridge=vmbr0\n";
        let c = parse_conf("1", "vm", s).unwrap();
        assert_eq!(c.macs, vec!["aa:bb:cc:dd:ee:01"]);
    }

    #[test]
    fn no_net_no_claim() {
        let s = "name: empty\nmemory: 1024\n";
        assert!(parse_conf("1", "vm", s).is_none());
    }

    #[test]
    fn synthetic_name_when_missing() {
        let s = "net0: virtio=AA:BB:CC:DD:EE:01,bridge=vmbr0\n";
        let c = parse_conf("42", "vm", s).unwrap();
        assert_eq!(c.name, "vm-42");
    }

    #[test]
    fn synthetic_name_uses_kind_and_id() {
        let s = "net0: virtio=AA:BB:CC:DD:EE:01,bridge=vmbr0\n";
        let c = parse_conf("7", "lxc", s).unwrap();
        assert_eq!(c.name, "lxc-7");
    }

    #[test]
    fn parse_conf_sets_provider_defaults() {
        let s = "name: x\nnet0: virtio=02:00:00:00:00:01,bridge=vmbr0\n";
        let c = parse_conf("100", "vm", s).unwrap();
        assert_eq!(c.provider, "proxmox");
        assert_eq!(c.provider_instance, "local");
        assert!(c.runs_on.is_none());
    }

    #[test]
    fn empty_content_no_claim() {
        assert!(parse_conf("1", "vm", "").is_none());
    }

    #[test]
    fn net_line_without_mac_yields_no_claim() {
        // A `netN:` line present but carrying no MAC produces no macs → None.
        let s = "name: nm\nnet0: name=eth0,bridge=vmbr0,ip=dhcp\n";
        assert!(parse_conf("1", "vm", s).is_none());
    }

    #[test]
    fn name_wins_over_hostname_by_first_seen() {
        // `hostname:` after `name:` overwrites (both branches assign `name`);
        // last matching line wins.
        let s = "name: first\nhostname: second\nnet0: virtio=02:00:00:00:00:01,bridge=vmbr0\n";
        let c = parse_conf("1", "vm", s).unwrap();
        assert_eq!(c.name, "second");
    }

    #[test]
    fn extract_mac_from_virtio() {
        assert_eq!(
            extract_mac("net0: virtio=02:00:00:00:00:01,bridge=vmbr0"),
            Some("02:00:00:00:00:01".to_string())
        );
    }

    #[test]
    fn extract_mac_lowercases() {
        assert_eq!(
            extract_mac("net0: virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr0"),
            Some("aa:bb:cc:dd:ee:ff".to_string())
        );
    }

    #[test]
    fn extract_mac_none_when_absent() {
        assert!(extract_mac("net0: name=eth0,bridge=vmbr0,ip=dhcp").is_none());
    }

    #[test]
    fn extract_mac_rejects_hex_prefixed_run() {
        // A longer hex run should not have its 17-char tail matched as a MAC.
        // Preceded by a hex digit → skipped.
        assert!(extract_mac("id=ff02:00:00:00:00:00:01").is_none());
    }

    #[test]
    fn is_mac_accepts_canonical() {
        assert!(is_mac("02:00:00:00:00:01"));
        assert!(is_mac("aa:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn is_mac_rejects_wrong_length() {
        assert!(!is_mac("02:00:00:00:00:0"));
        assert!(!is_mac("02:00:00:00:00:001"));
    }

    #[test]
    fn is_mac_rejects_bad_separator() {
        assert!(!is_mac("02-00-00-00-00-01"));
    }

    #[test]
    fn is_mac_rejects_non_hex() {
        assert!(!is_mac("0g:00:00:00:00:01"));
    }

    #[test]
    fn scan_dir_stamps_runs_on_from_node() {
        // A conf read out of a per-node pmxcfs dir must carry the node as
        // authoritative `runs_on`; the fallback (None) must leave it unset.
        let dir = std::env::temp_dir().join("orca-pve-scan-test-node");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("103.conf"),
            "name: opnsense\nnet0: virtio=02:00:00:00:00:aa,bridge=vmbr0\n",
        )
        .unwrap();

        let with_node = scan_dir(&dir.to_string_lossy(), "vm", Some("loki"));
        assert_eq!(with_node.len(), 1);
        assert_eq!(with_node[0].id, "103");
        assert_eq!(with_node[0].runs_on.as_deref(), Some("loki"));

        let fallback = scan_dir(&dir.to_string_lossy(), "vm", None);
        assert_eq!(fallback[0].runs_on, None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
