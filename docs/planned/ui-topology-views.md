# UI Topology Views

**Status:** Planned (ROADMAP §1.9). Stated 2026-05-28.

Orca's web UI presents systems three ways, switchable from the same
peer list:

## 1. Tree view (hierarchical hosting / containment)

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

## 2. Network map (full relationship graph)

Node-link diagram driven by the §1.21 typed-edge graph. Renders
**all** edge kinds: `hosts`, `connects`, `routes`, `exposes`,
`depends_on`, `replicates`, `backs_up`, `escrows`. Filterable by
edge kind so the operator can isolate a single layer (e.g.
"show only routes + connects" for pure network topology, "show
only hosts" for containment).

**Cycles render honestly.** The strange-loop case (loki hosts
opnsense which routes loki) draws as a cycle — no silent
breakage, no parent-edge picked arbitrarily.

Walks (`config.walk`) are visualizable directly: the visited
sub-graph highlights, with the walk direction shown.
Failure-domain queries paint the "what dies with X" set onto
the map.

### Interactions (mouse + touch, both first-class)

- **Pan** — mouse drag, touch drag.
- **Zoom in/out** — scroll wheel, pinch, UI +/− buttons,
  keyboard +/−.
- **Collapse / expand subtrees** — click a node to collapse
  its descendants; click again to expand. Per-operator UI
  preference, not server state.
- **Edge-kind filter** — layered toggle on top of the same
  view; toggling a kind off greys those edges + isolated
  nodes.
- **Cycles must survive every interaction** — no silent
  flattening when pan/zoom/collapse touches a cycle.

## Systems dashboard (the entry point above all three views)

The default Systems page is a **dashboard**, not a heading + list.
Layout spec lives in `feedback_systems_dashboard_layout` memory
(canonical). Key constraints to honor in any view that exposes
system cards:

- No "Systems" heading, no "Connected orca instances." subtitle.
- Single action button → popover with `Invite host` + `Pair with code`.
- Discovered (unenrolled) machines list at the top.
- Each system card uses the **bars-with-inline-stats** row
  pattern (label + bold percentage + raw value/unit, all in one
  HStack; bar fills the row beneath). No redundant stat line, no
  IP addresses on the card (those live in Details).

## 3. Table view (flat list)

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
