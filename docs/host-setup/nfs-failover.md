# NFS Failover Configuration

Automatic failover between primary (willow/10.10.10.10) and backup (maple/10.10.10.11) NFS servers.

## nfs-monitor — stale-handle watchdog (all hosts)

The script `scripts/nfs-monitor.sh` runs every minute on every NFS-consuming host. It detects stale handles (timeout `ls` + write probe), remounts via lazy-umount + remount, verifies stability with two probes spaced 2s apart, then restarts only the consumers that actually bind-mount the affected path. Behavior is driven by `/etc/nfs-monitor.conf`.

| Host | OS | Mounts monitored | RESTART_STRATEGY | Consumers | Conf in repo |
|------|----|------------------|------------------|-----------|---------------|
| freyr (10.10.10.15) | Alpine | data, downloads, backups (autofs) | `docker` | sonarr, sabnzbd, radarr, radarr-4k, qbittorrent, prowlarr, bazarr, lidarr, kapowarr, mylar3, lazylibrarian | [scripts/freyr/nfs-monitor.conf](../../scripts/freyr/nfs-monitor.conf) |
| baldur (10.10.10.6) | Alpine | data, backups | `docker` | immich-server, immich-machine-learning, immich-postgres, audiobookshelf, calibre-web, kavita, komga, navidrome | [scripts/baldur/nfs-monitor.conf](../../scripts/baldur/nfs-monitor.conf) |
| thor (10.10.10.8) | Proxmox VE | data, downloads, backups | `pct` | LXCs 101, 104, 107, 108, 109, 110 (auto-filtered to those with matching `mp[N]:` bind-mounts) | [scripts/thor/nfs-monitor.conf](../../scripts/thor/nfs-monitor.conf) |
| frigg (10.10.10.7) | Proxmox VE | data, backups | `pct` | LXCs 113 (jellyfin), 114 (njord) | [scripts/frigg/nfs-monitor.conf](../../scripts/frigg/nfs-monitor.conf) |
| pbs (10.10.10.17) | Debian (PBS) | /mnt/backups, /mnt/pbs | `systemctl` | proxmox-backup, proxmox-backup-proxy | [scripts/pbs/nfs-monitor.conf](../../scripts/pbs/nfs-monitor.conf) |

**Strategies:**
- `docker` — restart only running containers whose bind-mount sources start with the affected path
- `pct` — for each LXC ID in `PCT_CONTAINERS`, parse `/etc/pve/lxc/<id>.conf` for `mp[N]:` entries; restart only those whose source begins under the affected path. If the LXC is not in `running` state, it's skipped.
- `systemctl` — restart all listed units when any monitored mount recovers (no per-mount filtering)
- `none` — remount only; consumers self-recover

**Scheduler:**
- freyr: root crontab — `* * * * * /usr/local/bin/nfs-monitor.sh`
- baldur: `/etc/crontabs/svc` (busybox crond, owned by user `svc`) — `* * * * * doas /usr/local/bin/nfs-monitor.sh`. Note: `crontab -l` for root is empty by design; the active schedule is in `/etc/crontabs/`.
- thor / frigg: root crontab — `* * * * * /usr/local/bin/nfs-monitor.sh`
- pbs: `/etc/cron.d/nfs-monitor`

**Dry-run** any host before going live:
```bash
DRY_RUN=1 /usr/local/bin/nfs-monitor.sh
```

**Logs:** `/var/log/nfs-monitor.log` on every host.

### Built-in willow→maple failover (Proxmox hosts)

thor, frigg, and loki were migrated to autofs (dual-server replicated entries) on 2026-05-21. Static fstab NFS entries are commented out on all three. The `nfs-monitor.sh` failover/failback logic below remains active as a belt-and-suspenders watchdog but the primary failover path is now autofs.

**Autofs config on each Proxmox host:**
- `/etc/auto.master`: `/mnt/willow  /etc/autofs/auto.willow  --timeout=0 --ghost`
  > `--timeout=0` (never expire) is required on Proxmox hosts — LXC bind mounts go stale when autofs expires the underlying NFS, and LXCs cannot re-bind without a container restart. freyr/baldur use `--timeout=300` because their Docker containers access mounts continuously.
- `/etc/autofs/auto.willow` — thor:
  ```
  data      -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/data
  downloads -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/downloads
  backups   -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/backups
  ```
- frigg: same but `data` and `backups` only (no downloads share)
- loki: `data`, `halvor` (willow-only — not replicated to maple), `backups`

> **TODO**: rename mount point from `/mnt/willow` to `/mnt/nas` across all hosts and LXC bind-mount configs — tracked separately. The name `/mnt/willow` is misleading during maple failover, but autofs failover is transparent regardless of directory name.

`nfs-monitor.sh` continues to handle the secondary failover case and LXC consumer restarts. Set in `/etc/nfs-monitor.conf`:

