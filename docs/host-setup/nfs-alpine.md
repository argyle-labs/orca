# NFS Client Setup on Alpine Linux

This guide covers setting up NFS client mounts on Alpine Linux (VMs or containers) with automatic failover between primary and backup NFS servers.

> **IP Reference**: Replace NFS server IPs in the commands below with current values from [Network Map](../network/network-map.md).

## NFS Server Information

| Server | Role |
|--------|------|
| willow | Primary NFS server |
| maple | Backup NFS server |

## Prerequisites

- Alpine Linux 3.18+
- Network access to both NFS servers
- Root access

## Installation

```bash
apk update
apk add nfs-utils autofs autofs-openrc
```

## Create Mount Points

```bash
mkdir -p /mnt/willow/data
mkdir -p /mnt/willow/downloads
mkdir -p /mnt/willow/backups
```

## Manual Mount (Testing)

Test the mounts before making them permanent:

```bash
mount -t nfs 10.10.10.10:/mnt/user/data      /mnt/willow/data
mount -t nfs 10.10.10.10:/mnt/user/downloads /mnt/willow/downloads
mount -t nfs 10.10.10.10:/mnt/user/backups   /mnt/willow/backups

df -h | grep willow
```

## Persistent Mounts — Autofs with Failover (Recommended)

Autofs mounts shares on-demand and automatically fails over to Maple if Willow is unreachable (~5–30 sec). Handles boot ordering without `_netdev` or custom init scripts.

```bash
# Add autofs master entry
echo "/mnt/willow /etc/autofs/auto.willow --timeout=300 --ghost" >> /etc/autofs/auto.master

# Create the willow map
# - data + backups: Willow primary, Maple failover. Syncthing keeps these in
#   sync (sendreceive). Read+write both work against either server.
# - downloads: Willow primary, Maple writable failover. Maple's downloads
#   share is NOT Syncthing-replicated — it's an empty rw export used only
#   when Willow is unreachable. Writes against Maple-downloads are accepted.
cat > /etc/autofs/auto.willow << 'EOF'
data       -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/data
downloads  -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/downloads
backups    -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/backups
EOF

rc-update add autofs default
rc-service autofs start

# Trigger mounts and verify
ls /mnt/willow/data && ls /mnt/willow/downloads && ls /mnt/willow/backups
mount | grep willow
```

> See [nfs-failover.md](nfs-failover.md) for full failover method comparison.

## Persistent Mounts — Static fstab (Legacy / Simple Setups)

Use this only when autofs is not available or failover is not needed.

```bash
cp /etc/fstab /etc/fstab.backup

cat << 'EOF' >> /etc/fstab

# NFS Mounts - Willow NAS (10.10.10.10)
10.10.10.10:/mnt/user/data       /mnt/willow/data       nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/downloads  /mnt/willow/downloads  nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
10.10.10.10:/mnt/user/backups    /mnt/willow/backups    nfs  _netdev,nofail,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30,x-systemd.mount-timeout=60  0  0
EOF

mount -a
```

Enable boot-time mounting via OpenRC:

```bash
rc-update add netmount default
```

### Mount Options Explained

| Option | Description |
|--------|-------------|
| `vers=4.2` | Pin NFSv4.2 — sessions (4.1+) for state recovery, 4.2 for best attribute caching. Willow/Maple both advertise `+4.2`. |
| `_netdev` | Wait for network before mounting |
| `nofail` | Don't fail boot if mount fails |
| `soft` | Return an error on timeout instead of hanging indefinitely |
| `softreval` | On ESTALE, serve cached attributes briefly while the client re-resolves the path. **This is what survives Unraid mover-induced stale handles** without the error propagating to apps (Plex/Jellyfin scans, *arrs, downloaders). |
| `timeo=50` | Major timeout 5s (tenths of a second). |
| `retrans=2` | Retry 2 times before giving up. |
| `nconnect=4` | Open 4 parallel TCP streams to the NFS server. One stalled stream can't block the others, so mover pauses don't take down the whole mount. Honored once per server — the first mount of a given server sets it for all subsequent mounts of the same server. |
| `actimeo=30` | Cap attribute cache at 30s so the client notices mover-driven inode changes faster. |
| `x-systemd.mount-timeout=60` | Don't let systemd hang for 90s on a hard mount at boot. |

> **Why these options?** With the default `hard` mount, a stale NFS handle causes Docker container start to hang indefinitely. `soft,softreval,timeo=50,retrans=2` makes a stale handle fail fast (~10s) and lets the client transparently re-resolve via name lookup instead of returning ESTALE to apps. `nconnect=4` keeps a single mover-stalled stream from taking down the whole mount.

## Verify Permissions

```bash
ls -la /mnt/willow/

# Test write access
touch /mnt/willow/halvor/downloads/test-file && rm /mnt/willow/halvor/downloads/test-file && echo "Write access OK"
```

### NFS Root Squash

Willow exports with root squash enabled. Both `root` and container users (uid 1000) are mapped to `nobody` on the NFS side. Directories must be owned by `nobody:users` with at least `775` on the Willow side for containers to write:

```bash
# Run on Willow (10.10.10.10)
chown nobody:users /mnt/user/downloads/completed
chown nobody:users /mnt/user/downloads/incomplete
chmod 775 /mnt/user/downloads/completed
chmod 775 /mnt/user/downloads/incomplete
```

## Troubleshooting

### Mount Fails with "Network Unreachable"

```bash
# Check network status
ip addr show
ping -c 3 10.10.10.10

# Ensure rpcbind is running
rc-service rpcbind status
rc-service rpcbind start
```

### "Permission Denied" Error

```bash
# Check NFS exports from server
showmount -e 10.10.10.10

# Verify your IP is in the allowed list
```

### Stale File Handle

```bash
# Lazy unmount and remount
umount -l /mnt/nfs/media
mount /mnt/nfs/media
```

### Services Not Starting at Boot

```bash
# Verify services are enabled
rc-update show default | grep -E "rpcbind|nfsmount"

# Re-add if missing
rc-update add rpcbind boot
rc-update add nfsmount default
```

### Check RPC Status

```bash
# Show RPC info
rpcinfo -p 10.10.10.10

# Check local RPC
rpcinfo -p localhost
```

## Alpine-Specific Notes

### Minimal Installation

For minimal Alpine installations (e.g., Docker host), you may need additional packages:

```bash
# Full NFS client support
apk add nfs-utils rpcbind

# Or minimal (client only)
apk add nfs-utils
```

### BusyBox vs Full Utilities

Alpine uses BusyBox by default. For full `mount.nfs` features:

```bash
# Install full mount utilities if needed
apk add util-linux
```

### Disk-based vs Diskless Systems

For diskless Alpine (running from RAM):

```bash
# Save changes to persist across reboots
lbu commit -d

# Or add to lbu include list
lbu include /etc/fstab
lbu commit -d
```

## Docker Integration

After NFS mounts are configured, Docker containers can access them:

```bash
# Install Docker
apk add docker docker-compose
rc-update add docker default
rc-service docker start

# Use NFS paths in containers
docker run -v /mnt/nfs/media:/data alpine ls /data
```

## Related Documentation

- [NFS Setup for Debian/Ubuntu](nfs-debian.md)
- [NFS Setup for Proxmox LXC](nfs-lxc.md)
