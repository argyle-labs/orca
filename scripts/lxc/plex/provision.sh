#!/usr/bin/env bash
# Plex LXC provisioner — runs inside a fresh Debian 12 container.
# Installs Plex, Intel VAAPI driver, systemd drop-in, and update alias.
#
# Usage (from PVE host):
#   pct push <vmid> provision.sh /root/provision.sh --perms 0755
#   pct exec <vmid> -- /root/provision.sh

set -euo pipefail

PLEX_CLAIM="${PLEX_CLAIM:-}"   # optional: set to claim token from plex.tv/claim

echo "==> Updating base system"
apt-get update -qq
apt-get upgrade -y -qq
apt-get install -y -qq curl gnupg lsb-release vainfo intel-gpu-tools

echo "==> Enabling non-free repos for Intel VAAPI driver"
# Add non-free and non-free-firmware to every deb line that doesn't already have them
sed -i '/^deb /s/\(contrib\)\?\s*$/contrib non-free non-free-firmware/' /etc/apt/sources.list
# Deduplicate repeated words (idempotent re-runs)
sed -i 's/\bcontrib contrib\b/contrib/g; s/\bnon-free non-free\b/non-free/g' /etc/apt/sources.list
apt-get update -qq
apt-get install -y -qq intel-media-va-driver-non-free

echo "==> Adding Plex repo"
curl -fsSL https://downloads.plex.tv/plex-keys/PlexSign.key \
  | gpg --dearmor -o /usr/share/keyrings/plex-archive-keyring.gpg
echo "deb [signed-by=/usr/share/keyrings/plex-archive-keyring.gpg] https://downloads.plex.tv/repo/deb public main" \
  > /etc/apt/sources.list.d/plexmediaserver.list

echo "==> Installing Plex Media Server"
apt-get update -qq

if [[ -n "$PLEX_CLAIM" ]]; then
  PLEX_CLAIM="$PLEX_CLAIM" apt-get install -y plexmediaserver
else
  apt-get install -y plexmediaserver
fi

echo "==> Configuring VAAPI hardware transcoding (systemd drop-in)"
mkdir -p /etc/systemd/system/plexmediaserver.service.d
cat > /etc/systemd/system/plexmediaserver.service.d/vaapi.conf << 'EOF'
[Service]
Environment=LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri
Environment=LIBVA_DRIVER_NAME=iHD
EOF
systemctl daemon-reload

echo "==> Verifying VAAPI"
LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri \
LIBVA_DRIVER_NAME=iHD \
  vainfo 2>&1 | grep -E '(version|Driver version|VAProfile)' | head -6 || true

echo "==> Adding 'update' alias"
cat > /etc/profile.d/update-alias.sh << 'EOF'
alias update='apt-get update && apt-get upgrade -y'
EOF

echo "==> Enabling and starting Plex"
systemctl enable plexmediaserver
systemctl start plexmediaserver

echo ""
echo "==> Done. Plex is running."
echo "    Web UI:  http://$(hostname -I | awk '{print $1}'):32400/web"
echo "    Claim:   visit the web UI and sign in, or re-run with PLEX_CLAIM=<token>"
echo ""
echo "    Recommended next steps:"
echo "    1. Mount media shares (see docs/host-setup/nfs-lxc.md)"
echo "    2. In Plex Settings > Transcoder: enable Hardware Acceleration"
echo "    3. Run 'vainfo' to confirm iHD driver is active"
