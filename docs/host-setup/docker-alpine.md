# Docker Setup on Alpine Linux

This guide covers installing Docker on Alpine Linux, an ultra-lightweight distribution (~50MB base) ideal for minimal Docker hosts.

> **IP Reference**: Replace NFS server and host IPs in the commands below with current values from [Network Map](../network/network-map.md).

## Why Alpine?

- **Tiny footprint**: ~50MB base install vs ~1GB for Debian
- **Fast boot**: Boots in seconds
- **Low memory**: Uses ~30MB RAM at idle
- **Security-focused**: Uses musl libc and busybox
- **Perfect for**: Dedicated container hosts, edge devices, resource-constrained environments

## Prerequisites

- Alpine Linux 3.18+ installed
- Root access
- Network connectivity

## Step 1: Enable Community Repository

Docker packages are in the `community` repository, which is disabled by default on Alpine.

```bash
# Enable community repository
sed -i 's|#http://dl-cdn.alpinelinux.org/alpine/v[0-9.]*/community|http://dl-cdn.alpinelinux.org/alpine/v3.21/community|' /etc/apk/repositories

# Or manually edit /etc/apk/repositories and uncomment the community line
```

## Step 2: Update System

```bash
apk update && apk upgrade
```

## Step 3: Install Docker

```bash
# Install Docker, Docker Compose, and OpenRC init scripts
# Note: docker-openrc is a PACKAGE (not a command) that provides service scripts
apk add docker docker-cli-compose docker-openrc

# Enable Docker to start on boot
rc-update add docker default

# Start Docker now
service docker start
```

> **Note**: The `docker-openrc` package is required for OpenRC service management. It's a package name, not a command. After installing it, use `service docker start` and `rc-update add docker default` to manage Docker. Without the `docker-openrc` package, the `service docker start` and `rc-update add docker` commands will fail.

## Step 4: Add User to Docker Group (Optional)

```bash
# Add your user to docker group
addgroup $USER docker

# Log out and back in for changes to take effect
exit
```

> **Note**: Alpine doesn't include `newgrp` by default. You must log out and back in for group changes to apply. Alternatively, install it with `apk add shadow` if needed.

## Step 5: Verify Installation

```bash
# Check Docker version
docker --version
docker compose version

# Test Docker
docker run --rm hello-world
```

## Step 6: Configure Docker Daemon (Optional)

Create `/etc/docker/daemon.json`:

```bash
cat > /etc/docker/daemon.json << 'EOF'
{
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "10m",
    "max-file": "3"
  },
  "storage-driver": "overlay2"
}
EOF

# Restart Docker to apply
service docker restart
```

## Firewall Configuration

If using Alpine's iptables firewall:

```bash
# Install iptables if not present
apk add iptables

# Allow Docker API access (if needed for remote management)
# Note: Only enable if you need remote Docker access and have proper security
# iptables -A INPUT -p tcp --dport 2376 -j ACCEPT  # Docker TLS
# iptables -A INPUT -p tcp --dport 2375 -j ACCEPT  # Docker (insecure, not recommended)

# Save rules
rc-update add iptables
/etc/init.d/iptables save
```

## NFS Setup for Shared Storage

```bash
# Install NFS utilities
apk add nfs-utils

# Enable required services
rc-update add rpcbind
rc-update add nfsmount
service rpcbind start

# Create mount points
mkdir -p /mnt/nfs/{media,downloads,backups}

# Add to /etc/fstab
cat >> /etc/fstab << 'EOF'
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  defaults,_netdev,nofail  0  0
EOF

# Mount all
mount -a

# Verify
df -h | grep nfs
```

See [nfs-alpine.md](nfs-alpine.md) for advanced NFS configuration with failover.

## Enable IP Forwarding (For Overlay Networks)

```bash
# Enable IP forwarding
echo "net.ipv4.ip_forward = 1" >> /etc/sysctl.conf
sysctl -p

# Persist across reboots
rc-update add sysctl
```

## Alpine-Specific Considerations

### Package Management

```bash
# Search for packages
apk search docker

# Install a package
apk add <package>

# Remove a package
apk del <package>

# List installed packages
apk list --installed
```

### Service Management (OpenRC)

Alpine uses OpenRC, not systemd:

```bash
# Start a service
service docker start

# Stop a service
service docker stop

# Restart a service
service docker restart

# Check service status
service docker status

# Enable service at boot
rc-update add docker default

# Disable service at boot
rc-update del docker default

# List enabled services
rc-update show
```

### Shell Differences

Alpine uses `ash` (busybox shell) by default:

```bash
# Install bash if needed
apk add bash

# Change default shell
chsh -s /bin/bash $USER
```

### Missing Commands

Some common utilities need to be installed:

```bash
# Common utilities
apk add curl wget vim htop

# Network tools
apk add iputils bind-tools

# Build tools (if compiling)
apk add build-base
```

## Troubleshooting

### Docker Won't Start

```bash
# Check logs
cat /var/log/docker.log

# Check service status
service docker status

# Try starting manually for verbose output
dockerd --debug
```

### cgroup Issues

Alpine may need cgroup configuration:

```bash
# Install cgroup tools
apk add cgroup-tools

# Enable cgroup services
rc-update add cgroups
service cgroups start
```

### DNS Issues in Containers

```bash
# Edit daemon.json
cat > /etc/docker/daemon.json << 'EOF'
{
  "dns": ["8.8.8.8", "8.8.4.4"]
}
EOF

service docker restart
```

### Overlay Network Issues

```bash
# Ensure ip_vs modules are loaded
modprobe ip_vs
modprobe ip_vs_rr
modprobe ip_vs_wrr
modprobe ip_vs_sh

# Make persistent
cat > /etc/modules-load.d/ipvs.conf << 'EOF'
ip_vs
ip_vs_rr
ip_vs_wrr
ip_vs_sh
EOF
```

## Resource Comparison

| Metric | Alpine | Debian 12 |
|--------|--------|-----------|
| Base install | ~50 MB | ~1 GB |
| RAM at idle | ~30 MB | ~150 MB |
| Boot time | ~3 sec | ~15 sec |
| Package manager | apk | apt |
| Init system | OpenRC | systemd |
| C library | musl | glibc |

## When to Use Alpine vs Debian

**Use Alpine when:**
- Minimal resource usage is critical
- Running on edge/IoT devices
- Simple container host with no extra services
- You're comfortable with OpenRC and ash shell

**Use Debian when:**
- Need glibc compatibility (some software requires it)
- Require systemd features
- More familiar with apt ecosystem
- Running complex services beyond Docker

## Related Documentation

- [Privilege Escalation (sudo)](sudo-alpine.md) - Set up doas for non-root users
- [NFS Setup on Alpine](nfs-alpine.md) - Shared storage configuration
- [NFS Failover Methods](nfs-failover.md) - Autofs, systemd, keepalived
- [NFS Proxmox Cluster](../network/NFS-PROXMOX-CLUSTER.md) - NFS host mounts (loki/thor)
