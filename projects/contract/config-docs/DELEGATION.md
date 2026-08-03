# Standard Delegation Patterns

Reference document for all agents. Agents reference this file instead of
maintaining their own routing tables. The roster (wolf/otter/…) is supplied by
the external `argyle-labs/agents` plugin and registered into orca at runtime;
this file describes the delegation *model*, not a hardcoded agent list — consult
`agent_list` over the Orca MCP for the live roster.

## The delegation model

- **wolf** is the primary orchestrator. Route open-ended or cross-domain work
  here when you are unsure where it belongs.
- **lynx** plans: it maps the minimal agent chain before work begins.
- **otter** is the I/O sub-orchestrator. It fans out to reads (owl), writes
  (crow), notes (raven), file-finding (bloodhound), and docs (ibis), and owns
  session logging and log search.
- **Specialists** (below) own a single concern. Prefer routing through otter or
  wolf; invoke a specialist directly only for a narrow, well-scoped task.
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
| Adversarial plan review | @mongoose |
| DevOps / CI/CD / infra | @falcon |
| Note-taking / memory vault | @raven |
| Session logging / search across logs | @otter |
| File reads, writes, finds, documentation | @otter (delegates to owl/crow/raven/bloodhound/ibis) |
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
