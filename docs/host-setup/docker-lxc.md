# Docker in Proxmox LXC Containers

This guide covers running Docker inside Proxmox LXC containers. LXCs are lightweight alternatives to VMs but require special configuration for Docker.

> **IP Reference**: Replace NFS server and host IPs in the commands below with current values from [Network Map](../network/network-map.md).

## Overview

**Pros:**
- Lower resource overhead than VMs
- Faster startup
- Share host kernel (efficient)
- Good for lightweight Docker workloads

**Cons:**
- Requires privileged container (security tradeoff)
- No GPU passthrough (use VMs for GPU workloads)
- Less isolation than VMs
- Some Docker features may not work

**Best for:** Lightweight services, arr stack, downloaders, non-GPU workloads

## Prerequisites

- Proxmox VE 8.0+
- Debian/Ubuntu container template
- Network bridge configured

## Step 1: Create the LXC Container

### Via Proxmox Web UI

1. Click **Create CT**
2. **General**:
   - Hostname: `docker-worker`
   - Password: Set root password
   - **Unprivileged container**: **UNCHECK** (must be privileged)
3. **Template**: Select Debian 12 or Ubuntu 22.04
4. **Disks**:
   - Root disk: 8-20 GB depending on workload
5. **CPU**: 2-4 cores
6. **Memory**: 2048-4096 MB (no swap recommended)
7. **Network**:
   - Bridge: vmbr0
   - IPv4: Static (e.g., 10.10.10.30/24)
   - Gateway: 10.10.10.1
8. **DNS**: Set your DNS servers
9. **Confirm**: Review and create

### Via CLI (pct)

```bash
# Download template if needed
pveam update
pveam download local debian-12-standard_12.2-1_amd64.tar.zst

# Create privileged container
pct create 300 local:vztmpl/debian-12-standard_12.2-1_amd64.tar.zst \
  --hostname docker-worker \
  --cores 2 \
  --memory 2048 \
  --swap 0 \
  --rootfs local-lvm:10 \
  --net0 name=eth0,bridge=vmbr0,ip=10.10.10.30/24,gw=10.10.10.1 \
  --nameserver "10.10.10.201 1.1.1.1" \
  --unprivileged 0 \
  --features nesting=1,keyctl=1 \
  --onboot 1
```

## Step 2: Configure LXC for Docker

Edit the container config on the Proxmox host:

```bash
# Edit container config (replace 300 with your CT ID)
nano /etc/pve/lxc/300.conf
```

Add these lines at the end:

```ini
# Docker support
lxc.apparmor.profile: unconfined
lxc.cgroup2.devices.allow: a
lxc.cap.drop:
lxc.mount.auto: proc:rw sys:rw
```

**What these settings do:**
- `apparmor.profile: unconfined` - Disables AppArmor restrictions
- `cgroup2.devices.allow: a` - Allows access to all devices
- `cap.drop:` - Don't drop any capabilities
- `mount.auto` - Allows proc and sys mounts with write access

## Step 3: Start Container and Install Docker

```bash
# Start the container
pct start 300

# Enter the container
pct enter 300
```

Inside the container:

```bash
# Update system
apt-get update && apt-get upgrade -y

# Install prerequisites
apt-get install -y \
  curl \
  wget \
  gnupg \
  ca-certificates \
  apt-transport-https

# Add Docker repository
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc

echo \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian \
  $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \
  tee /etc/apt/sources.list.d/docker.list > /dev/null

# Install Docker
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Verify Docker works
docker run --rm hello-world
```

## Step 4: Configure Docker

```bash
# Enable Docker to start on boot (Debian/Ubuntu)
systemctl enable docker
systemctl start docker

# Verify Docker is running
docker info
docker ps
```

Containers are managed as standalone Docker hosts. Workloads can be moved between hosts (freyr, mimir, etc.) manually via Portainer or by re-deploying compose stacks — no swarm required.

## Step 5: Set Up NFS Storage

### Option A: Mount NFS Inside Container

```bash
# Install NFS client
apt-get install -y nfs-common

# Create mount points
mkdir -p /mnt/nfs/{media,downloads,backups}

# Add to fstab
cat >> /etc/fstab << 'EOF'
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  defaults,_netdev,nofail  0  0
EOF

# Mount
mount -a
```

### Option B: Bind Mount from Proxmox Host (Recommended)

This is more reliable and performant. On the **Proxmox host**:

```bash
# Mount NFS on the host first
mkdir -p /mnt/nfs/{media,downloads,backups}
mount -t nfs 10.10.10.10:/mnt/user/data/media /mnt/nfs/media
mount -t nfs 10.10.10.10:/mnt/user/halvor/downloads /mnt/nfs/downloads
mount -t nfs 10.10.10.10:/mnt/user/halvor/backups /mnt/nfs/backups

# Add to host's /etc/fstab for persistence
cat >> /etc/fstab << 'EOF'
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  defaults,_netdev,nofail  0  0
EOF
```

Then add bind mounts to the container config (`/etc/pve/lxc/300.conf`):

```ini
# Bind mount NFS from host
mp0: /mnt/nfs/media,mp=/mnt/nfs/media
mp1: /mnt/nfs/downloads,mp=/mnt/nfs/downloads
mp2: /mnt/nfs/backups,mp=/mnt/nfs/backups
```

Restart the container to apply:

```bash
pct stop 300
pct start 300
```

## Step 6: NFS Failover Configuration

For automatic failover between primary (willow/10.10.10.10) and backup (maple/10.10.10.11), configure autofs on the Proxmox host — all containers with bind mounts benefit automatically.

