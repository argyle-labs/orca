# 00 — What orca is

> **Prerequisites:** none. **Canonical sources:**
> [`../architecture.md`](../architecture.md),
> [`../../CRATE_RESPONSIBILITIES.md`](../../CRATE_RESPONSIBILITIES.md).

## The one-sentence version

orca is a single Rust binary, running on every system in a homelab, that
exposes one set of verbs through three surfaces — and those verbs manage the
whole fleet rather than just the host they ran on.

Three claims in that sentence do real work. Take them one at a time.

## Claim 1 — One binary, every host

There is no server package and client package. The same `orca` executable is
the daemon, the CLI, the mesh peer, and the embeddable library. Which of those
it acts as depends on how it was invoked, not on which build you installed.

Why it matters to you as a learner: when you read `projects/system/src/install.rs`
you are reading code that runs on the machine being installed *and* on the
machine driving the install. There is no "agent side" to go look for.

See [`../single-binary.md`](../single-binary.md).

## Claim 2 — One verb, three surfaces

A domain crate declares a verb once, with the `#[orca_tool]` macro. That one
declaration emits:

| Surface | What you get |
|---|---|
| CLI | `orca <noun> <verb>` as a clap subcommand |
| REST | `/api/v1/<tool>` on `:12000` / `:12443`, and an OpenAPI entry |
| MCP | a JSON-RPC tool an agentic client can call |

The macro is the *sole* emitter of the OpenAPI path entries, which is why the
spec is never hand-maintained and never drifts from the code.

This is the single most important structural fact about the codebase. It means
**`projects/server` is thin on purpose** — it is transport only. If you go
looking for "where the API is implemented", you will not find it in the server
crate; it is in whichever domain crate owns that noun.

The web dashboard is deliberately *not* a fourth surface. It is an
out-of-process plugin (**peacock**) that consumes the REST surface like any
other client. That constraint is what keeps the API honest: anything the UI
can do, the CLI and MCP can do, because there is nowhere else to put logic.

Count of `orca_tool` references across the workspace, as of this writing: 343,
concentrated most heavily in `projects/system` — lifecycle is the largest
domain.

## Claim 3 — Fleet-wide, not host-local

A verb is addressed to a *system*, and the mesh routes it. You do not SSH to
a host to run orca there. Each host has a stable identity anchored to
`/etc/machine-id`; pairing establishes mutual mTLS trust on port `12002`;
after that, any peer can dispatch a verb to any other paired peer.

The consequence for how you work: there is no `--peer` flag and no peer
concept in the user-facing surface. You name the system by id and orca decides
whether that means local execution or a mesh hop.

See [`../mesh.md`](../mesh.md).

## The ports, once

| Port | Protocol | Purpose |
|---|---|---|
| 12000 | HTTP | REST + MCP-over-HTTP + browser UI |
| 12443 | HTTPS | the same, with TLS |
| 12002 | mTLS | mesh: peer dispatch and replication |

All three are per-host configurable. Resolution order is env var, then the DB
config store, then the compile-time default — so always call `http_port()`,
never the constant. That precedence (`projects/db/src/ports.rs`) is a pattern
you will meet again: **env overrides DB overrides default**, repo-wide.

## Where the code lives

Every crate is under `projects/`, with a flat package name and no `orca-`
prefix. The organizing principle is domain ownership: a domain crate owns its
concept *and its tables*. There is no storage layer and there are no
`*-store` crates — `db` holds only the pool, migrations, and replication.

Dependencies point one direction along a single acyclic spine:

```
database, utils → contracts → sdk → identities
  → { authentication, authorization } → hosts → systems
    → capabilities (deployments, models, agents, files, specs,
                    notifications, mcp, media, secrets, storage, configs)
      → server → topology
```

A crate may depend on anything strictly below it and nothing at or above.
`topology` sits at the top precisely because nothing depends on it.

Note the workspace is mid-migration toward this 23-crate target from ~36
crates, so the directory listing under `projects/` is currently longer than
the spine above. `CRATE_RESPONSIBILITIES.md` is the north star for where code
*belongs*; the directory is where it *is*.

## Exercises

1. Confirm the binary and its version:
   `orca --version`
2. Pick any noun from `orca --help` and find the crate that owns it:
   `grep -rl "orca_tool" --include="*.rs" projects/ | xargs grep -l "<noun>"`
   You should land in a domain crate, never in `projects/server`.
3. Ask the live daemon for the fleet's shape: `orca system list`. Each row is
   a host running this same binary.
4. Open `projects/db/src/ports.rs` and trace `http_port()`. Name the three
   resolution tiers without re-reading this lesson.

## Checkpoint

You can answer these before moving on:

- Why is `projects/server` thin, and where does API logic actually live?
- Why is the web UI a plugin rather than part of core?
- What makes the OpenAPI spec impossible to drift from the code?
- What does "a domain owns its own tables" rule out?

Next: `01-the-tool-macro.md` — following one verb from macro to all three
surfaces.
