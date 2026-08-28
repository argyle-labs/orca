# Storage serving-side follow-ups

Status: **BACKLOG** — findings from the 2026-08-27 `data` NFS→SMB cutover
incident. Concrete work items under roadmap §1.5 (storage server side) and §1.2
(host update lifecycle). Nothing here is implemented yet.

## Incident in one line

After the fleet rc.8 rollout rebooted willow, every fresh SMB mount of the `data`
share was denied (`mount error(13)`, dmesg `reconnect tcon failed rc = -13`).
Root cause was server-side, not client: samba's `orca` account lost its unix
mapping. `/etc/rc.d/rc.samba restart` on willow fixed it.

## Items

### 1. unRAID SMB user is not reboot-durable  ·  owned by `argyle-labs/unraid`

willow's smbd logged `build_sam_account: smbpasswd database is corrupt! username
orca with uid 999 is not in unix passwd database`. The reboot recreated the unix
`orca` user (uid 999) *after* smbd started, so samba's passdb→unix mapping was
corrupt and every new tree-connect for `orca` was denied. `getent passwd orca`
later showed the user present, but smbd had cached the broken state until
restarted.

Fix: the unraid plugin's user/SMB provisioning must (a) create the unix user
before samba starts (or restart samba after the user exists), (b) restart samba
after (re)provisioning the SMB user, and (c) self-heal the "not in unix passwd
database" state. Also observed: `orca` was not in the `users` group (gid 100)
despite `/boot/config/go` claiming it — verify group membership converges.

### 2. Daemon cannot mount on non-unRAID hosts  ·  §1.5

baldur's daemon logged `[converge] backend recover error: mount -a: exit
Some(1): mount: you must be root` — the privileged applier (`orca admin
storage-apply` invoked via `sudo -n`) is not permitted for the `orca` user on the
Alpine hosts. Consequence: the daemon never actually mounts, and every working
`data` mount on the fleet is currently a hand-run `mount -t cifs` that does not
survive reboot (the fstab lines are `#DISABLED`).

Fix: install the `sudo -n` allowance for `admin storage-apply` on non-unRAID
hosts as part of install, and make converged mounts reboot-durable.

### 3. Convergence leaves the mount down when sources probe unhealthy  ·  §1.5

While samba was broken, `mount_converge` logged `/mnt/data has NO live source (2
ordered sources down); leaving unmounted` for both routes and made no further
progress. Confirm convergence recovers on its own once the sources return
healthy. Separately, a fresh `storage mount update --action apply` on loki
rendered nothing (`changed: []`) for a placement that was enabled with a correct
host id and an enabled share + routes — investigate whether the render gap is
independent of source health (`projects/system/src/mount_converge.rs`).

### 4. unRAID `plugin install` does not restart the daemon  ·  §1.2

`plugin install <url> forced` on willow/maple staged the new binary but left the
old daemon process running (exe showed `(deleted)`); a manual
`/etc/rc.d/rc.orca restart` was needed to pick up the new version. The unRAID
update path should restart the daemon as part of the plugin install.

### 5. Inviter-side `pod offer` / `pod accept` join-confirm bug  ·  pod

The inviter-initiated pairing (`pod offer <addr>` / `pod accept <code>`) fails
with "no matching pending outbound offer"; the joiner-initiated `pod join
<inviter-addr>` works. Fix the inviter-side offer persistence so both directions
work.

## References

- Client-side mount surface: `projects/system/src/{mount_converge,mount_exec}.rs`,
  `storage_tools.rs`.
- Roadmap §1.5 (storage server side) and §1.2 (host update lifecycle) in
  [`README.md`](README.md).
