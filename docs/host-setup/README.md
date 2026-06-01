# Host setup (pre-orca, per-OS)

These guides cover **manual** prerequisite setup for hosts that
will later run orca (Docker daemon, NFS client config, base
package install on Alpine / Debian / Fedora / LXC / Unraid).
They are operator runbooks for the bootstrap window before orca
is managing the host.

> **Scope note.** Everything in this directory will eventually be
> orca verbs. Until parity ([`../ROADMAP.md`](../ROADMAP.md)
> Phase 1) is reached, these manual runbooks are the source of
> truth. Cross-references:
>
> - LXC + VM lifecycle (declarative `pct.conf` / `qemu-server.conf`,
>   bind-source readiness, restore-aware start) →
>   [`../planned/lxc-vm-reconciler.md`](../planned/lxc-vm-reconciler.md).
> - Host updates, drivers, reboots, UPS-coordinated shutdown →
>   [`../planned/host-lifecycle.md`](../planned/host-lifecycle.md).
> - First-boot install + service-user + PKI →
>   [`../install-runbook.md`](../install-runbook.md).
> - NFS / SMB declarative share management →
>   [`../planned/storage-shares.md`](../planned/storage-shares.md).

## Guides

| Guide | Topic |
|---|---|
| [host-setup-alpine.md](host-setup-alpine.md) | Alpine VM bring-up |
| [host-setup-fedora.md](host-setup-fedora.md) | Fedora VM bring-up (NVIDIA GPU) |
| [host-setup-unraid.md](host-setup-unraid.md) | Unraid as a docker host |
| [docker-alpine.md](docker-alpine.md) | Docker on Alpine |
| [docker-debian.md](docker-debian.md) | Docker on Debian |
| [docker-lxc.md](docker-lxc.md) | Docker inside Proxmox LXC |
| [nfs-alpine.md](nfs-alpine.md) | NFS client on Alpine |
| [nfs-debian.md](nfs-debian.md) | NFS client on Debian / Ubuntu |
| [nfs-lxc.md](nfs-lxc.md) | NFS client on Proxmox LXC |
| [nfs-failover.md](nfs-failover.md) | NFS failover config |

## Convergence target

Per ROADMAP §1.3, the per-OS divergence here should collapse to
**one canonical install path** with platform sections, driven by
orca verbs. The per-OS files stay as operator references for the
manual / recovery path.
