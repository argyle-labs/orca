#!/bin/bash
# Deploy orca-failover artifacts on tyr.
# Idempotent. Run as root on tyr after scp'ing the dist-tyr/ directory.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)

install -m 0755 "$HERE/orca-failover"            /usr/local/sbin/orca-failover
install -d -m 0755                                /etc/orca
[ -f /etc/orca/failover.conf ] || \
  install -m 0644 "$HERE/failover.conf"           /etc/orca/failover.conf
install -m 0644 "$HERE/orca-failover.service"     /etc/systemd/system/orca-failover.service
install -m 0644 "$HERE/orca-failover.timer"       /etc/systemd/system/orca-failover.timer
install -d -m 0755                                /var/lib/orca-failover

systemctl daemon-reload
# DO NOT auto-enable the timer. The mount-swap model is architecturally invalid
# for NFSv4 re-export gateways — swapping tyr's upstream invalidates downstream
# clients' cached file handles (ESTALE storm). See:
#   memory/project_orca_failover_nfsv4_stale_handle.md (POSTMORTEM 2026-06-09)
# The artifact stays installed for `pin` / `unpin` / manual `swap` operations,
# which are useful and don't trigger the broken auto-failback cycle. The probe
# loop must remain disabled until a real HA model is chosen (DRBD / CephFS /
# GlusterFS / app-layer failover — see project_orca_unified_shares Phase 3).
systemctl disable orca-failover.timer 2>/dev/null || true
echo "installed; timer left DISABLED (see install.sh comment). Manual ops:"
echo "  orca-failover status | pin <share> <host> | unpin <share> | swap <share> <host>"
/usr/local/sbin/orca-failover status
