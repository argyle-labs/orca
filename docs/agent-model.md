# Agent Model

Agents are markdown files with YAML frontmatter. They live in `~/brain/ai/claude/agents/`.

## Format

```markdown
---
name: fox
description: Debug agent — traces root cause of errors and unexpected behavior
---

You are Fox. Your job is to find root causes...
(rest is the system prompt)
```

The `name` and `description` fields are parsed by brain to build the agent registry. Everything after the frontmatter is the system prompt injected when that agent is active.

## Why markdown files

- Editable in any text editor or Obsidian without rebuilding the binary
- Version controlled alongside other vault content
- Portable — the same agent definitions work for both the local brain sessions and Claude Code (`.claude/agents/` is a symlink into the vault)
- The description field is used for `brain_agents` MCP tool output, so Claude Code can discover what agents exist

## Loading strategy

At startup, `src/agents.rs` reads all `.md` files from the agents dir. If the directory doesn't exist or is empty, a fallback set of agents is loaded from the binary itself (compiled in by `build.rs`). This makes the binary usable even on a fresh machine before the vault is set up.

## Wolf as orchestrator

Wolf is the entry point for all sessions. When a user sends a message, Wolf decides:
1. Handle it directly
2. Delegate to a specialist via the `delegate` tool

The `delegate` tool call looks like:
```json
{ "name": "delegate", "arguments": { "agent": "fox", "prompt": "why is X failing?" } }
```

`session.rs` intercepts this tool call, loads Fox's system prompt, and runs a sub-session. The result is injected back into Wolf's context as a tool result.

## Specialist agents

Specialists have focused system prompts and don't need to know about delegation mechanics. They receive a task, execute it with the tools available, and return. The full tool set (bash, read, write, edit, glob, grep, confirm) is available to all agents unless the agent definition restricts it.

## Build-time embedding

`build.rs` reads all agent `.md` files and generates a `AGENTS` constant in the binary. This is the fallback. The runtime filesystem path always takes precedence — changing an agent file takes effect on the next `brain` invocation without rebuilding.
