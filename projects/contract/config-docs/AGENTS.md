---
name: AGENTS
description: Agent roster, delegation model, and how to invoke agents via MCP
---

# Agent System

Agents are defined in `~/code/argyle-labs/orca/projects/agents/src/agents/` and served via the Orca MCP (`orca_get_agent`). There are no file-based agents in `~/.claude/agents/`.

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

# Agent Backend Selection

Each `run_agent` invocation routes to one of three backends based on a global setting plus optional per-agent overrides.

## Modes

| mode     | behavior |
|----------|----------|
| `local`  | LM Studio first; on failure (unreachable, no chat model loaded, mid-call error) **falls back to delegating to Claude Code** so the caller's task continues. |
| `claude` | Always Claude. See "Claude path" below. |
| `hybrid` | Per-agent override; agents with no override default to Claude. The Local fallback applies whenever the resolver picks Local. |

**Server-side Anthropic remains hard-fail**: when `use_server_anthropic = true` but no key is configured, that's an error — the user asked for it, configuration is broken.

## Claude path

When the resolver picks Claude, two sub-paths:

- **Default — delegate to caller.** `run_agent` returns a JSON envelope `{ action: "delegate_to_claude_code", agent, agent_prompt, task }`. The calling Claude Code session is expected to invoke `get_agent` + `Agent(general-purpose)` itself. The orca server makes no Anthropic API calls.
- **Opt-in — server-side Anthropic.** When `agent_backend.use_server_anthropic = true` AND an API key is stored in the encrypted orca DB (SQLCipher), the server makes the Anthropic call directly. If the toggle is on but no key is present, that's an error — the user asked for it, configuration is broken.

Whichever Claude model is currently configured is used; no model id is hardcoded in the resolver.

## MCP tools

| tool | purpose |
|------|---------|
| `agent_backend_status` | mode, overrides, server-anthropic toggle, key presence |
| `agent_backend_set_mode` | set mode to local/claude/hybrid |
| `agent_backend_override` | set/clear per-agent override (hybrid mode only) |
| `agent_backend_use_server_anthropic` | toggle direct server-side Anthropic calls |
| `agent_backend_set_api_key` | store key in encrypted orca DB |
| `agent_backend_clear_api_key` | remove key from orca DB |
| `agent_backend_api_key_status` | report key presence (masked, never raw) |