See [nfs-lxc.md](nfs-lxc.md) for the full setup, or [nfs-failover.md](nfs-failover.md) for all failover methods.

## Docker Volume Mapping with NFS

Once NFS is mounted (via any method above), map it into Docker containers:

### docker-compose.yml Example

```yaml
services:
  sonarr:
    image: linuxserver/sonarr
    volumes:
      - /mnt/nfs/media/tv:/tv
      - /mnt/nfs/downloads:/downloads
      - sonarr_config:/config

volumes:
  sonarr_config:
    driver: local
```

### Docker Compose Example

```yaml
services:
  sonarr:
    image: linuxserver/sonarr
    volumes:
      - /mnt/nfs/media/tv:/tv
      - /mnt/nfs/downloads:/downloads
      - sonarr_config:/config

volumes:
  sonarr_config:
    driver: local
```

### Best Practices for Docker + NFS

1. **Path consistency**: Ensure NFS mount paths are identical on all nodes
2. **Permissions**: NFS exports should allow the container's UID/GID (often 1000:1000 for linuxserver images)
3. **Config volumes**: Keep application config in local Docker volumes (better performance)
4. **Media/downloads**: Store these on NFS for sharing across nodes
5. **Database files**: Avoid NFS for databases - use local storage or dedicated database servers

## Full Example: Complete LXC Config

`/etc/pve/lxc/300.conf`:

```ini
arch: amd64
cores: 2
features: nesting=1,keyctl=1
hostname: docker-worker
memory: 2048
nameserver: 10.10.10.201 1.1.1.1
net0: name=eth0,bridge=vmbr0,gw=10.10.10.1,hwaddr=XX:XX:XX:XX:XX:XX,ip=10.10.10.30/24,type=veth
onboot: 1
ostype: debian
rootfs: local-lvm:vm-300-disk-0,size=10G
swap: 0
unprivileged: 0

# Docker support
lxc.apparmor.profile: unconfined
lxc.cgroup2.devices.allow: a
lxc.cap.drop:
lxc.mount.auto: proc:rw sys:rw

# NFS bind mounts from host (optional)
mp0: /mnt/nfs/media,mp=/mnt/nfs/media
mp1: /mnt/nfs/downloads,mp=/mnt/nfs/downloads
mp2: /mnt/nfs/backups,mp=/mnt/nfs/backups
```

## Alpine Linux in LXC

For even lighter containers, use Alpine:

```bash
# Download Alpine template
pveam download local alpine-3.19-default_20240207_amd64.tar.xz

# Create container
pct create 301 local:vztmpl/alpine-3.19-default_20240207_amd64.tar.xz \
  --hostname docker-alpine \
  --cores 2 \
  --memory 1024 \
  --rootfs local-lvm:4 \
  --net0 name=eth0,bridge=vmbr0,ip=10.10.10.31/24,gw=10.10.10.1 \
  --unprivileged 0 \
  --features nesting=1,keyctl=1
```

Add Docker support to config, then inside the container:

```bash
# Install Docker
apk update
apk add docker docker-cli-compose

# Enable and start
rc-update add docker default
service docker start

# Docker is ready for standalone deployments
```

## Troubleshooting

### Docker Won't Start

```bash
# Check logs
journalctl -u docker -f

# Common fix: ensure cgroup v2 is working
cat /proc/cgroups

# Check container features
pct config 300 | grep features
# Should show: nesting=1,keyctl=1
```

### "Operation not permitted" Errors

Container needs to be privileged with proper config:

```bash
# Verify on Proxmox host
pct config 300 | grep unprivileged
# Should show: unprivileged: 0

# Check apparmor setting in config
grep apparmor /etc/pve/lxc/300.conf
```

### Docker Network / Module Issues

```bash
# Inside container, load required kernel modules
modprobe overlay
modprobe br_netfilter

# If modules fail, load them on the Proxmox host instead
# On host:
modprobe overlay
modprobe br_netfilter
```

### Container Won't Start After Config Change

```bash
# Check for config errors
pct config 300

# Check Proxmox logs
journalctl -u pve-container@300

# Try starting with verbose output
pct start 300 --debug
```

### NFS Bind Mount Issues

```bash
# Ensure NFS is mounted on host first
df -h | grep nfs

# Check mount point permissions
ls -la /mnt/nfs/

# Verify bind mount syntax in config
grep "mp[0-9]" /etc/pve/lxc/300.conf
```

## Security Considerations

Running Docker in privileged LXC containers has security implications:

1. **Container escape risk**: A compromised container could potentially access the host
2. **No namespace isolation**: Shares kernel with host
3. **Full device access**: Has access to all host devices

**Mitigations:**
- Keep Proxmox and container packages updated
- Use firewall rules to limit network access
- Don't run untrusted images
- Consider VMs for sensitive workloads

## When to Use LXC vs VM

| Use Case | Recommendation |
|----------|----------------|
| GPU workloads | VM (LXC can't do GPU passthrough) |
| Lightweight services (arr stack) | LXC |
| High-security requirements | VM |
| Resource-constrained host | LXC |
| Quick testing/development | LXC |
| Production media servers | VM |

## Related Documentation

- [Docker Setup on Debian](docker-debian.md) - Standard Docker installation
- [Docker Setup on Alpine](docker-alpine.md) - Lightweight Docker host
- [NFS in LXC](nfs-lxc.md) - Advanced NFS configuration
- [NFS Failover Methods](nfs-failover.md) - Autofs, systemd, keepalived
- [NFS Proxmox Cluster](../network/NFS-PROXMOX-CLUSTER.md) - NFS host mounts (loki/thor)
