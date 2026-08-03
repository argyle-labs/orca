# Documentation Map

The index of every doc in this repo. One line per doc; if it's not here, it
doesn't exist. How docs are written: [`DOCUMENTATION-GUIDELINES.md`](DOCUMENTATION-GUIDELINES.md).

## Getting oriented

- [`../README.md`](../README.md) — quick-start, install, dev commands.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — contributor standards and norms.
- [`../CRATE_RESPONSIBILITIES.md`](../CRATE_RESPONSIBILITIES.md) — the source of truth for what each workspace crate owns.
- [`repo-structure.md`](repo-structure.md) — directory-level map of the repo and on-host layout.
- [`../CHANGELOG.md`](../CHANGELOG.md) — release notes.

## Concepts & architecture

- [`architecture.md`](architecture.md) — the four-role binary, three-surface tool model, ports, identity, state ownership.
- [`single-binary.md`](single-binary.md) — why one binary per host, and the build sequence.
- [`CAPABILITY-REGISTRIES.md`](CAPABILITY-REGISTRIES.md) — canonical repo-wide capability-registry architecture.
- [`MANAGED-UNIT.md`](MANAGED-UNIT.md) — the universal `contract::unit` capability surface.
- [`pod.md`](pod.md) — the pod mesh: mutual-trust mesh of orca instances.
- [`BACKUP-SUBSYSTEM.md`](BACKUP-SUBSYSTEM.md) — the as-built backup subsystem (living doc).
- [`icon-system.md`](icon-system.md) — the argyle-labs icon-as-metadata system.
- [`ROADMAP.md`](ROADMAP.md) — canonical development sequencing and standing rules.
- [`coverage-baseline.md`](coverage-baseline.md) — test-coverage policy and the workspace floor.

## Developer onboarding

- [`dev/00-tour.md`](dev/00-tour.md) — codebase tour: orient before writing code.
- [`dev/02-patterns.md`](dev/02-patterns.md) — the recurring design patterns.
- [`dev/03-hot-paths.md`](dev/03-hot-paths.md) — the three most-trafficked code flows, traced end to end.
- [`dev/04-domain-concepts.md`](dev/04-domain-concepts.md) — orca-specific domain concepts.
- [`dev/05-how-to-recipes.md`](dev/05-how-to-recipes.md) — step-by-step recipes for common tasks.
- [`dev/01-primer/00-overview.md`](dev/01-primer/00-overview.md) — Rust primer index (ownership, enums, traits, async, errors, modules — files `01`–`06`).
- [`learn/codebase-tour.md`](learn/codebase-tour.md) — guided walk from request to Rust handler.
- [`learn/rust-primer.md`](learn/rust-primer.md) — Rust concepts drawn from actual orca code.
- [`learn/frontend-guide.md`](learn/frontend-guide.md) — where the web dashboard lives (the external **peacock** plugin) and how orca serves and exposes tools to it.
- [`learn/svelte-primer.md`](learn/svelte-primer.md) — pointer to peacock's Svelte 5 + SvelteKit stack.
- [`cli-reference.md`](cli-reference.md) — the `orca` command-line surface.

## Runbooks

- [`install-runbook.md`](install-runbook.md) — operator-facing fresh-host install.
- [`force-update-runbook.md`](force-update-runbook.md) — recover a host stuck on the wrong version.
- [`fleet-wipe-rejoin-runbook.md`](fleet-wipe-rejoin-runbook.md) — one-time re-key onto clean UUIDv7 identities.

## Design / RFCs

- [`MINIMAL-BACKUP.md`](MINIMAL-BACKUP.md) — RFC: universal minimal-backup + update-with-backup standard (originating draft).
- [`design/nfs-share-model.md`](design/nfs-share-model.md) — NFS share model: orca-managed native mounts (landed).
- [`design/packages-primitive.md`](design/packages-primitive.md) — proposal: cross-platform `packages` primitive.
- [`plugin-generics-punchlist.md`](plugin-generics-punchlist.md) — survey of per-plugin logic that should move to core.

## Plugin authoring

- [`plugin-authoring.md`](plugin-authoring.md) — the two plugin mechanisms and how to write one.
- [`OUT-OF-PROCESS-PLUGINS.md`](OUT-OF-PROCESS-PLUGINS.md) — the adopted out-of-process, capability-delegated plugin model.
- [`dynamic-linking.md`](dynamic-linking.md) — the subprocess plugin-loading model.
- [`../PLUGINS.md`](../PLUGINS.md) — plugin-author quick-pointer.

## Contracts / config-docs

The behavioral contract shared across every OrcaTool surface, under
[`../projects/contract/config-docs/`](../projects/contract/config-docs/):

- [`RULES.md`](../projects/contract/config-docs/RULES.md) — the top-level rule set.
- [`CODING_RULES.md`](../projects/contract/config-docs/CODING_RULES.md) — coding standards.
- [`TOOL_RULES.md`](../projects/contract/config-docs/TOOL_RULES.md) — tool-surface rules.
- [`CANONICAL_SOURCES.md`](../projects/contract/config-docs/CANONICAL_SOURCES.md) — where each fact's source of truth lives.
- [`DELEGATION.md`](../projects/contract/config-docs/DELEGATION.md) — delegation model.
- [`AGENTS.md`](../projects/contract/config-docs/AGENTS.md) — agent behavior contract.
- [`PERSONA.md`](../projects/contract/config-docs/PERSONA.md) — persona contract.
- [`MEMORY_SYSTEM.md`](../projects/contract/config-docs/MEMORY_SYSTEM.md) — memory-system contract.
- [`SEVERITY_RUBRIC.md`](../projects/contract/config-docs/SEVERITY_RUBRIC.md) — severity rubric.
- [`FRONTEND.md`](../projects/contract/config-docs/FRONTEND.md) — frontend contract (the peacock surface).
