# NFS Client Setup on Proxmox LXC Containers

This guide covers setting up NFS client mounts in Proxmox LXC containers with automatic failover between primary and backup NFS servers. LXC containers require special configuration to access NFS shares.

> **IP Reference**: Replace NFS server IPs in the commands below with current values from [Network Map](../network/network-map.md).

## NFS Server Information

| Server | Role |
|--------|------|
| willow | Primary NFS server |
| maple | Backup NFS server (synced via rclone) |

## Prerequisites

- Proxmox VE 7.0+
- LXC container (privileged or unprivileged)
- Network access to both NFS servers
- Root access to Proxmox host and LXC container

## Method 1: Proxmox Bind Mounts (Recommended)

The recommended approach is to mount NFS on the Proxmox host and bind-mount into containers.

### Step 1: Mount NFS on Proxmox Host

```bash
# On Proxmox host
# Install NFS client
apt-get update && apt-get install -y nfs-common

# Create mount points on host
mkdir -p /mnt/nfs/media
mkdir -p /mnt/nfs/downloads
mkdir -p /mnt/nfs/backups

# Add to /etc/fstab on Proxmox host
cat << 'EOF' >> /etc/fstab

# NFS Mounts - Primary NFS Server (willow)
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
EOF

# Mount all
mount -a
```

### Step 2: Add Bind Mounts to LXC Container

Edit the container configuration on the Proxmox host:

```bash
# Edit container config (replace 100 with your container ID)
nano /etc/pve/lxc/100.conf
```

Add bind mount entries:

```ini
# Bind mounts from Proxmox host to container
mp0: /mnt/nfs/media,mp=/mnt/nfs/media
mp1: /mnt/nfs/downloads,mp=/mnt/nfs/downloads
mp2: /mnt/nfs/backups,mp=/mnt/nfs/backups
```

### Step 3: Restart Container

```bash
# Stop and start container to apply changes
pct stop 100
pct start 100

# Verify mounts inside container
pct enter 100
df -h | grep nfs
```

### Using Proxmox Web UI

