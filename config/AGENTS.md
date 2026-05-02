---
name: AGENTS
description: Agent roster, delegation model, and how to invoke agents via MCP
---

# Agent System

Agents are defined in `~/code/orca/projects/agents/src/agents/` and served via the Orca MCP (`orca_get_agent`). There are no file-based agents in `~/.claude/agents/`.

To invoke any agent:
1. Call `orca_get_agent(name="<agent-name>")` via the Orca MCP
2. Spawn `Agent(general-purpose, prompt="<agent instructions>\n\n<your task>")`

# Key Entry Points

| Agent | Role | When to use |
|-------|------|-------------|
| **wolf** | Orchestrator | Route here when the task spans multiple domains or you are unsure where it belongs |
| **lynx** | Task planner | Breaking complex work into discrete tracked steps |
| **otter** | I/O sub-orchestrator | File operations, session logging, specialist delegation |

# Otter's Specialists

Otter delegates to these agents — do not invoke them directly unless Otter is unavailable:

| Specialist | Domain |
|-----------|--------|
| **owl** | Read and explain code — what does this do, how does X work |
| **crow** | Write or implement code — execute mode only |
| **raven** | Write to memory vault — remember this, save this note |
| **bloodhound** | Find files, resolve paths, load filesystem context |
| **ibis** | Documentation consistency — does this README match the code |

# Delegation Rules

- Delegate to wolf when the task is open-ended or cross-domain
- Delegate to otter when you need file I/O, session logging, or specialist work
- Delegate to lynx when the task needs a tracked implementation plan
- Use Glob/Grep/Read directly for simple targeted lookups — no delegation needed
- Use the Agent tool with `subagent_type: general-purpose` for all agent invocations

# Narrating Delegation

When delegating to Otter, write `Orca: "Otter, ..."` first, then call the Agent tool. When Otter returns, present its actual output as `Otter: "..."` — never fabricate the response.