```sh
NFS_PRIMARY="10.10.10.10"     # willow
NFS_FAILOVER="10.10.10.11"    # maple — empty disables the feature
```

Behavior:

- **Failover.** When `check_mount` flags a path stale and a TCP probe to `NFS_PRIMARY:2049` fails, the recovery path skips fstab/autofs (which would block on NFS hard-mount) and remounts directly from `NFS_FAILOVER`, deriving the export path and options from the fstab line (stripping `_netdev`, `nofail`, `x-systemd.*`). Bind-mount LXC consumers are restarted via `restart_pct` so they pick up the new source inode.
- **Failback.** A pre-pass at the top of each run checks every monitored mount; if `findmnt` shows the source is the failover host and the primary is reachable again, the mount is switched back to the primary and consumers are restarted again. If the failback `mount` fails, the failover mount is restored and the script tries again next cycle.

Validated end-to-end on thor (2026-05-14) by blocking `10.10.10.10:2049` with nftables on the host: `/mnt/willow/downloads` switched to maple in ~18s, switched back in ~6s after the block was removed.

Caveats:
- `check_mount` cannot detect "should be mounted but isn't" — if the underlying NFS mount disappears entirely (e.g. boot-time mount failure with `nofail`), this script will not notice. Eager fstab mounts must succeed at boot, or the mount unit must be explicitly retried. The frigg incident on 2026-05-14 was this case.
- Failover/failback each restart the LXC consumers; expect ~20–30s of downtime per transition. This is acceptable for media servers but may be disruptive for stateful services.
- This is a shell implementation kept minimal — per the orca/Rust migration plan, the long-term home for this logic is the orca daemon.

---

## Failover replication model

**Replication model:**
- `data` and `backups` — Syncthing sendreceive between Willow and Maple. Either server is fully usable.
- `downloads` — NOT Syncthing-replicated. Maple's downloads share is an empty rw NFS export used only as a failover target. While freyr is failed over, contents diverge from Willow; SAB/qBit and the *arrs detect missing files and retry/error, which is acceptable. Failback is automatic when Willow returns; apps reconcile on their own. Maple's `/boot/config/shares/downloads.cfg` must have `shareExportNFS="e"` and `shareHostListNFS="*(rw,sync,no_subtree_check,no_root_squash)"`.

> **LXC Containers**: Configure failover on the **Proxmox host** (Debian). LXC containers with bind mounts automatically benefit — no per-container configuration needed. See [nfs-lxc.md](nfs-lxc.md).

## Method 1: Autofs with Replicated Servers (Recommended)

Autofs supports replicated servers natively — tries the primary first and falls back to the backup automatically.

### Debian/Ubuntu (systemd)

```bash
sudo apt-get install -y autofs

echo "/mnt/nfs /etc/auto.nfs --timeout=300" | sudo tee -a /etc/auto.master

cat << 'EOF' | sudo tee /etc/auto.nfs
media      -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/data/media
# downloads: Willow primary, Maple writable failover (Maple downloads is NOT Syncthing-replicated)
downloads  -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/downloads
backups    -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/backups
EOF

sudo systemctl enable autofs
sudo systemctl restart autofs

# Test — access triggers the mount
ls /mnt/nfs/media
```

### Alpine Linux (OpenRC)

```bash
apk add autofs

echo "/mnt/nfs /etc/auto.nfs --timeout=300" >> /etc/auto.master

cat > /etc/auto.nfs << 'EOF'
media      -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/data/media
# downloads: Willow primary, Maple writable failover (Maple downloads is NOT Syncthing-replicated)
downloads  -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/downloads
backups    -fstype=nfs,rw,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30  10.10.10.10,10.10.10.11:/mnt/user/backups
EOF

rc-update add autofs default
rc-service autofs start

ls /mnt/nfs/media
```

**How it works:**
- Autofs tries primary (10.10.10.10) first; if unreachable, falls back to backup (10.10.10.11)
- Mounts are on-demand and unmounted after 300s of inactivity
- Failover and failback are automatic (~5–30 sec)

## Method 2: Systemd Failover Script (Debian/Ubuntu)

A systemd service that polls every 30 seconds and switches mounts when the primary fails.

