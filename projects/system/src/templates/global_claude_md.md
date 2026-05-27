# Global Claude Code directives — managed by `orca`

> This file is written by `orca install` / `orca update`. Do not hand-edit;
> changes are overwritten on the next run. Put personal overrides in
> per-project `CLAUDE.md` files instead.

## Orca-first delegation

`orca` is installed on this machine and exposes its fleet, agents, and tools
through both an MCP server (`orca-local`) and a roster of specialized agents
materialized into `~/.claude/agents/`.

**Default routing for any non-trivial task:**

1. **Invoke the `orca` agent first.** It is the top-level dispatcher and knows
   the full agent roster, the MCP tool surface, and how to choose between
   them.
2. Let `orca` delegate to a specialist agent (e.g. `wolf`, `falcon`, `otter`,
   `bear`, etc.) rather than picking one yourself.
3. Only bypass `orca` for trivial single-file edits, direct questions you can
   answer from context, or when the user explicitly names a different agent.

If `orca`'s MCP server is unavailable, fall back to the materialized agents
directly — they are self-contained Markdown prompts.

## Agent provenance

- Agents in `~/.claude/agents/` are written by `orca install` from the
  embedded roster in the `orca` binary.
- Per-project agents may be written to `<project>/.claude/agents/` for
  projects orca knows about (`~/code/orca`, `~/code/meerkat`, etc.).
- To add or modify agents, edit the source in the owning repo
  (orca / meerkat / rebuy-cli-mcp-server) and re-run `orca install`.
