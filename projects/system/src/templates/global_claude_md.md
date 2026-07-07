# Global Claude Code directives — managed by `orca`

> This file is written by `orca install` / `orca update`. Do not hand-edit;
> changes are overwritten on the next run. Put personal overrides in
> per-project `CLAUDE.md` files instead.

## Orca-first routing

`orca` is installed on this machine and exposes its fleet, agents, and tools
through both an MCP server (`orca-local`) and a roster of specialized agents
materialized into `~/.claude/agents/`.

**Default routing for any non-trivial task:**

1. **Invoke the `orca` agent first** via the `Agent` tool. Pass the user's
   request verbatim plus any directly relevant context. `orca` knows the full
   agent roster, the MCP tool surface, and how to choose between them.
2. **`orca` returns a routing decision, not a result.** Claude Code does not
   grant the `Agent` tool to subagents, so `orca` cannot delegate on its own.
   Its response ends with an `orca-route` fenced block specifying `route:
   wolf | otter | direct` plus the prompt to send. Read that block and
   execute it: invoke the named subagent (`wolf` or `otter`) with the prompt
   `orca` provided, or — if `route: direct` — answer the user yourself using
   the contents of the block.
3. Only bypass `orca` for trivial single-file edits, direct questions you can
   answer from context, or when the user explicitly names a different agent.

If `orca`'s MCP server is unavailable, fall back to the materialized agents
directly — they are self-contained Markdown prompts.

## Agent provenance

- Agents in `~/.claude/agents/` are written by `orca install` from the
  embedded roster in the `orca` binary.
- To add or modify agents, edit the source in the owning repo
  (the orca repo or any plugin repo that contributes agents) and re-run
  `orca install`.
