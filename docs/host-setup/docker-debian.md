# Docker Setup on Debian

This guide covers installing Docker on a Debian-based system (Debian 12 Bookworm, Ubuntu 22.04+) for standalone Docker Compose deployments.

## Prerequisites

- Debian 12 (Bookworm) or Ubuntu 22.04+ installed
- Root or sudo access
- Network connectivity to other nodes (for NFS, Tailscale, etc.)

## Step 1: Update System

```bash
sudo apt-get update && sudo apt-get upgrade -y
```

## Step 2: Install Required Packages

```bash
sudo apt-get install -y \
  curl \
  wget \
  gnupg \
  ca-certificates \
  apt-transport-https \
  software-properties-common \
  nfs-common
```

## Step 3: Install Docker

### Add Docker Repository

```bash
# Add Docker's official GPG key
sudo install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg | sudo tee /etc/apt/keyrings/docker.asc > /dev/null
sudo chmod a+r /etc/apt/keyrings/docker.asc

# Add the repository to apt sources
echo \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian \
  $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \
  sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
```

For Ubuntu, replace `debian` with `ubuntu` in the URLs above.

### Install Docker Engine

```bash
sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
```

### Add Your User to Docker Group (Optional)

This allows running Docker commands without sudo:

```bash
sudo usermod -aG docker $USER

# Apply group changes (or log out and back in)
newgrp docker
```

### Verify Installation

```bash
docker --version
docker compose version
docker run hello-world
```

## Step 4: Configure Docker Daemon (Optional)

Create or edit `/etc/docker/daemon.json` for custom configuration:

```bash
sudo mkdir -p /etc/docker
sudo tee /etc/docker/daemon.json > /dev/null << 'EOF'
{
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "10m",
    "max-file": "3"
  },
  "storage-driver": "overlay2"
}
EOF

sudo systemctl restart docker
```

## Step 5: Enable Docker to Start on Boot

```bash
sudo systemctl enable docker
sudo systemctl enable containerd
```

## Firewall Configuration

If using UFW, allow Docker API access (if needed for remote management):

```bash
# SSH (if needed)
sudo ufw allow 22/tcp

# Docker API ports (only if you need remote Docker access)
# Note: Only enable if you need remote Docker access and have proper security
# sudo ufw allow 2376/tcp   # Docker TLS (secure)
# sudo ufw allow 2375/tcp   # Docker (insecure, not recommended)

# Apply rules
sudo ufw reload
```

## Verify Docker is Running

```bash
# Check Docker service status
sudo systemctl status docker

# Check Docker info
docker info

# Test container run
docker run --rm alpine echo "Docker is working!"
```

## Troubleshooting

### Docker Service Won't Start

```bash
# Check logs
sudo journalctl -u docker -f

# Check for configuration errors
sudo dockerd --validate

# Reset Docker (caution: removes all containers/images)
sudo systemctl stop docker
sudo rm -rf /var/lib/docker
sudo systemctl start docker
```

### Permission Denied Errors

```bash
# Ensure user is in docker group
groups $USER

# If docker group missing, re-add
sudo usermod -aG docker $USER
newgrp docker
```

### DNS Resolution Issues in Containers

```bash
# Edit daemon.json to specify DNS
sudo tee /etc/docker/daemon.json > /dev/null << 'EOF'
{
  "dns": ["8.8.8.8", "8.8.4.4"]
}
EOF

sudo systemctl restart docker
```

## Next Steps

- [Set up NFS storage](nfs-debian.md) - Configure shared storage
- [NFS Failover Methods](nfs-failover.md) - Autofs, systemd, keepalived
- [NFS Proxmox Cluster](../network/NFS-PROXMOX-CLUSTER.md) - NFS host mounts (loki/thor)
- [Docker Compose Stacks](../compose/README.md) - Deploy services
