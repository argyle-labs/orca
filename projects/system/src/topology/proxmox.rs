//! Proxmox → TopologyClaim collector.
//!
//! Reads pmxcfs (`/etc/pve/qemu-server/*.conf` for VMs, `/etc/pve/lxc/*.conf`
//! for LXCs) directly on the Proxmox host. No API client, no credentials —
//! the cluster filesystem is already mounted and world-readable. Each conf
//! contains `name`/`hostname` plus one or more `netN:` lines with the MAC,
//! which is exactly the join key the inference layer matches against other
//! peers' `interfaces[].mac`.
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
    out.extend(scan_dir("/etc/pve/qemu-server", "vm"));
    out.extend(scan_dir("/etc/pve/lxc", "lxc"));
    Ok(out)
}

fn scan_dir(dir: &str, kind: &str) -> Vec<TopologyClaim> {
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
        if let Some(claim) = parse_conf(id, kind, &content) {
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
}
