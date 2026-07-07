# Severity Rubric

Standard severity definitions for all review agents (bear, bod-api-review, connector-review, viper, shrew, otter). Reference this file instead of defining severities inline.

## Levels

### CRITICAL — merge blocker, no exceptions
- Will crash production
- Will corrupt data or cause irrecoverable data loss
- Will cause downtime
- Leaks secrets or credentials to logs, responses, or external systems
- Authentication or authorization bypass

### HIGH — merge blocker unless explicitly deferred with documented justification
- Wrong behavior in realistic (non-happy-path) conditions
- Fails on deleted externals, concurrent operations, or edge-case data
- Missing rollback path for a destructive operation
- Breaking API change without client coordination
- Bulk data operation inside a migration transaction (lock risk)

### MEDIUM — should fix before merge, but negotiable
- Subtle bugs that only appear in specific sequences
- Missing test coverage for important paths
- Schema drift without client coordination (nullable → not nullable, type narrowing)
- Operational gaps: new background job not scheduled, new route missing auth or rate-limiting
- Stale documentation that contradicts the implementation

### LOW — fix opportunistically
- Code smell or misleading names
- Placement issues (wrong file, wrong directory)
- Doc drift (typos, outdated examples)
- Style inconsistencies not caught by the linter

## Rules for reviewers

- Every CRITICAL and HIGH finding must include: **file path**, **line number**, and a **concrete remediation** (proposed diff or specific change to make).
- Do not conflate "I prefer X" with "this is wrong." State which level applies and why.
- "This is a style preference" is LOW at most. Do not merge-block on style.
- Every finding cites file + line. No vague findings ("this area is problematic").
