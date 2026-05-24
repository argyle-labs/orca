# Planned — orca scope docs

Work-in-progress designs for orca capabilities. Each doc has a
work breakdown with sizing (S/M/L/XL) and cross-references to the
others. Consumer-repo examples cite the meerkat / scottkey
homelab; orca itself stays generic.

## Roadmap

| Guide | What it covers |
|-------|----------------|
| [orca-v1-scope.md](orca-v1-scope.md) | Orca framework v1 scope — what's shipped (Prod) and what's next |
| [orca-as-logic-layer.md](orca-as-logic-layer.md) | Umbrella migration plan: move all behavior out of meerkat into orca; trust-tier credential sync; git-provider abstraction |
| [install-bootstrap.md](install-bootstrap.md) | One-command install, platform detection, minimum permissions, pairing |
| [host-lifecycle.md](host-lifecycle.md) | Drivers (NVIDIA/AMD/Intel), OS updates, reboots, UPS-coordinated shutdowns |
| [observability.md](observability.md) | Metrics + logs + viewer; tree UI; retention; external sinks |
| [backup-restore.md](backup-restore.md) | Unified backup framework: managed services + orca self-state; restore drills; offsite |
| [pki-lifecycle.md](pki-lifecycle.md) | CA rotation, peer revocation, compromise response |
| [schema-evolution.md](schema-evolution.md) | Parity rule — no retirement before validation; in-repo migrations |
| [caddy-plugin-scope.md](caddy-plugin-scope.md) | Caddy as a managed plugin: routes from config repo, mTLS to orca upstreams, fanout |
| [plugin-architecture.md](plugin-architecture.md) | Plugin ecosystem: host plugins vs service plugins, contract |
| [namespace-consolidation.md](namespace-consolidation.md) | Tool-namespace cleanup |
| [rebuy-plugin-scope.md](rebuy-plugin-scope.md) | Rebuy as the second consumer + plugin-contract canary |
