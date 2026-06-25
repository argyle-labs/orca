#!/usr/bin/env bash
# Installs the C23 glibc symbol shim for Plex VAAPI on Ubuntu 24.04 LXCs.
# Must be run on the Proxmox HOST (not inside the container).
#
# Usage:
#   VMID=110 ./install.sh

set -euo pipefail

: "${VMID:?VMID required}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "==> Building shim on host"
gcc -shared -fPIC -nostdlib -nodefaultlibs \
  -o /tmp/plex-vaapi-shim.so "$SCRIPT_DIR/shim.c"

echo "==> Pushing shim into CT $VMID"
pct push "$VMID" /tmp/plex-vaapi-shim.so /usr/local/lib/plex-vaapi-shim.so --perms 0644

echo "==> Writing systemd drop-in"
pct exec "$VMID" -- bash -c 'mkdir -p /etc/systemd/system/plexmediaserver.service.d'
pct exec "$VMID" -- bash -c 'cat > /etc/systemd/system/plexmediaserver.service.d/vaapi.conf << EOF
[Service]
Environment=LD_PRELOAD=/usr/local/lib/plex-vaapi-shim.so
Environment=LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri
Environment=LIBVA_DRIVER_NAME=iHD
EOF'

echo "==> Reloading and restarting Plex"
pct exec "$VMID" -- systemctl daemon-reload
pct exec "$VMID" -- systemctl restart plexmediaserver

echo "==> Verifying VAAPI inside CT $VMID"
pct exec "$VMID" -- bash -c \
  'LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri LIBVA_DRIVER_NAME=iHD \
   LD_PRELOAD=/usr/local/lib/plex-vaapi-shim.so \
   vainfo 2>&1 | grep -E "(version|Driver|VAProfileH264Main)" | head -5'

echo "==> Done."
