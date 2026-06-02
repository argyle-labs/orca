# UI Topology Views

**Status:** Planned (ROADMAP §1.9). Stated 2026-05-28.

Orca's web UI presents systems two ways, switchable from the same
peer list:

## 1. Tree view (network diagram / dependency tree)

Hierarchical layout that mirrors physical reality:

- physical host → VMs/LXCs → containers → services
- cross-host dependencies (storage gateway → consumers; primary →
  replica; UPS coordinator → fleet)

Built on the `parent_peer_id` hierarchy inferred server-side (never
manual config) and replicated like other per-host data. Goal is to
let the operator reason about **failure domains** at a glance —
"if this host dies, what goes with it."

Dependency edges also draw on the mesh-state ownership graph:
who replicates/depends on whom (storage tiers per §1.17, backup
targets per §1.8, escrow shares per §1.12b).

## 2. Table view (flat list)

Today's pod overview. Sortable / filterable by host, role, status,
drift count, last-seen.

## Design rules

- **Both modes from day one.** Don't hard-code a flat table and
  retrofit the tree later — the data model has to support both.
- **Server-side inference only.** No "set my parent" UI; topology
  is derived from observation (mesh pairing, runtime probes,
  config-store ownership).
- **Replicated state, not RPC-on-render.** The tree is reading
  from the same replicated per-host data as the table view; no
  live cross-fleet calls to paint a frame.

## Source

Promoted from memory `project_ui_topology_views.md` 2026-06-01.
Related: `project-host-hierarchy-detection`,
`project-unified-mesh-state`, `project-data-ownership-and-realtime`.
