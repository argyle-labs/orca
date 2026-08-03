# Documentation Guidelines

How docs are written in this repo. Every doc under `docs/`, every crate-level
`//!` module doc, and every Markdown file in `projects/**` follows these rules.
The goal: **the repo teaches you how to work in it** — what the code standards
are, where things live, and what the core concepts are — without ever going
stale enough to mislead.

## The prime directive: code wins

Docs describe the code; they never *are* the code. When a doc and the source
disagree, the source is right and the doc is a bug. Two consequences:

1. **Link, don't duplicate.** Never paste a fragile code body — a `struct`, an
   `enum`, a function, a `Cargo.toml` block, a route table — into prose. Pasted
   code drifts the moment someone edits the real file, and nothing flags it.
   Instead **reference the source by path** (and symbol), and describe *what it
   does and why*, which is the part that doesn't rot:

   > ✅ The command surface is the `Command` enum in
   > [`projects/server/src/main.rs`](../projects/server/src/main.rs) — one
   > variant per subcommand.

   > ❌ ```rust
   > enum Command { Serve { .. }, McpServe, ... }   // will drift
   > ```

   A *short* illustrative snippet (2–5 lines) that is clearly generic — not
   copied from a specific source file — is fine when it teaches a pattern.
   Anything pinned to `file.rs:NN` line numbers is a maintenance liability;
   prefer symbol names (`fn resolve_reachable`) over line pins.

2. **Verify before you write.** Every path, crate name, tool name, verb, flag,
   and port in a doc must be checked against the tree at write time. If you
   can't verify it exists, it doesn't go in.

## Describe what is, not what isn't

Docs state what a thing **is**, how it **works**, and what a contributor **needs
to know** — in the present tense. They do not narrate history, absence, or
contrast. A reader wants the current shape of the system, not a changelog of how
it got here.

Cut these constructions and rewrite affirmatively:

- History / negation: "was removed", "no longer", "used to", "formerly",
  "previously", "since extracted", "deprecated", "retired".
- Contrast with a past design: "instead of the old X", "replaces Y", "unlike
  before", "rather than the previous".
- Absence for its own sake: "there is no X", "core embeds no Y", "not baked in".

Keep the *fact*, drop the *negation*. Reframe by naming the mechanism that
exists:

> ❌ Agents are not baked into the binary; core embeds no roster — `orca install`
>    no longer writes them.
> ✅ The agent roster is supplied by plugins over the `agents.register`
>    capability; `orca agents install` materializes it into `~/.claude/agents/`.

> ❌ Dispatch is no longer a hand-written match in `mcp/handlers.rs`.
> ✅ Each `#[orca_tool]` function is projected to CLI, HTTP, and MCP by
>    [`dispatch`](../projects/dispatch).

The rare exception is a doc whose *subject* is a migration (a design RFC's
"Motivation", a `Status: Snapshot` record) — there, past state is the content
and is allowed, as long as the doc says so up front.

Every non-reference doc should answer, for its topic: **what is it, how does it
work, and what do I need to know to work on it.**

## This repo only

Docs describe **this** repository. They never carry:

- **Personal / homelab specifics** — real hostnames (`loki`, `willow`, …), IP
  addresses, MAC addresses, personal usernames, `~/personal/...` paths. Use
  neutral placeholders (`<host>`, `10.0.0.0/24`-style RFC-1918 examples only
  when a concrete number is unavoidable, `<user>`).
- **Content that belongs to another repo.** Frontend lives in the `peacock`
  plugin; agent rosters live in the `agents` plugin. Point to those repos;
  don't document their internals here. See
  [docs-reflect-this-repo](../CONTRIBUTING.md).

## Structure conventions

- **Every doc opens with one sentence** stating what it is and who it's for,
  then a one-line `Status:` marker when the doc is a living record, a design
  RFC, or a dated snapshot (`Status: Living doc`, `Status: Proposal`,
  `Status: Snapshot 2026-08-02`). RFCs and snapshots are allowed to describe
  not-yet-built or point-in-time state **as long as they say so**.
- **`docs/README.md` is the map.** Any new doc gets one line there, in the
  right section. If it's not in the index, it doesn't exist.
- **Runbooks** list real, copy-pasteable commands that exist today. A command
  in a runbook is a promise it works — verify each against the CLI / `Makefile`
  / tool surface.
- **Internal links are relative and must resolve.** Broken links are treated as
  build breakage.

## Currency

- **Stale means fix it now, not flag it.** If you touch a doc and find it wrong,
  correct it in the same pass.
- **Roadmaps self-expire.** When a roadmap item ships, move it out of "planned."
- **No superseded architecture presented as current.** The plugin model is
  out-of-process (subprocess), not in-process cdylib/`abi_stable`; dispatch is
  the `#[orca_tool]` macro, not a hand-written match. Docs that still teach the
  old model are wrong, not merely dated.

## The three things a newcomer's docs must answer

Onboarding docs (`docs/dev/`, `docs/learn/`, this file, `docs/README.md`) exist
to answer, in order:

1. **What are the concepts?** — the four-role binary, tool surfaces, domains,
   plugins-out-of-process, the thin-`db`/domains-own-tables split.
2. **Where do things live?** — the crate map
   ([`CRATE_RESPONSIBILITIES.md`](../CRATE_RESPONSIBILITIES.md)) and repo layout
   ([`repo-structure.md`](repo-structure.md)).
3. **What are the standards?** — [`CONTRIBUTING.md`](../CONTRIBUTING.md) and the
   coding rules in [`projects/contract/config-docs/`](../projects/contract/config-docs/).

If a change alters any of those three, the corresponding doc changes in the same
PR.
