# Vault Structure

The brain vault at `~/brain` is the persistent knowledge store. It is separate from this code repo.

## What it is

`~/brain` is a symlink to `~/dotfiles/obsidian/` — an Obsidian vault that is git-tracked in the private `scottdkey/dotfiles` repo. This means:
- All vault content is versioned in git
- It syncs across machines via the dotfiles repo
- It can be opened as an Obsidian vault for human browsing
- It is independent of this code repo's lifecycle

## What lives there

```
~/brain/
  ai/claude/
    agents/       Agent .md files (loaded by brain at startup)
    commands/     Custom Claude slash commands
    memory/       Per-project auto-memory (MEMORY.md + individual files)
    logs/sessions/  JSONL session logs (one file per session)
    plans/        Implementation plans
    plugins/      Claude Code plugin config
  config/
    brain.toml    Runtime config (API base URL, model, database settings)
  notes/          Freeform personal notes
  docs/           (not this repo's docs/ — personal reference docs if any)
```

## What does NOT live there

- **This codebase** (`~/code/brain/`) — the source code is a separate repo
- **This docs/ directory** — compiled into the binary via rust-embed, always available with the binary
- **node_modules, target/** — build artifacts, gitignored

## Why the separation

The vault is personal knowledge that should persist and evolve independently of the brain binary. If the brain repo is reworked or replaced, the vault stays intact. Session logs, agent definitions, and memory should not be at risk from a `git clean` or branch switch in the code repo.

## Memory structure

Each project has a directory under `ai/claude/memory/<project>/`:
- `MEMORY.md` — index file; one line per memory, links to individual files
- `*.md` — individual memory files with YAML frontmatter (`type`, `name`, `description`)

The `brain_get_context` MCP tool reads these and returns the full memory context for a project. Claude Code's auto-memory system writes here automatically.

## Config loading

`src/config.rs` looks for `brain.toml` at `~/brain/config/brain.toml`. The path is resolved at runtime — there is no hardcoded `/home/user/` path in the binary, only `dirs::home_dir()`.
