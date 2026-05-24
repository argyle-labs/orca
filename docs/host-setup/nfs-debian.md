# NFS Client Setup on Debian/Ubuntu

This guide covers setting up NFS client mounts on Debian-based systems (Debian, Ubuntu, Proxmox VMs) with automatic failover between primary and backup NFS servers.

> **IP Reference**: Replace NFS server IPs in the commands below with current values from [Network Map](../network/network-map.md).

## NFS Server Information

| Server | Role |
|--------|------|
| willow | Primary NFS server |
| maple | Backup NFS server (synced via rclone) |

## Prerequisites

- Debian 11+ or Ubuntu 20.04+
- Network access to both NFS servers
- Root or sudo access

## Installation

```bash
# Update package list
sudo apt-get update

# Install NFS client utilities
sudo apt-get install -y nfs-common
```

## Create Mount Points

```bash
# Create directories for NFS mounts
sudo mkdir -p /mnt/nfs/media
sudo mkdir -p /mnt/nfs/downloads
sudo mkdir -p /mnt/nfs/backups
```

## Manual Mount (Testing)

Test the mounts before making them permanent:

```bash
# Mount media share
sudo mount -t nfs 10.10.10.10:/mnt/user/data/media /mnt/nfs/media

# Mount downloads share
sudo mount -t nfs 10.10.10.10:/mnt/user/halvor/downloads /mnt/nfs/downloads

# Mount backups share
sudo mount -t nfs 10.10.10.10:/mnt/user/halvor/backups /mnt/nfs/backups

# Verify mounts
df -h | grep nfs
```

## Persistent Mounts (fstab)

Add entries to `/etc/fstab` for automatic mounting at boot:

```bash
# Backup existing fstab
sudo cp /etc/fstab /etc/fstab.backup

# Add NFS mount entries
cat << 'EOF' | sudo tee -a /etc/fstab

# NFS Mounts - Primary NFS Server (willow)
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  defaults,_netdev,nofail  0  0
EOF
```

### Mount Options Explained

| Option | Description |
|--------|-------------|
| `defaults` | Use default options (rw, suid, dev, exec, auto, nouser, async) |
| `_netdev` | Wait for network before mounting (essential for NFS) |
| `nofail` | Don't fail boot if mount fails (prevents boot hang) |

### Additional Mount Options (Optional)

For better performance or specific requirements:

```bash
# High-performance options (for media streaming)
10.10.10.10:/mnt/user/data/media  /mnt/nfs/media  nfs  defaults,_netdev,nofail,rsize=1048576,wsize=1048576,hard,timeo=600,retrans=2  0  0

# Soft mount with stale-handle recovery (returns errors instead of hanging; survives Unraid mover)
10.10.10.10:/mnt/user/data/media  /mnt/nfs/media  nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
```

## Apply Mounts

```bash
# Mount all entries from fstab
sudo mount -a

# Verify mounts are active
mount | grep nfs

# Check available space
df -h /mnt/nfs/*
```

## Verify Permissions

```bash
# Check mount permissions
ls -la /mnt/nfs/

# Test write access (if applicable)
touch /mnt/nfs/downloads/test-file && rm /mnt/nfs/downloads/test-file
echo "Write access OK"
```

## Systemd Mount Units (Alternative)

For more control, use systemd mount units instead of fstab:

```bash
# Create mount unit for media
sudo tee /etc/systemd/system/mnt-nfs-media.mount << 'EOF'
[Unit]
Description=NFS Mount - Media
After=network-online.target
Wants=network-online.target

[Mount]
What=10.10.10.10:/mnt/user/data/media
Where=/mnt/nfs/media
Type=nfs
Options=defaults,_netdev,nofail

[Install]
WantedBy=multi-user.target
EOF

# Enable and start mount
sudo systemctl daemon-reload
sudo systemctl enable mnt-nfs-media.mount
sudo systemctl start mnt-nfs-media.mount
```

## Troubleshooting

### Mount Hangs

```bash
# Check if NFS server is reachable
ping -c 3 10.10.10.11

# Check if NFS service is running on server
showmount -e 10.10.10.11

# Check for firewall issues
sudo ufw status
```

### Permission Denied

```bash
# Check NFS exports on server
showmount -e 10.10.10.11

# Verify your IP is allowed in exports
# On NFS server: cat /etc/exports
```

### Stale File Handle

```bash
# Unmount and remount
sudo umount -l /mnt/nfs/media
sudo mount /mnt/nfs/media

# Force unmount if needed
sudo umount -f /mnt/nfs/media
```

### Check NFS Client Status

```bash
# Show NFS mount statistics
nfsstat -m

# Check RPC status
rpcinfo -p 10.10.10.11
```

## Automount with autofs (Optional)

For on-demand mounting:

```bash
# Install autofs
sudo apt-get install -y autofs

# Configure auto.master
echo "/mnt/nfs /etc/auto.nfs --timeout=60" | sudo tee -a /etc/auto.master

# Configure auto.nfs
cat << 'EOF' | sudo tee /etc/auto.nfs
media      -fstype=nfs,rw,soft  10.10.10.10:/mnt/user/data/media
downloads  -fstype=nfs,rw,soft  10.10.10.10:/mnt/user/halvor/downloads
backups    -fstype=nfs,rw,soft  10.10.10.10:/mnt/user/halvor/backups
EOF

# Restart autofs
sudo systemctl restart autofs
sudo systemctl enable autofs
```

## NFS Failover Configuration

For automatic failover between primary (willow/10.10.10.10) and backup (maple/10.10.10.11), see [nfs-failover.md](nfs-failover.md) for all methods (autofs, systemd script, keepalived, manual fstab).

**Recommended — autofs with replicated servers:**

```bash
sudo apt-get install -y autofs
echo "/mnt/nfs /etc/auto.nfs --timeout=300" | sudo tee -a /etc/auto.master
cat << 'EOF' | sudo tee /etc/auto.nfs
media      -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/data/media
downloads  -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/halvor/downloads
backups    -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/halvor/backups
EOF
sudo systemctl enable autofs && sudo systemctl restart autofs
ls /mnt/nfs/media
```

## Docker Integration

After NFS mounts are configured, Docker containers can access them via bind mounts:

```yaml
services:
  sonarr:
    volumes:
      - /mnt/nfs/media:/data/media
      - /mnt/nfs/downloads:/downloads
```

**Note**: When using autofs, ensure the mount is accessed before Docker starts, or use the `x-systemd.automount` option in fstab to ensure mounts are available.

## Related Documentation

- [NFS Setup for Alpine Linux](nfs-alpine.md)
- [NFS Setup for Proxmox LXC](nfs-lxc.md)
- [Docker Compose Deployment](../../compose/README.md)
