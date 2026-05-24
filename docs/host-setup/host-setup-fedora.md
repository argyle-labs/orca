# Docker Host Setup: Fedora VM (NVIDIA GPU)

Set up a Fedora VM as a Docker host with NVIDIA GPU passthrough. Best for media servers that need hardware transcoding (Jellyfin, Plex).

> **IP Reference**: Replace NFS server and host IPs in the commands below with current values from [Network Map](../../docs/network/network-map.md).

## 1. Create the VM with GPU Passthrough

### Proxmox

#### Enable IOMMU on the Proxmox Host

Edit `/etc/default/grub`:

```bash
# Intel CPU
GRUB_CMDLINE_LINUX_DEFAULT="quiet intel_iommu=on iommu=pt"

# AMD CPU
GRUB_CMDLINE_LINUX_DEFAULT="quiet amd_iommu=on iommu=pt"
```

```bash
update-grub
reboot
```

Verify IOMMU is enabled:

```bash
dmesg | grep -e DMAR -e IOMMU
```

#### Blacklist GPU Drivers on the Host

Prevent the Proxmox host from using the GPU:

```bash
cat >> /etc/modprobe.d/blacklist.conf << 'EOF'
blacklist nouveau
blacklist nvidia
blacklist nvidiafb
blacklist nvidia_drm
EOF

update-initramfs -u
reboot
```

#### Find the GPU's IOMMU Group

```bash
# List IOMMU groups
for d in /sys/kernel/iommu_groups/*/devices/*; do
  n=${d#*/iommu_groups/*}; n=${n%%/*}
  printf 'IOMMU Group %s: ' "$n"
  lspci -nns "${d##*/}"
done | grep -i nvidia
```

Note the PCI IDs (e.g., `10de:xxxx`).

#### Create the VM

1. **Create VM** in Proxmox UI:
   - OS: Fedora Server ISO
   - System: OVMF (UEFI), Machine: q35
   - Disks: 32+ GB (VirtIO Block)
   - CPU: host type, 4+ cores
   - Memory: 8192+ MB
   - Network: VirtIO, bridge vmbr0
2. **Add PCI device**: VM > Hardware > Add > PCI Device
   - Select the NVIDIA GPU
   - Check: **All Functions**, **Primary GPU** (if needed), **PCI-Express**
3. Start VM and install Fedora

### Unraid

1. Download Fedora Server ISO to `/mnt/user/isos/`
2. **VMs** > **Add VM** > **Linux**
   - Machine: Q35
   - BIOS: OVMF
   - CPU, RAM as above
   - Graphics Card: select your NVIDIA GPU
   - USB Devices: select GPU audio device if present
3. Start VM, install Fedora via VNC

## 2. Install NVIDIA Drivers

After Fedora is installed and booted:

```bash
# Update system
sudo dnf update -y

# Enable RPM Fusion (required for NVIDIA drivers)
sudo dnf install -y \
  https://download1.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm \
  https://download1.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm

# Install NVIDIA drivers
sudo dnf install -y akmod-nvidia xorg-x11-drv-nvidia-cuda

# Wait for kernel module to build (can take a few minutes)
sudo akmods --force
sudo dracut --force

# Reboot
sudo reboot
```

Verify the GPU is detected:

```bash
nvidia-smi
```

## 3. Install Docker

```bash
# Install Docker
sudo dnf -y install dnf-plugins-core
sudo dnf-3 config-manager --add-repo https://download.docker.com/linux/fedora/docker-ce.repo
sudo dnf install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Enable and start Docker
sudo systemctl enable --now docker

# Add your user to docker group
sudo usermod -aG docker $USER
newgrp docker

# Verify
docker run --rm hello-world
```

## 4. Install NVIDIA Container Toolkit

This lets Docker containers access the GPU:

```bash
# Add NVIDIA container toolkit repo
curl -s -L https://nvidia.github.io/libnvidia-container/stable/rpm/nvidia-container-toolkit.repo | \
  sudo tee /etc/yum.repos.d/nvidia-container-toolkit.repo

# Install
sudo dnf install -y nvidia-container-toolkit

# Configure Docker to use the NVIDIA runtime
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker

# Verify GPU is accessible from containers
docker run --rm --gpus all nvidia/cuda:12.0-base nvidia-smi
```

## 5. Install Portainer Agent

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

## 6. Set Up NFS Storage

```bash
sudo dnf install -y nfs-utils

sudo mkdir -p /mnt/nfs/{media,downloads}

sudo bash -c 'cat >> /etc/fstab << EOF
10.10.10.10:/mnt/user/data/media       /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
EOF'

sudo mount -a
df -h | grep nfs
```

## 7. Create Docker Network

```bash
docker network create --driver bridge traefik-network
```

## 8. Deploy Services with GPU

When deploying services that need the GPU, add the NVIDIA runtime:

```yaml
# In your compose file
services:
  jellyfin:
    image: lscr.io/linuxserver/jellyfin:latest
    runtime: nvidia
    environment:
      - NVIDIA_VISIBLE_DEVICES=all
    # ... rest of config
```

Or use Intel QuickSync if the host has an Intel iGPU:

```yaml
services:
  jellyfin:
    image: lscr.io/linuxserver/jellyfin:latest
    devices:
      - /dev/dri:/dev/dri
    # ... rest of config
```

## Troubleshooting

### nvidia-smi Shows No GPU

```bash
# Check if module is loaded
lsmod | grep nvidia

# Rebuild kernel module
sudo akmods --force
sudo reboot
```

### Docker Can't Access GPU

```bash
# Verify NVIDIA runtime is configured
docker info | grep -i nvidia

# Re-run toolkit configuration
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
```

### GPU Not Passed Through (Proxmox)

```bash
# On Proxmox host, verify IOMMU
dmesg | grep -e DMAR -e IOMMU

# Check VFIO binding
lspci -nnk | grep -A 3 nvidia
# Should show: Kernel driver in use: vfio-pci
```
