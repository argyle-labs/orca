# Tool Usage Rules

Standard guardrails for all agents. Agent files reference this document instead of repeating these rules individually.

## Write and Edit

- **Read first.** Before writing or editing any file, read its current contents. Never overwrite blindly.
- **One file, one confirmation.** Never batch multiple file edits into a single confirmation. Each file change is its own confirm/apply cycle.
- **Edit over Write.** Use Edit for modifying existing files — it sends only the diff. Use Write only for new files or complete rewrites.
- **Verify after.** After any write or edit, re-read the affected section to confirm the change is correct and complete.
- **No unsolicited changes.** Never modify a file that was not explicitly targeted by the current task.

## Bash

- **Read-only commands** (ls, grep, cat, ps, git status, git log, git diff): run freely.
- **Mutation commands** (rm, mv, cp, git add, git commit, git push, kubectl apply, docker run, any `>` or `>>` redirect): show the exact command first and get explicit user confirmation before running.
- **Destructive commands** (rm -rf, git reset --hard, drop table, wipefs, mkfs, etc.): blocked by system hooks. If a hook blocks the command, investigate and fix the underlying issue — do not bypass hooks.
- **Never skip hooks** (--no-verify, --force-with-lease, etc.) unless the user explicitly requests it.

## Agent

- **State the plan first** for any multi-agent sequence. Do not fire a chain of agents without telling the user what you are doing.
- **Parallel only when independent.** Parallel agents are efficient; parallel agents stepping on each other are a bug.
- **Synthesize, do not parrot.** When an agent returns findings, interpret and summarize — do not relay raw output verbatim.
- **Narrate handoffs.** When delegating, say who is going where and why. When results return, say what was found.

## Dispatch discipline

How to shape every Agent/subagent call. The orchestrator decomposes; workers do one thing and return.

1. **One subtask per agent.** A dispatch prompt names a single deliverable with a single return shape. If you wrote "and" or "then" in the prompt, split it into separate agents.
2. **Bounded scope = quick return.** Each agent does ONE pass and returns a short, reviewable result. "Implement all of X, then validate, then fix what broke" is the orchestrator's loop, not a worker's job — never hand a worker a multi-phase marathon.
3. **Fail fast, surface up.** An agent that hits a blocker (won't compile, ambiguous spec, missing file) returns the blocker immediately. It does NOT grind, retry endlessly, or widen scope to "fix" around it. The orchestrator decides the next move.
4. **Fan out independent subtasks in parallel.** N atomic agents in one message beat one agent doing N things. Never serialize work that has no dependency between the steps.
5. **The orchestrator owns sequencing, validation, and integration** — not the workers. Workers produce; the orchestrator composes and verifies.
6. **No two concurrent agents may write the same files.** Partition by file or directory before fanning out; if two subtasks must touch the same file, serialize just those.

## Modification policy

Agents that modify state (write files, edit files, move files, delete files, apply fixes) do so:

1. **One item at a time** — never batch multiple changes into one confirmation
2. **With explicit user confirmation per item** — prior approval does not carry forward to the next item
3. **With verification after** — re-read or re-run to confirm the change took effect correctly

This policy applies to bear, ferret, ibis, swift, magpie, jackdaw, crow, raven, and any agent that writes or edits files. It is not restated in those agent files — this document is the authoritative source.

## Read-only agents

These agents never modify files under any circumstance. They read, analyze, and report — nothing more.

- **owl** — reads and explains code
- **hound** — privacy and PII sweep
- **otter** — integration contract validation
- **mongoose** — adversarial plan review
- **kestrel** — coverage audit (agents and hooks)
- **elephant** — external tech docs and knowledge
- **osprey** — escalation judgment

These agents reference this section instead of restating "never modify files" in their own rules.
