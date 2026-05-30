---
name: MEMORY_SYSTEM
description: Memory system — types, when to save, how to save, file format
---

# Memory System

Memory files are written to `~/.claude/projects/*/memory/` which symlinks into `~/.orca/memory/<project>/` (git-tracked in dotfiles).

Prefer `orca memory` CLI commands when available:
```sh
orca memory write --type feedback --name <name> --project <project> --body "..."
orca memory read --project <project>
orca memory search --query "..." [--project <project>]
```

Fall back to direct Write tool only when the CLI is unavailable.

# Memory Types

## user
Information about the user's role, goals, preferences, and knowledge. Use to tailor responses.
Save when: learning role, preferences, responsibilities, or domain knowledge.

## feedback
Guidance the user has given about how to approach work. Record corrections AND confirmations.
Body structure: lead with the rule, then **Why:** and **How to apply:** lines.
Save when: user corrects an approach, or confirms a non-obvious one worked.

## project
Information about ongoing work, goals, bugs, or decisions not derivable from code or git history.
Body structure: lead with the fact, then **Why:** and **How to apply:** lines.
Save when: learning who is doing what, why, or by when. Convert relative dates to absolute.

## reference
Pointers to where information lives in external systems.
Save when: learning about external resources (Linear projects, Grafana boards, Slack channels).

# What NOT to Save

- Code patterns, architecture, file paths — derivable from current code
- Git history — `git log` is authoritative
- Debugging recipes — the fix is in the code; context belongs in the commit message
- Anything in CLAUDE.md or config files
- Ephemeral task details or current conversation state

# File Format

```markdown
---
name: <memory name>
description: <one-line description — used to decide relevance>
type: user | feedback | project | reference
---

<content — for feedback/project: rule/fact, then **Why:** and **How to apply:** lines>
```

Add a pointer to `MEMORY.md` index (one line, under ~150 chars):
`- [Title](file.md) — one-line hook`

# Before Recommending from Memory

A memory naming a file path, function, or flag is a claim about what existed when written. Before recommending:
- Named file path → verify it exists
- Named function or flag → grep for it
- Repo state summary → prefer `git log` over the snapshot

Memory that conflicts with current code: trust what you observe now, update the stale memory.
