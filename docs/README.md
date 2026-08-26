# Documentation Map

> [!WARNING]
> **Pre-alpha — not ready for public release.** orca is under active, sweeping
> development: interfaces, the domain model, the plugin ABI, and the on-disk
> schema all change frequently and may break between versions. No stability or
> support guarantee yet. Explore or contribute — do not deploy to anything you
> care about.

The index of every doc in this repo. One line per doc; if it's not here, it
doesn't exist. How docs are written: [`DOCUMENTATION-GUIDELINES.md`](DOCUMENTATION-GUIDELINES.md).

## How docs are written

- [`DOCUMENTATION-GUIDELINES.md`](DOCUMENTATION-GUIDELINES.md) — the rules every doc in this repo follows.

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
- [`planned/`](planned/README.md) — forward-looking work (the roadmap): initiatives, research, phased plans. `docs/` top-level is current state only.
- [`coverage-baseline.md`](coverage-baseline.md) — test-coverage policy and the workspace floor.

## Developer onboarding

- [`dev/00-tour.md`](dev/00-tour.md) — codebase tour: orient before writing code.
- [`dev/02-patterns.md`](dev/02-patterns.md) — the recurring design patterns.
- [`dev/03-hot-paths.md`](dev/03-hot-paths.md) — the three most-trafficked code flows, traced end to end.
- [`dev/04-domain-concepts.md`](dev/04-domain-concepts.md) — orca-specific domain concepts.
- [`dev/05-how-to-recipes.md`](dev/05-how-to-recipes.md) — step-by-step recipes for common tasks.
- [`dev/01-primer/00-overview.md`](dev/01-primer/00-overview.md) — Rust primer index (ownership, enums, traits, async, errors, modules — files `01`–`06`).
- [`learn/frontend-guide.md`](learn/frontend-guide.md) — where the web dashboard lives (the external **peacock** plugin) and how orca serves and exposes tools to it.
- [`cli-reference.md`](cli-reference.md) — the `orca` command-line surface.

## Runbooks

- [`install-runbook.md`](install-runbook.md) — operator-facing fresh-host install.
- [`force-update-runbook.md`](force-update-runbook.md) — recover a host stuck on the wrong version.
- [`fleet-wipe-rejoin-runbook.md`](fleet-wipe-rejoin-runbook.md) — one-time re-key onto clean UUIDv7 identities.

## Design records

As-built design docs for landed subsystems. Proposals, RFCs, and backlogs for
unbuilt work live under [`planned/`](planned/README.md).

- [`design/nfs-share-model.md`](design/nfs-share-model.md) — NFS share model: orca-managed native mounts (landed).

## Plugin authoring

- [`plugin-authoring/`](plugin-authoring/README.md) — how to write a plugin, split by concept (tools, CRUD, unit providers, backends, toolkit capabilities, manifest plugins, agents).
- [`OUT-OF-PROCESS-PLUGINS.md`](OUT-OF-PROCESS-PLUGINS.md) — the adopted out-of-process, capability-delegated plugin model.
- [`plugin-loading.md`](plugin-loading.md) — the subprocess plugin-loading model.
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
