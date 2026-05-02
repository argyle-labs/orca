# Agent Model

Agents are markdown files with YAML frontmatter. The authoritative set lives in `projects/agents/src/agents/` inside the brain code repo and is embedded into the binary at compile time. They are also served via the `brain_get_agent` MCP tool.

## Format

```markdown
---
name: fox
description: Debug agent — traces root cause of errors and unexpected behavior
tools: [Read, Glob, Grep, Bash]
model: claude-sonnet-4-6
color: orange
---

You are Fox. Your job is to find root causes...
(rest is the system prompt)
```

The `name` and `description` fields are parsed by brain to build the agent registry. The frontmatter can optionally restrict `tools` and set a preferred `model`. Everything after the frontmatter is the system prompt injected when that agent is active.

## How to find an agent

```sh
brain agents                  # list all agents with name + description
brain_get_agent name="fox"    # MCP tool: returns the full system prompt
```

There are 39+ agents defined. Key entry points:
- **wolf** — orchestrator, entry point for all sessions. Routes to specialists.
- **lynx** — task planner, breaks work into steps
- **pinky** — I/O sub-orchestrator: reads, writes, notes, file-finding, session logs
- **fox** — debugging, root cause analysis
- **owl** — reasoning and explanation
- **crow** — code writing
- **raven** — notes and memory
- **bloodhound** — file finding and path resolution
- **ibis** — documentation consistency

## Loading strategy

`projects/agents/build.rs` reads all `.md` files from `src/agents/` at compile time and generates a static match function. The `BRAIN_AGENTS_DIR` environment variable overrides the default path — useful in dev to hot-reload agents from an alternate location without rebuilding.

The agent library is exposed via the `brain_get_agent` MCP tool, not by filesystem scan at runtime. Claude Code discovers available agents by calling `brain_agents` (returns names + descriptions) and retrieves any specific agent's prompt with `brain_get_agent`.

## Wolf as orchestrator

Wolf is the entry point for all sessions. When a user sends a message, Wolf decides:
1. Handle it directly
2. Delegate to a specialist via a `delegate` tool call

The `delegate` tool call looks like:
```json
{ "name": "delegate", "arguments": { "agent": "fox", "prompt": "why is X failing?" } }
```

`session.rs` intercepts this, loads Fox's system prompt, and runs a sub-session. The result is injected back into Wolf's context as a tool result. This nesting is transparent to the user.

## Specialist agents

Specialists have focused system prompts and don't know about delegation mechanics. They receive a task, execute it with the tools available, and return. The full tool set (bash, read, write, edit, glob, grep) is available to all agents unless the agent definition restricts it via the `tools` frontmatter field.

## Pinky as I/O sub-orchestrator

Pinky is Wolf's companion for I/O tasks. Brain narrates tasks to Pinky, who delegates to the right specialist:

```
Pinky's delegation map:
  owl         → read and explain code
  crow        → write or implement code
  raven       → write to memory vault
  bloodhound  → find files, resolve paths
  ibis        → check docs match code
```

Session logging is also Pinky's responsibility. Every session writes a JSONL file to `~/.brain/logs/sessions/`.

## Adding a new agent

1. Create `projects/agents/src/agents/<name>.md` with frontmatter + system prompt
2. Rebuild with `cargo build` — `build.rs` embeds it
3. The agent is immediately available via `brain_get_agent name="<name>"`

In dev with `BRAIN_AGENTS_DIR` set, changes take effect on the next session start without a rebuild.

## Why markdown files

- Editable in any text editor or Obsidian without rebuilding the binary (with hot-reload env var)
- Version controlled alongside the code
- The description field feeds the `brain_agents` MCP tool output so Claude Code can discover available agents dynamically
