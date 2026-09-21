# Standard Delegation Patterns

Reference document for all agents. Agents reference this file instead of
maintaining their own routing tables. The roster (wolf/otter/…) is supplied by
the external `argyle-labs/agents` plugin and registered into orca at runtime;
this file describes the delegation *model*, not a hardcoded agent list — consult
`agent_list` over the Orca MCP for the live roster.

## The delegation model

- **orca** (the main session / hub) is the single orchestrator. It owns the
  task, delegates directly to the right leaf specialist, and verifies before
  claiming done. There is no mid-tier orchestrator.
- **wolf** is a pure executor/reviewer leaf, like every other specialist. It
  runs the work it is handed; it does not fan out to or route to other agents.
- **lynx** plans: it maps the minimal agent chain before work begins.
- **otter** is a pure I/O executor leaf. It handles reads, writes, notes,
  file-finding, docs, session logging, and log search when handed that work.
  It does not orchestrate other agents — orca delegates to owl (reads), crow
  (writes), raven (notes), bloodhound (file-finding), and ibis (docs) directly.
- **The star model:** one level of delegation (orca → leaf). Every specialist
  reports back up to orca; nothing chains sideways through a mid-tier. A writer
  never certifies its own work — an independent reviewer runs before done.
  Parallel fan-out (N instances of one specialist at once) is fine.
- **Specialists** (below) own a single concern. orca delegates to them
  directly for well-scoped work.
- Use Glob/Grep/Read directly for simple targeted lookups — no delegation needed.

## Specialist agents

| Task | Agent |
|------|-------|
| Debug a bug, trace root cause | @fox |
| Read and explain code | @owl |
| Write or implement code | @crow |
| Simplify / reduce duplication | @spider |
| Code standards (any language) | @ferret |
| Critical review, gap-finding, system audit | @bear |
| Security audit | @viper |
| Test coverage audit | @shrew |
| Accessibility audit (WCAG 2.1 AA) | @swift |
| External tech docs (TS, React, Postgres, etc.) | @elephant |
| Privacy / PII sweep | @hound |
| Coverage audit (missing agents/hooks) | @kestrel |
| PR comment formatting (Bitbucket/GitHub API) | @heron |
| Adversarial plan review | @shrike |
| DevOps / CI/CD / infra | @falcon |
| Note-taking / memory vault | @raven |
| Session logging / search across logs | @otter |
| File reads, writes, finds, documentation | @otter |
| Filesystem index + path resolution | @bloodhound |
| Documentation consistency | @ibis |
| Agent file maintenance | @wren |
| Placement auditing (wrong location) | @jackdaw |
| Scope graduation (project → global) | @magpie |
| Planning (minimal agent chain, token estimate) | @lynx |
| Escalation judgment (local vs Claude) | @osprey |
| Container inspection (running dev containers) | @hawk |
| Machine process / port inspection | @mole |

## Before writing, refactoring, or reviewing code

Load codebase context first. See [`CANONICAL_SOURCES.md`](CANONICAL_SOURCES.md)
for the authoritative type, schema, and architecture sources in this repo.
Grepping for patterns is not a substitute for understanding the architecture.

**Never guess at conventions. Read first.**
