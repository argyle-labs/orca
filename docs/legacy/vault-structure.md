# Vault Structure

The brain vault at `~/.brain/` is the persistent knowledge store. It is separate from this code repo and symlinks into `~/dotfiles/brain/` — git-tracked in the private `scottdkey/dotfiles` repo.

## What it is

`~/.brain/` → `~/dotfiles/brain/` — an Obsidian-compatible vault. This means:
- All vault content is versioned in git (in the dotfiles repo)
- Syncs across machines via the dotfiles repo
- Can be opened as an Obsidian vault for human browsing
- Independent of the brain code repo's lifecycle

## Directory layout

```
~/.brain/
  memory/
    brain/          ← Claude auto-memory for this project (MEMORY.md + individual files)
    meerkat/         ← Claude auto-memory for the meerkat project
    <project>/      ← one directory per project
  plans/            ← implementation plans (symlinked from ~/.claude/plans/)
  logs/
    sessions/       ← JSONL session logs (one file per session, written by Pinky)
  plugins/          ← Claude Code plugin config (symlinked from ~/.claude/plugins/)
  brain-config/     → ~/code/brain/config/   (symlink, Obsidian-visible)
  meerkat-docs/      → ~/code/meerkat/docs/    (symlink, Obsidian-visible)
```

## What does NOT live in the vault

- **This codebase** (`~/code/brain/`) — source code is a separate git repo
- **Agent definitions** — live in `~/code/brain/projects/agents/src/agents/`, embedded in the binary and served via the `brain_get_agent` MCP tool
- **This docs/ directory** — compiled into the binary via rust-embed, always available via API and MCP regardless of filesystem
- **`node_modules`, `target/`** — build artifacts, gitignored everywhere

## Memory structure

Each project has a directory under `~/.brain/memory/<project>/`:
- `MEMORY.md` — index file; one line per memory entry, links to individual files
- `*.md` — individual memory files with YAML frontmatter (`type`, `name`, `description`)

Memory types:
- `user` — who the developer is, their skills, preferences
- `feedback` — guidance about approach: what to do and avoid
- `project` — ongoing work, decisions, active initiatives
- `reference` — pointers to external systems (Jira projects, dashboards, etc.)

The `brain_get_context` MCP tool reads these and returns the full memory context for a project. Claude Code's auto-memory system writes here automatically.

## Session logs

Pinky writes JSONL records to `~/.brain/logs/sessions/YYYY-MM-DD_HHMMSS_<project>.jsonl`. Each record:

```json
{
  "id": "uuid",
  "session": "2026-05-01_120000_brain",
  "timestamp": "ISO-8601",
  "project": "brain",
  "role": "assistant",
  "agent": "brain",
  "content": "...",
  "important": false,
  "tags": [],
  "note": ""
}
```

Search logs with: `brain log search "query"` or the `brain_search_logs` MCP tool.

## Config loading

The brain binary looks for `brain.toml` at `~/brain/config/brain.toml` (the vault path, via `$HOME` resolution at runtime — no hardcoded paths in the binary). The `~/.brain/brain-config/` symlink points to `~/code/brain/config/` so shared reference docs are Obsidian-visible.

## Why the separation

The vault is personal knowledge that should persist independently of the brain binary. If the brain repo is reworked or replaced, session logs, memory, and plans stay intact. A `git clean` or branch switch in the code repo cannot touch the vault.
