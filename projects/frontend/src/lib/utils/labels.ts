export function addrKindLabel(kind: string): string {
  switch (kind) {
    case 'lan_v4':
      return 'LAN IPv4';
    case 'lan_v6':
      return 'LAN IPv6';
    case 'tailscale_v4':
      return 'Tailscale IPv4';
    case 'tailscale_v6':
      return 'Tailscale IPv6';
    case 'fqdn':
      return 'FQDN';
    default:
      return kind;
  }
}

export function systemTypeLabel(t: string): string {
  switch (t) {
    case 'unraid':
      return 'Unraid';
    case 'proxmox-ve':
      return 'Proxmox VE';
    case 'proxmox-backup-server':
      return 'Proxmox Backup Server';
    case 'truenas-scale':
      return 'TrueNAS Scale';
    case 'truenas-core':
      return 'TrueNAS Core';
    case 'macos':
      return 'macOS';
    case 'debian':
      return 'Debian';
    case 'alpine':
      return 'Alpine';
    case 'nixos':
      return 'NixOS';
    case 'linux':
      return 'Linux';
    default:
      return t;
  }
}

export function capabilityLabel(c: string): string {
  switch (c) {
    case 'docker':
      return 'Docker';
    case 'vm-host':
      return 'VM host';
    case 'lxc-host':
      return 'LXC host';
    case 'backup-target':
      return 'Backup target';
    case 'gpu-nvidia':
      return 'NVIDIA GPU';
    case 'gpu-amd':
      return 'AMD GPU';
    case 'gpu-intel':
      return 'Intel GPU';
    default:
      return c;
  }
}