1. Go to **Datacenter** → **Container** → **Resources**
2. Click **Add** → **Mount Point**
3. Configure:
   - **Storage**: Select "Directory" or leave blank for bind mount
   - **Path on Host**: `/mnt/nfs/media`
   - **Path in Container**: `/mnt/nfs/media`
   - **Backup**: Unchecked (NFS data shouldn't be backed up by Proxmox)

## Method 2: Direct NFS Mount in Container (Privileged Only)

For privileged containers, you can mount NFS directly inside the container.

### Prerequisites

The container must be privileged and have the `nfs` feature enabled:

```bash
# On Proxmox host, edit container config
nano /etc/pve/lxc/100.conf
```

Add these options:

```ini
# Enable NFS in container
features: nesting=1,mount=nfs
unprivileged: 0
```

### For Debian/Ubuntu LXC

```bash
# Inside the container
apt-get update && apt-get install -y nfs-common

# Create mount points
mkdir -p /mnt/nfs/media /mnt/nfs/downloads /mnt/nfs/backups

# Add to fstab
cat << 'EOF' >> /etc/fstab
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
EOF

# Mount
mount -a
```

### For Alpine LXC

```bash
# Inside the container
apk add nfs-utils

# Enable services
rc-update add rpcbind
rc-update add nfsmount
rc-service rpcbind start

# Create mount points
mkdir -p /mnt/nfs/media /mnt/nfs/downloads /mnt/nfs/backups

# Add to fstab
cat << 'EOF' >> /etc/fstab
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
EOF

# Mount
mount -a
```

## Method 3: Proxmox Storage with NFS

Configure NFS as a Proxmox storage backend, then use it for container storage.

### Step 1: Add NFS Storage in Proxmox

Via Web UI:
1. **Datacenter** → **Storage** → **Add** → **NFS**
2. Configure:
   - **ID**: `nfs-media`
   - **Server**: `10.10.10.11`
   - **Export**: `/mnt/user/data/media`
   - **Content**: `images,rootdir` (or as needed)

Via CLI:

```bash
pvesm add nfs nfs-media \
  --server 10.10.10.11 \
  --export /mnt/user/data/media \
  --content images,rootdir
```

### Step 2: Use in Container Config

```bash
# Add as mount point using storage ID
mp0: nfs-media:subvol-100-disk-0,mp=/mnt/nfs/media,size=1000G
```

## Unprivileged Container Considerations

Unprivileged containers cannot mount NFS directly. Use Method 1 (bind mounts) instead.

### UID/GID Mapping

For unprivileged containers with bind mounts, you may need to handle UID mapping:

```bash
# On Proxmox host, check container's UID map
grep -E "^lxc.idmap" /etc/pve/lxc/100.conf

# Example output:
# lxc.idmap: u 0 100000 65536
# lxc.idmap: g 0 100000 65536
```

If files on NFS have UID 65534 (nobody), you may need to:

1. Use privileged container
2. Add specific UID/GID mapping
3. Change NFS export options

### UID Mapping Example

```ini
# In /etc/pve/lxc/100.conf
# Map host UID 65534 to container UID 65534
lxc.idmap: u 0 100000 65534
lxc.idmap: g 0 100000 65534
lxc.idmap: u 65534 65534 1
lxc.idmap: g 65534 65534 1
lxc.idmap: u 65535 165535 1
lxc.idmap: g 65535 165535 1
```

## Troubleshooting

### Mount Point Not Visible in Container

```bash
# Check if mount exists on host
df -h /mnt/nfs/media

# Verify container config
cat /etc/pve/lxc/100.conf | grep mp

# Restart container
pct stop 100 && pct start 100
```

### Permission Denied in Container

```bash
# Check ownership on host
ls -la /mnt/nfs/

# For unprivileged containers, check UID mapping
# Files might appear as 'nobody' inside container
```

### "Operation Not Permitted" for NFS Mount

```bash
# Ensure container has nfs feature enabled
grep features /etc/pve/lxc/100.conf

# Should include: features: nesting=1,mount=nfs

# Container must be privileged for direct NFS
grep unprivileged /etc/pve/lxc/100.conf
# Should be: unprivileged: 0
```

### Mounts Not Available After Host Reboot

```bash
# Ensure NFS mounts on host have _netdev option
grep nfs /etc/fstab

# Check mount order - NFS should mount before containers start
systemctl list-dependencies pve-container@100.service
```

### Slow Performance

```bash
# Check NFS version being used
nfsstat -m

# Try NFSv4.2 explicitly (vers=4.2 supported by Willow/Maple)
mount -t nfs -o vers=4.2,soft,softreval,nconnect=4 10.10.10.10:/mnt/user/data/media /mnt/nfs/media

# Add version to fstab
10.10.10.10:/mnt/user/data/media  /mnt/nfs/media  nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  0  0
```

## Complete LXC Configuration Example

Here's a complete example for an Alpine LXC container with NFS access:

```ini
# /etc/pve/lxc/100.conf

arch: amd64
cores: 2
memory: 1024
swap: 512
hostname: traefik
net0: name=eth0,bridge=vmbr0,firewall=1,gw=10.10.10.1,hwaddr=XX:XX:XX:XX:XX:XX,ip=10.10.10.13/24,type=veth
ostype: alpine
rootfs: local-lvm:vm-100-disk-0,size=8G
unprivileged: 0
features: nesting=1,mount=nfs

# Bind mounts from Proxmox host
mp0: /mnt/nfs/media,mp=/mnt/nfs/media
mp1: /mnt/nfs/downloads,mp=/mnt/nfs/downloads
mp2: /mnt/nfs/backups,mp=/mnt/nfs/backups

# Optional: Resource limits
lxc.cgroup2.memory.max: 1073741824
```

## NFS Failover Configuration

For LXC containers, configure failover on the **Proxmox host** — all containers with bind mounts automatically benefit.

See [nfs-failover.md](nfs-failover.md) for all methods. For this host (Debian), use Method 1 (autofs) or Method 2 (systemd script).

**Recommended — autofs on Proxmox host:**

```bash
apt-get install -y autofs
echo "/mnt/nfs /etc/auto.nfs --timeout=300" >> /etc/auto.master
cat << 'EOF' > /etc/auto.nfs
media      -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/data/media
downloads  -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/halvor/downloads
backups    -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/halvor/backups
EOF
systemctl enable autofs && systemctl restart autofs
ls /mnt/nfs/media
```

Container bind mounts remain unchanged:

```ini
mp0: /mnt/nfs/media,mp=/mnt/nfs/media
mp1: /mnt/nfs/downloads,mp=/mnt/nfs/downloads
mp2: /mnt/nfs/backups,mp=/mnt/nfs/backups
```

---

## Related Documentation

- [NFS Setup for Debian/Ubuntu](nfs-debian.md)
- [NFS Setup for Alpine Linux](nfs-alpine.md)
- [Docker in LXC](docker-lxc.md) - Running Docker in LXC containers
- [Traefik Setup](../services/traefik.md)
- [Docker Compose Deployment](../compose/README.md)
