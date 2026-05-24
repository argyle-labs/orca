# Docker Host Setup: Unraid

Deploy containers directly on an Unraid server using its built-in Docker support.

> **IP Reference**: Replace NFS server and host IPs in the commands below with current values from [Network Map](../../docs/network/network-map.md).

## 1. Enable Docker on Unraid

1. Go to **Settings** > **Docker**
2. Set **Enable Docker** to **Yes**
3. Set **Docker vDisk location** (default is fine, or use a dedicated SSD)
4. Click **Apply**

## 2. Install Portainer

Deploy via Unraid's Community Applications or manually:

### Via Community Applications (Recommended)

1. Go to **Apps** tab
2. Search for **Portainer-CE**
3. Install with defaults
4. Access at `http://<unraid-ip>:9000`

### Via Docker Run

```bash
docker run -d \
  --name portainer \
  --restart always \
  -p 9000:9000 \
  -p 9443:9443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v portainer_data:/data \
  portainer/portainer-ce:latest
```

## 3. Create Docker Network

```bash
docker network create --driver bridge traefik-network
```

## 4. Deploy Compose Stacks via Portainer

Since Unraid doesn't natively support `docker compose` files through its UI, use Portainer:

1. Open Portainer at `http://<unraid-ip>:9000`
2. Go to **Stacks** > **Add stack**
3. Choose **Repository**:
   - Repository URL: your GitHub repo URL
   - Reference: `main`
   - Compose path: `compose/<service>/services/<service>.yml` (or the relevant compose file)
4. Set **Environment variables** manually in the Portainer UI:
   - `NFS_SERVER`, `TZ`, `PIA_USERNAME`, `PIA_PASSWORD`, etc.
5. Click **Deploy the stack**

### Deploying Multiple Compose Files (Downloaders)

For downloaders that need VPN routing, create a stack with multiple compose files:

1. **Stacks** > **Add stack** > **Web editor**
2. Paste the combined contents of `openvpn.yml` and whichever services you want
3. Set environment variables
4. Deploy

Alternatively, use Portainer's **Git Repository** method pointing to `compose/downloaders/services/openvpn.yml` and add additional compose files.

## 5. NFS Storage

Unraid is typically the NFS server itself. If your media is on the same Unraid server:

- Media is at `/mnt/user/data/media` (or your share path)
- Map these paths directly in your compose files or Portainer stack config
- No NFS client setup needed — just use local paths

If media is on a different Unraid server:

```bash
# Install NFS client (Unraid uses Slackware)
# NFS client is built into Unraid - just mount
mkdir -p /mnt/nfs/{media,downloads}

mount -t nfs 10.10.10.10:/mnt/user/data/media /mnt/nfs/media
mount -t nfs 10.10.10.10:/mnt/user/halvor/downloads /mnt/nfs/downloads
```

Add mounts to Unraid's **Go** script (`/boot/config/go`) for persistence across reboots:

```bash
echo 'mount -t nfs 10.10.10.10:/mnt/user/data/media /mnt/nfs/media' >> /boot/config/go
echo 'mount -t nfs 10.10.10.10:/mnt/user/halvor/downloads /mnt/nfs/downloads' >> /boot/config/go
```

## 6. GPU Passthrough (NVIDIA)

Unraid has built-in NVIDIA GPU support:

1. Go to **Settings** > **Docker** > ensure **Docker custom network type** is `macvlan` or `bridge`
2. Install the **Nvidia-Driver** plugin from Community Applications
3. In your container/stack config, add:

```yaml
runtime: nvidia
environment:
  - NVIDIA_VISIBLE_DEVICES=all
```

Or in the Portainer stack, select the NVIDIA runtime.

Verify GPU access:

```bash
docker run --rm --gpus all nvidia/cuda:12.0-base nvidia-smi
```

## Managing Other Docker Hosts from Unraid's Portainer

If you have VMs (Alpine, Fedora) running Docker with Portainer agents:

1. In Portainer: **Settings** > **Environments** > **Add environment** > **Agent**
2. Enter the agent's address: `<vm-ip>:9001`
3. You can now deploy stacks to any host from a single Portainer UI
