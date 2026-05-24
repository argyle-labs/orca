# Docker Host Setup: Alpine Linux VM

Set up an Alpine Linux VM as a lightweight Docker host. Best for CPU-only workloads (no GPU).

> **IP Reference**: Replace NFS server and host IPs in the commands below with current values from [Network Map](../../docs/network/network-map.md).

## 1. Create the VM

### Proxmox

1. Download Alpine ISO: **local** > **ISO Images** > **Download from URL**
   - URL: `https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/alpine-virt-3.21.0-x86_64.iso`
2. **Create VM**:
   - OS: Select the Alpine ISO
   - System: BIOS (SeaBIOS), SCSI controller: VirtIO SCSI
   - Disks: 8-20 GB (VirtIO Block)
   - CPU: 2-4 cores
   - Memory: 1024-4096 MB
   - Network: VirtIO, bridge vmbr0
3. Start VM, open console, run `setup-alpine`
   - Keyboard: `us`
   - Hostname: your choice (e.g., `docker-alpine`)
   - Network: static IP on your LAN
   - Root password: set one
   - Disk: `sda`, `sys` mode
   - Reboot and remove ISO

### Unraid

1. Download Alpine ISO to `/mnt/user/isos/`
2. **VMs** > **Add VM** > **Linux**
   - Name, CPU cores, RAM as above
   - Primary vDisk: 8-20 GB
   - OS Install ISO: select Alpine ISO
   - Network: virtio, br0
3. Start VM, open VNC console, run `setup-alpine` as above

## 2. Enable Community Repository

```bash
sed -i 's|#http://dl-cdn.alpinelinux.org/alpine/v[0-9.]*/community|http://dl-cdn.alpinelinux.org/alpine/v3.21/community|' /etc/apk/repositories
apk update && apk upgrade
```

## 3. Install Docker

```bash
apk add docker docker-cli-compose docker-openrc curl wget bash

# Enable and start Docker
rc-update add docker default
service docker start

# Verify
docker run --rm hello-world
```

## 4. Install Portainer Agent

Portainer manages containers via a web UI. Install the agent so the central Portainer server can manage this host.

```bash
docker run -d \
  --name portainer_agent \
  --restart always \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /var/lib/docker/volumes:/var/lib/docker/volumes \
  -p 9001:9001 \
  portainer/agent:latest
```

Then in the Portainer web UI: **Settings** > **Environments** > **Add environment** > **Agent** > enter `<vm-ip>:9001`.

## 5. Set Up NFS Storage

```bash
apk add nfs-utils
rc-update add rpcbind
rc-update add nfsmount
service rpcbind start

mkdir -p /mnt/nfs/{media,downloads}

cat >> /etc/fstab << 'EOF'
10.10.10.10:/mnt/user/data/media       /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
EOF

mount -a
df -h | grep nfs
```

See [nfs-alpine.md](nfs-alpine.md) for advanced NFS configuration.

## 6. Create Docker Network

```bash
docker network create --driver bridge traefik-network
```

## 7. Deploy Services

Clone the repo and deploy compose files through Portainer (paste compose YAML + set environment variables), or directly:

```bash
cd compose/media
docker compose -f services/jellyfin.yml up -d
```

## Optional: Configure Docker Daemon

```bash
cat > /etc/docker/daemon.json << 'EOF'
{
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "10m",
    "max-file": "3"
  }
}
EOF

service docker restart
```

## Optional: IP Forwarding

Needed if services require overlay networks:

```bash
echo "net.ipv4.ip_forward = 1" >> /etc/sysctl.conf
sysctl -p
```
