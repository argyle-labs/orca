# brain

Context-first AI agent orchestrator. Local-first, with Claude escalation.

## Install

```sh
cargo build --release
cp target/release/brain ~/.local/bin/brain
```

## Usage

```sh
brain                    # interactive session (auto-detects project from cwd)
brain halvor             # interactive session with project context
brain run -a fox "why is this failing?"   # one-shot agent delegation
brain escalate "security question" --project halvor  # ask Claude directly
```

## Commands

| Command | Description |
|---------|-------------|
| `brain` | Start interactive session (local model) |
| `brain <project>` | Start session with project context loaded |
| `brain run -a <agent> "<prompt>"` | One-shot: delegate to a specialist agent |
| `brain escalate "<question>"` | Ask Claude directly (requires API key) |
| `brain agents` | List available agents |
| `brain projects` | List projects (memory dirs in brain vault) |
| `brain log search "<query>"` | Search all session logs |
| `brain log sessions` | List recent sessions |
| `brain log recall <id>` | Replay a session |
| `brain doctor` | Validate agents, config, symlinks |
| `brain login` | Store Anthropic API key in macOS Keychain |
| `brain auth` | Check API key and LM Studio connectivity |
| `brain audit [path]` | Run Bear audit on a project |

## Interactive commands

| Command | Description |
|---------|-------------|
| `/model` | List models / interactive picker |
| `/model <spec>` | Switch model (e.g. `claude-sonnet-4-6`, `lmstudio:qwen3`) |
| `/flag [note]` | Mark last message as important |
| `/search <query>` | Search session logs |
| `/sessions` | List recent sessions |
| `/recall <id>` | Replay a session |
| `/escalate <question>` | Send to Claude, inject answer into context |
| `/context` | Show messages, tokens, context window % |
| `/tokens` | Token ledger |
| `/narration` | Toggle Brain/Pinky narration |
| `/cleanup` | Kill orphaned brain processes |
| `clear` | Clear conversation history |
| `exit` | Quit |

## Architecture

```
src/
  main.rs        CLI entry, subcommands, project detection
  session.rs     Interactive REPL, chat loop, agent delegation
  config.rs      Config loading (brain vault, API keys, LM Studio URL)
  context.rs     Project context resolution + system prompt assembly
  agents.rs      Agent loading (filesystem + build-time embedded fallback)
  log.rs         Session logging (JSONL), search, recall
  ledger.rs      Token usage tracking
  types.rs       Message, ToolCall, ToolResult, BackendResponse
  auth.rs        macOS Keychain API key storage
  backend/
    mod.rs       ModelBackend trait
    lmstudio.rs  LM Studio OpenAI-compatible backend (default)
    claude.rs    Anthropic Messages API backend (escalation)
  tools/
    mod.rs       ToolRegistry + definitions (read, write, edit, glob, grep, bash, confirm, delegate)
    bash.rs      Bash execution with permission prompts + timeout
    fs.rs        File read/write/edit
    search.rs    Glob + grep
build.rs         Embeds agent .md files into the binary at compile time
```

## Agents

19 specialist agents loaded from `~/brain/ai/claude/agents/`. Wolf is the orchestrator
and delegates to specialists via the `delegate` tool.

| Agent | Role |
|-------|------|
| wolf | Orchestrator — routes to the right agent |
| owl | Read and explain code |
| fox | Debug — trace root cause |
| crow | Write code |
| spider | Simplify, find abstractions |
| bear | Critical review + system audit |
| ferret | Code standards enforcement |
| elephant | External docs (TS, React, Next.js, etc.) |
| hawk | Inspect running containers |
| mole | Inspect machine processes and ports |
| badger | Halvor homelab operations |
| boar | BOD dev environment (carl CLI) |
| raven | Take notes, write to brain vault |
| pinky | Session scribe — logs, search, recall |
| lynx | Task planner — minimal agent chain |
| magpie | Scope graduation — promote project rules to global |
| oracle | Escalation judge — local vs Claude |
| scribe | Documentation consistency |
| smith | Agent file maintenance — self-repair |

## Model policy

Local models (LM Studio) run everything by default. Claude is escalation-only,
gated by Oracle. Use `brain login` to store an API key for escalation.

## Brain vault

The brain vault at `~/brain` (symlinked from `~/dotfiles/obsidian/`) stores:
- Agent definitions: `ai/claude/agents/*.md`
- Session logs: `ai/claude/logs/sessions/*.jsonl`
- Project memory: `ai/claude/memory/<project>/`

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `ANTHROPIC_API_KEY` | (keychain) | API key (env var overrides keychain) |
| `LMSTUDIO_URL` | `http://localhost:1234` | LM Studio server URL |
| `BRAIN_LOG` | (off) | Tracing filter (e.g. `debug`) |
| `BRAIN_AGENTS_DIR` | `~/brain/ai/claude/agents` | Override agents dir at build time |
