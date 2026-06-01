# Planned — orca scope docs

Work-in-progress designs for orca capabilities. Each doc has a
work breakdown with sizing (S/M/L/XL) and cross-references to the
others. Consumer-repo examples cite the meerkat / scottkey
homelab; orca itself stays generic.

> **Canonical sequencing lives in [`../ROADMAP.md`](../ROADMAP.md).**
> Every file in this directory is scope detail for one or more
> roadmap items. System lifecycle (Phase 1) is the focus until
> parity; service surface (Phase 2) and deferred items (Phase 3)
> wait behind it.

## Scope detail by topic

| Guide | What it covers | Roadmap item |
|-------|----------------|---|
| [orca-v1-scope.md](orca-v1-scope.md) | Framework-wide v1 scope: shipped table, P0/P1/P2 build map, config store + scheduler + bootstrap + GitHub App + git sync | Phase 0 + 1.3 + Phase 2 cross-cuts |
| [orca-as-logic-layer.md](orca-as-logic-layer.md) | Umbrella migration plan: move all behavior out of meerkat into orca; trust-tier credential sync; git-provider abstraction | Phase 2 (inventory) |
| [install-bootstrap.md](install-bootstrap.md) | One-command install, platform detection, minimum permissions, pairing | Phase 1.3 |
| [host-lifecycle.md](host-lifecycle.md) | Drivers (NVIDIA/AMD/Intel), OS updates, reboots, UPS-coordinated shutdowns | Phase 1.2 + 1.6 |
| [lxc-vm-reconciler.md](lxc-vm-reconciler.md) | Declarative Proxmox LXC + VM configs: diff repo vs live, bind/tmpfs lifecycle, restore-aware start, drift detection | Phase 1.1 (next slice) |
| [observability.md](observability.md) | Metrics + logs + viewer; tree UI; retention; external sinks | Phase 1.9 |
| [backup-restore.md](backup-restore.md) | Unified backup framework: managed services + orca self-state; restore drills; offsite | Phase 1.8 |
| [storage-shares.md](storage-shares.md) | Native share management: one declarative share → reconciled NFS + SMB(+fruit) + Avahi/mDNS + wsdd for Mac/Windows/Linux | Phase 1.7 |
| [secrets-identity.md](secrets-identity.md) | Unified identity + secret backends: one orca login everywhere (incl. SMB), 1Password-backed secrets, baseline password rotation | Phase 2 |
| [pki-lifecycle.md](pki-lifecycle.md) | CA rotation, peer revocation, compromise response | Phase 3 |
| [schema-evolution.md](schema-evolution.md) | Parity rule — no retirement before validation; in-repo migrations | Phase 1.10 (cross-cutting) |
| [caddy-plugin-scope.md](caddy-plugin-scope.md) | Caddy as a managed plugin: routes from config repo, mTLS to orca upstreams, fanout | Phase 2 |
| [plugin-architecture.md](plugin-architecture.md) | Plugin ecosystem: host plugins vs service plugins, contract | Cross-cutting |
| [namespace-consolidation.md](namespace-consolidation.md) | Tool-namespace cleanup | Phase 3 |
| [rebuy-plugin-scope.md](rebuy-plugin-scope.md) | Rebuy as the second consumer + plugin-contract canary | Phase 3 |
