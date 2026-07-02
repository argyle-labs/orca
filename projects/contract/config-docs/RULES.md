---
name: RULES
description: Core behavior rules — execution model, path resolution, session logging, git safety
---

# Core Behavior

- Do not modify the codebase unless the user explicitly grants permission. Default: advise and provide snippets only.
- Never run build pipelines — tell the user to build instead.
- Never commit, push, or stage git changes. Tell the user when it is time to commit.
- Specs and plans go in `~/.orca/plans/` — never anywhere else.

# Execute vs Plan Modes

Default mode is **advisory** — analysis, recommendations, code snippets. The user implements.

**Execution is opt-in.** Explicit triggers: "execute", "do it", "write it", "implement it", "go ahead". Once execution is authorized for a task, carry it to completion without re-confirming every step. Authorization is scoped to the current task only.

**Never default to execution.** When intent is unclear, advise.

### Command semantics

- "do it", "proceed", "execute the plan", "I approve", "go" = execute the approved plan to completion. Do not ask "Proceed?" after every step. Ask once before the plan. Then execute.
- Stop only for genuine blockers or ambiguity, not for routine steps.

# Path Resolution

All config paths use `~/` for the home directory.

- Claude Code file tools (Read, Write, Edit, Glob) expand `~/` natively — pass paths as-is.
- Bash commands: use `$HOME` for reliable shell expansion.
- Never hardcode `/home/username/` or `/Users/username/` — config is shared across Linux and macOS.
- If a path does not resolve, run `echo $HOME` to confirm, then construct explicitly.

# Session Logging

On the first substantive response in every conversation, spawn Otter in the background to start a session log. Detect the project from the working directory. Otter writes to `~/.orca/logs/sessions/YYYY-MM-DD_HHMMSS_<project>.jsonl`.

Throughout the conversation, when a decision, fix, architecture choice, or anything the user flags occurs, append to the session log with `important: true` and relevant tags. Log at minimum:
- Bug diagnoses and root causes
- Code changes and what they fix
- Dependency updates
- New scripts or tooling
- User preferences or corrections

# Knowledge Vault

`~/.orca/` — primary vault (symlink → `~/dotfiles/orca/`, git-tracked in dotfiles)
`~/dotfiles/notes/` — Obsidian vault root

Structure:
- `~/.orca/memory/<project>/` — auto-memory (per-project)
- `~/.orca/plans/` — implementation plans
- `~/.orca/logs/sessions/` — session logs
- `~/.orca/plugins/` — installed plugins
- `~/code/argyle-labs/orca/projects/agents/src/agents/` — agent definitions (source of truth)
- `~/code/argyle-labs/orca/config/` — shared reference docs

# Git Safety

- Never use `--no-verify`, `--no-gpg-sign`, or force-push to main/master
- Never amend a published commit — create a new one
- Stage specific files, never `git add -A` or `git add .`
- Destructive operations (reset --hard, branch -D, rm -rf) require explicit user authorization