```bash
sudo tee /usr/local/bin/nfs-failover.sh << 'EOF'
#!/bin/bash
PRIMARY_NFS="10.10.10.10"
BACKUP_NFS="10.10.10.11"
STATE_FILE="/var/run/nfs-failover-state"
LOG_FILE="/var/log/nfs-failover.log"

declare -A MOUNTS=(
    ["/mnt/nfs/media"]="/mnt/user/data/media"
    ["/mnt/nfs/downloads"]="/mnt/user/downloads"
    ["/mnt/nfs/backups"]="/mnt/user/backups"
)

log() { echo "$(date '+%Y-%m-%d %H:%M:%S') - $1" | tee -a "$LOG_FILE"; }

check_server() { timeout 5 showmount -e "$1" &>/dev/null; }

get_current() { [[ -f "$STATE_FILE" ]] && cat "$STATE_FILE" || echo "$PRIMARY_NFS"; }

remount_all() {
    local server=$1
    log "Switching NFS to $server"
    for mp in "${!MOUNTS[@]}"; do
        umount -l "$mp" 2>/dev/null
        mount -t nfs -o _netdev,vers=4.2,soft,softreval,timeo=50,retrans=2,nconnect=4,actimeo=30 \
            "$server:${MOUNTS[$mp]}" "$mp" \
            && log "Mounted $mp" || log "ERROR: Failed to mount $mp"
    done
    echo "$server" > "$STATE_FILE"
}

current=$(get_current)
if check_server "$PRIMARY_NFS"; then
    [[ "$current" != "$PRIMARY_NFS" ]] && remount_all "$PRIMARY_NFS" && log "Failed back to primary"
elif check_server "$BACKUP_NFS"; then
    [[ "$current" != "$BACKUP_NFS" ]] && remount_all "$BACKUP_NFS" && log "Failed over to backup"
else
    log "ERROR: Both NFS servers are unreachable!"
fi
EOF

sudo chmod +x /usr/local/bin/nfs-failover.sh
```

```bash
sudo tee /etc/systemd/system/nfs-failover.service << 'EOF'
[Unit]
Description=NFS Failover Check
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/nfs-failover.sh
EOF

sudo tee /etc/systemd/system/nfs-failover.timer << 'EOF'
[Unit]
Description=NFS Failover Monitor Timer

[Timer]
OnBootSec=30
OnUnitActiveSec=30

[Install]
WantedBy=timers.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable nfs-failover.timer
sudo systemctl start nfs-failover.timer
```

## Method 3: Keepalived Floating IP (Advanced)

A floating VIP (10.10.10.100) moves between NFS servers. Clients mount the VIP and never need reconfiguration.

**On both NFS servers**, install keepalived:

```bash
sudo apt-get install -y keepalived
```

**Primary server (willow)** — `/etc/keepalived/keepalived.conf`:

```
vrrp_script check_nfs {
    script "/usr/bin/systemctl is-active nfs-server"
    interval 2
    weight 2
}

vrrp_instance NFS_VIP {
    state MASTER
    interface eth0
    virtual_router_id 51
    priority 101
    advert_int 1

    authentication {
        auth_type PASS
        auth_pass nfs_secret
    }

    virtual_ipaddress {
        10.10.10.100/24
    }

    track_script {
        check_nfs
    }
}
```

**Backup server (maple)** — same config, but `state BACKUP` and `priority 100`.

**On clients**, mount using the floating IP:

```
10.10.10.100:/mnt/user/data/media  /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
10.10.10.100:/mnt/user/downloads   /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
10.10.10.100:/mnt/user/backups     /mnt/nfs/backups    nfs  defaults,_netdev,nofail  0  0
```

## Method 4: Manual fstab Failover

Keep two fstab entries (one commented) for manual switching:

```
# Primary active
10.10.10.10:/mnt/user/data/media      /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0
#10.10.10.11:/mnt/user/data/media     /mnt/nfs/media      nfs  defaults,_netdev,nofail  0  0

10.10.10.10:/mnt/user/halvor/downloads  /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0
#10.10.10.11:/mnt/user/halvor/downloads /mnt/nfs/downloads  nfs  defaults,_netdev,nofail  0  0

10.10.10.10:/mnt/user/halvor/backups    /mnt/nfs/backups    nfs  defaults,_netdev,nofail  0  0
#10.10.10.11:/mnt/user/halvor/backups   /mnt/nfs/backups    nfs  defaults,_netdev,nofail  0  0
```

To fail over to backup manually:

```bash
sudo umount /mnt/nfs/*
sudo sed -i 's/^10.10.10.10/#10.10.10.10/g; s/^#10.10.10.11/10.10.10.11/g' /etc/fstab
sudo mount -a
```

## Comparison

| Method | Failover | Failback | Complexity | Downtime |
|--------|----------|----------|------------|----------|
| Autofs replicated | Automatic | Automatic | Low | ~5–30 sec |
| Systemd script | Automatic | Automatic | Medium | ~30–60 sec |
| Keepalived (floating IP) | Automatic | Automatic | High | ~2–5 sec |
| Manual fstab | Manual | Manual | None | Minutes |

**Recommendation**: **Autofs with replicated servers** — simple, no extra services, handles failover and failback automatically.

## Related Documentation

- [NFS Setup on Alpine](nfs-alpine.md)
- [NFS Setup on Debian/Ubuntu](nfs-debian.md)
- [NFS in Proxmox LXC](nfs-lxc.md)
