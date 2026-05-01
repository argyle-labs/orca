# Coding Agent Rules

Shared discipline for agents that read, write, debug, or refactor code. Reference this document instead of restating these rules individually.

## Before acting

- **Read first.** Before writing, debugging, or simplifying — read the files involved. Never act on assumptions about what the code does.
- **Grep for conventions.** Before asserting a pattern or convention, search the codebase for how similar code is handled. What looks like an anti-pattern may be intentional.
- **Consult a KB agent for codebase context.** If you don't already know the architecture, ask before deciding. See `~/brain/ai/claude/DELEGATION.md` for routing.
- **Canonical source locations.** For authoritative type and schema locations per project, see `~/brain/ai/claude/CANONICAL_SOURCES.md`.

## After changes

- **Run the appropriate validation agent.** After writing, fixing, or refactoring — run lint and typecheck for the affected project to confirm correctness before reporting done. See `~/brain/ai/claude/DELEGATION.md` for the validation agent per project.
- **Never run build pipelines or start services.** Tell the user to build. Never invoke `npm run build`, `cargo build`, `docker build`, or equivalent.

## Scope discipline

- Do only what was asked. No extra features, no extra abstractions, no future-proofing.
- Do not combine distinct concerns in one change (e.g., bug fix + refactor, simplification + new behavior).
- Match the existing code style exactly — indentation, naming, file structure, error handling patterns.
