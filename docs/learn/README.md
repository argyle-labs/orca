# Learning orca

A sequenced path through orca and its plugin ecosystem. The reference docs in
[`../`](../README.md) are organized by *subject*; this path is organized by
*order you can actually absorb them in*. Each lesson states what you must
already know, teaches one idea, and ends with exercises you run against the
live fleet.

This is a learner's path, not a second source of truth. Where a lesson states
a fact, it links the canonical doc that owns it. If a lesson and a reference
doc disagree, the reference doc wins — and the lesson is a bug.

## Track 1 — The shape of the system

- [`00-what-orca-is.md`](00-what-orca-is.md) — one binary, three surfaces, a mesh of peers. The mental model everything else hangs on.
- `01-the-tool-macro.md` — how `#[orca_tool]` becomes a CLI subcommand, a REST route, and an MCP tool at once.
- `02-crates-and-the-spine.md` — the domain-crate model and the acyclic dependency spine.
- `03-state-config-secrets.md` — what lives in the encrypted DB, what is read live from the host, and where secrets come from.
- `04-the-mesh.md` — identity, pairing, mTLS, and how a verb issued here executes there.

## Track 2 — Plugins

- `05-why-plugins.md` — thin plugin / maximal core, and why vendor integrations are never core.
- `06-anatomy-of-a-plugin.md` — walking one real first-party plugin end to end.
- `07-capabilities-and-seams.md` — how a plugin reaches db, secrets, storage, http without owning them.
- `08-writing-a-plugin.md` — build one from empty directory to registered tool.

## Track 3 — Operating the thing

- `09-peacock-and-the-operator-surface.md` — the web UI as a plugin that consumes REST.
- `10-deploy-backup-restore.md` — the three verbs every managed unit owes you.
- `11-reconcile-and-self-heal.md` — level-triggered convergence instead of event chasing.

## Track 4 — Contributing

- `12-dev-loop.md` — build, test, and the gates that stop a bad push.
- `13-adding-a-domain.md` — the full path for new core capability.

> Lessons are written on demand, in order. An unlinked entry above is planned,
> not missing.
