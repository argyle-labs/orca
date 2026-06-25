#!/usr/bin/env bash
# Proxmox host script — creates a Plex LXC from a Debian 12 template.
# Run on the PVE host as root.
#
# Usage:
#   VMID=110 HOSTNAME=mimir STORAGE=local-lvm BRIDGE=vmbr0 ./create.sh
#
# Required env:
#   VMID      — CT ID (e.g. 110)
#   HOSTNAME  — container hostname (e.g. mimir, njord)
#
# Optional env (defaults shown):
#   STORAGE   — rootfs storage pool  (default: local-lvm)
#   BRIDGE    — network bridge        (default: vmbr0)
#   MEMORY    — RAM in MB             (default: 4096)
#   CORES     — vCPU count            (default: 4)
#   DISK      — rootfs size           (default: 32G)
#   TEMPLATE  — Debian 12 template path on PVE host
#               (default: /var/lib/vz/template/cache/debian-12-standard_12.7-1_amd64.tar.zst)

set -euo pipefail

: "${VMID:?VMID required}"
: "${HOSTNAME:?HOSTNAME required}"
STORAGE="${STORAGE:-local-lvm}"
BRIDGE="${BRIDGE:-vmbr0}"
MEMORY="${MEMORY:-4096}"
CORES="${CORES:-4}"
DISK="${DISK:-32G}"
TEMPLATE="${TEMPLATE:-/var/lib/vz/template/cache/debian-12-standard_12.7-1_amd64.tar.zst}"

if [[ ! -f "$TEMPLATE" ]]; then
  echo "Template not found: $TEMPLATE"
  echo "Download with: pveam download local debian-12-standard_12.7-1_amd64.tar.zst"
  exit 1
fi

echo "==> Creating LXC $VMID ($HOSTNAME)"
pct create "$VMID" "$TEMPLATE" \
  --hostname "$HOSTNAME" \
  --memory "$MEMORY" \
  --cores "$CORES" \
  --rootfs "${STORAGE}:${DISK}" \
  --net0 "name=eth0,bridge=${BRIDGE},ip=dhcp" \
  --unprivileged 0 \
  --features "nesting=1" \
  --ostype debian \
  --start 0

echo "==> Configuring GPU passthrough (/dev/dri/renderD128)"
pct set "$VMID" --dev0 /dev/dri/renderD128,gid=44

echo "==> Starting container"
pct start "$VMID"
sleep 3

echo "==> Container $VMID running. Next step:"
echo "    Copy provision.sh into the container and run it:"
echo "    pct push $VMID $(dirname "$0")/provision.sh /root/provision.sh --perms 0755"
echo "    pct exec $VMID -- /root/provision.sh"
