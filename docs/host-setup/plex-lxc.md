# Plex LXC — Debian 12 Template

Reusable Proxmox CT template for Plex Media Server with Intel Quick Sync / VAAPI hardware transcoding.

Works on any Proxmox host with an Intel iGPU exposing `/dev/dri/renderD128`.

## Why Debian 12

Ubuntu 24.04's `intel-media-va-driver-non-free 24.1.0` is compiled against glibc 2.39
and uses C23 symbols (`__isoc23_strtoul`, `__isoc23_fscanf`, etc.) that Plex's
musl-compiled Transcoder binary cannot resolve. Debian 12 (Bookworm) ships driver
`23.4.x` built against glibc 2.36 — no C23 symbols, no shim required.

## Prerequisites

On the Proxmox host:

```sh
# Download Debian 12 template (if not already present)
pveam update
pveam download local debian-12-standard_12.7-1_amd64.tar.zst
```

Verify `/dev/dri/renderD128` exists on the host:

```sh
ls -la /dev/dri/
```

## Create the container

```sh
# Minimal — uses all defaults (storage=local-lvm, bridge=vmbr0, 4GB RAM, 4 cores, 32G disk)
VMID=110 HOSTNAME=mimir ./scripts/lxc/plex/create.sh

# Custom storage/bridge
VMID=114 HOSTNAME=njord STORAGE=local-zfs BRIDGE=vmbr1 ./scripts/lxc/plex/create.sh
```

The script creates the CT unprivileged with `nesting=1` and attaches
`/dev/dri/renderD128` via `dev0` (gid=44, video group).

## Provision Plex

```sh
VMID=110  # or 114 for njord

# Optional: grab a claim token from https://plex.tv/claim (expires in 4 min)
# export PLEX_CLAIM=claim-xxxxxxxxxxxx

pct push $VMID scripts/lxc/plex/provision.sh /root/provision.sh --perms 0755
pct exec $VMID -- /root/provision.sh
```

The provisioner:

1. Installs `intel-media-va-driver-non-free` from Debian non-free
2. Adds the official Plex apt repo and installs `plexmediaserver`
3. Writes a systemd drop-in (`/etc/systemd/system/plexmediaserver.service.d/vaapi.conf`)
   exporting `LIBVA_DRIVERS_PATH` and `LIBVA_DRIVER_NAME=iHD` into every Plex process
4. Runs `vainfo` to confirm the iHD driver initialises cleanly
5. Adds `alias update='apt-get update && apt-get upgrade -y'` to `/etc/profile.d/`

## Mount media (NFS)

See [nfs-lxc.md](nfs-lxc.md) for the standard NFS bind-mount pattern.
Add mounts to the CT conf before starting, or inside the container in `/etc/fstab`.

## Plex settings after first boot

1. Open `http://<ct-ip>:32400/web`
2. Settings → Transcoder → **Enable Hardware-Accelerated Encoding** ✓
3. Settings → Transcoder → **Use Hardware-Accelerated Video Encoding** ✓

Verify hardware is active from inside the container:

```sh
vainfo
# expect: Intel iHD driver ... VAProfileH264 / VAProfileHEVC entrypoints
```

## `update` alias

All containers provisioned with this script have:

```sh
alias update='apt-get update && apt-get upgrade -y'
```

Source it in the current shell with `. /etc/profile.d/update-alias.sh` or open a new login shell.

## Existing containers (Ubuntu 24.04)

If migrating an existing Ubuntu 24.04 Plex LXC rather than rebuilding from scratch,
a C23 symbol shim is required. See the shim source at `scripts/lxc/plex/ubuntu-vaapi-shim/`.
