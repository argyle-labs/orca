//! Human-readable labels for the canonical system_type, capability, and
//! address-kind tags. Server-owned so every surface (UI, CLI, REST, MCP)
//! renders identical text without re-implementing the switch per client.
//!
//! Pure functions over `&str`. Unknown tags fall back to the input — the
//! UI then renders whatever the host advertised, which is the right
//! behaviour for forward-compat (a peer on a newer release can introduce
//! a new tag and older receivers still display it readably).

/// Human label for a canonical `system_type` tag.
pub fn system_type_label(t: &str) -> String {
    match t {
        "unraid" => "Unraid",
        "proxmox-ve" => "Proxmox VE",
        "proxmox-backup-server" => "Proxmox Backup Server",
        "truenas-scale" => "TrueNAS Scale",
        "truenas-core" => "TrueNAS Core",
        "macos" => "macOS",
        "debian" => "Debian",
        "alpine" => "Alpine",
        "nixos" => "NixOS",
        "linux" => "Linux",
        other => return other.to_string(),
    }
    .to_string()
}

/// Human label for one of the `detected_capabilities` tags.
pub fn capability_label(c: &str) -> String {
    match c {
        "docker" => "Docker",
        "vm-host" => "VM host",
        "lxc-host" => "LXC host",
        "backup-target" => "Backup target",
        "gpu-nvidia" => "NVIDIA GPU",
        "gpu-amd" => "AMD GPU",
        "gpu-intel" => "Intel GPU",
        other => return other.to_string(),
    }
    .to_string()
}

/// Human label for a `PodPeerAddress.kind` / `AddressChannel.kind` tag.
pub fn addr_kind_label(kind: &str) -> String {
    match kind {
        "lan_v4" => "LAN IPv4",
        "lan_v6" => "LAN IPv6",
        "tailscale_v4" => "Tailscale IPv4",
        "tailscale_v6" => "Tailscale IPv6",
        "fqdn" => "FQDN",
        other => return other.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_type_label_covers_every_known_tag() {
        assert_eq!(system_type_label("unraid"), "Unraid");
        assert_eq!(system_type_label("proxmox-ve"), "Proxmox VE");
        assert_eq!(
            system_type_label("proxmox-backup-server"),
            "Proxmox Backup Server"
        );
        assert_eq!(system_type_label("truenas-scale"), "TrueNAS Scale");
        assert_eq!(system_type_label("truenas-core"), "TrueNAS Core");
        assert_eq!(system_type_label("macos"), "macOS");
        assert_eq!(system_type_label("debian"), "Debian");
        assert_eq!(system_type_label("alpine"), "Alpine");
        assert_eq!(system_type_label("nixos"), "NixOS");
        assert_eq!(system_type_label("linux"), "Linux");
    }

    #[test]
    fn system_type_label_unknown_passthrough() {
        assert_eq!(system_type_label("future-os-2030"), "future-os-2030");
        assert_eq!(system_type_label(""), "");
    }

    #[test]
    fn capability_label_covers_every_known_tag() {
        assert_eq!(capability_label("docker"), "Docker");
        assert_eq!(capability_label("vm-host"), "VM host");
        assert_eq!(capability_label("lxc-host"), "LXC host");
        assert_eq!(capability_label("backup-target"), "Backup target");
        assert_eq!(capability_label("gpu-nvidia"), "NVIDIA GPU");
        assert_eq!(capability_label("gpu-amd"), "AMD GPU");
        assert_eq!(capability_label("gpu-intel"), "Intel GPU");
    }

    #[test]
    fn capability_label_unknown_passthrough() {
        assert_eq!(capability_label("zfs"), "zfs");
    }

    #[test]
    fn addr_kind_label_covers_every_known_tag() {
        assert_eq!(addr_kind_label("lan_v4"), "LAN IPv4");
        assert_eq!(addr_kind_label("lan_v6"), "LAN IPv6");
        assert_eq!(addr_kind_label("tailscale_v4"), "Tailscale IPv4");
        assert_eq!(addr_kind_label("tailscale_v6"), "Tailscale IPv6");
        assert_eq!(addr_kind_label("fqdn"), "FQDN");
    }

    #[test]
    fn addr_kind_label_unknown_passthrough() {
        assert_eq!(addr_kind_label("wireguard_v4"), "wireguard_v4");
    }
}
