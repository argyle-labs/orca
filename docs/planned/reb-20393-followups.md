# REB-20393 long-term followups

Captured during REB-20393 (CI2 ProductCache → CI4 migration) on 2026-06-05. These are issues discovered while operating the orca agent fleet against the rebuy multi-repo workspace; they are tracked here so they survive the closure of the active session.

## 1. Wolf agent cannot dispatch sub-agents

**Observed:** During REB-20393 validation, the dispatching pattern was Orca → Wolf → Crow (Wolf orchestrates, Crow writes code). Multiple times in a single workflow, Wolf reported it had no `Agent` tool exposed and could not actually dispatch Crow as a sub-agent — only Read/Bash/Write/Edit/WebFetch/WebSearch were available to it.

**Impact:** Wolf either falls back to executing code-write work itself (defeating the specialization split and bypassing Crow's writing rules) or it pauses and asks Orca to re-dispatch a fresh Crow at the same level Wolf was operating at. Both modes burn round-trips and erode the Orchestrator → Specialist contract.

**Audit result (2026-06-05, wren):** Wolf's frontmatter already lists `Agent` in both `~/.claude/agents/wolf.md` and `~/code/orca/projects/plugins/agents/src/agents/wolf.md`. Every other orchestrator-class agent (otter, orca, bear, falcon, ferret, shrew, swift, viper, ibis) also already declares `Agent`. Lynx is the only one without, and that's intentional — lynx hands off to wolf via the user, not via subagent dispatch.

So the runtime gap is NOT in agent definitions. Likely root causes to investigate:

1. **Stale in-memory agent registry** — Claude Code may load an older Wolf definition at session start and not pick up `orca install` updates until the session restarts.
2. **Task-tool allowlist upstream** — the Task tool's allowed-subagent allowlist (in Orca's MCP server or the Task tool's own config) may gate dispatch independently of the child agent's `tools:` frontmatter.
3. **No transitive dispatch rights** — Wolf having `Agent` in frontmatter advertises capability, but the runtime may require the *parent* agent (the one that invoked Wolf) to explicitly grant child-dispatch. If Orca dispatches Wolf, and Wolf has `Agent`, can Wolf dispatch Crow? Verify the build's dispatch semantics.

**Verification step:** after the next `orca install` cycle and a fresh Claude Code session, dispatch Wolf with a code-writing task and confirm it can call `Agent(subagent_type='crow', ...)`. If it still fails, the bug is in the dispatch layer (Task tool config / MCP allowlist / runtime registry), not the agent files.

**Touch points:** Claude Code Task-tool config, Orca MCP server agent dispatch handling, session registry refresh behavior on `orca install`.

## 2. Rebuy `Libraries/Services/Api/V1/ProductSearch/` belongs under `Products/`

**Observed:** In `rebuy-core-ci4/src/Libraries/Services/`, the directory tree includes:

```
Libraries/Services/
├── Api/V1/ProductSearch/
│   ├── LegacyProductSearchService.php   (being renamed → ProductSearchService.php)
│   └── ProductSearchBodyBuilder.php
├── ApiEndpointsInterface.php
├── EngineVersion.php
├── ProductEngineClient.php
├── ProductEngineDispatchConfig.php
└── Products/
    ├── BustResult.php
    ├── CollectionProductsPipeline.php
    └── …
```

Product search is conceptually a product capability. `Api/V1/` is a misleading namespace — these are service classes, not API endpoints, and they have no actual V1/V2 dimension separate from the engine dispatch enum already present (`EngineVersion.php`).

**Proposed long-term move:** `Libraries/Services/Api/V1/ProductSearch/` → `Libraries/Services/Products/Search/` (or `Products/ProductSearch/`). Out of scope for REB-20393 — a follow-up structural refactor.

**Why deferred:** REB-20393 already touches every consumer of these classes. A namespace move stacked on top would expand the diff surface and the regression surface beyond what the validation harness can cover in one pass. Land REB-20393 first, then refactor structure.

**Touch points:** rebuy-core-ci4, admin-api consumers, apiv2 consumers. Composer autoload + PSR-4 path mappings will need to follow.

## 3. Carried-forward bugs must be fixed-with-toggle, not silently propagated

**Observed:** Multiple "Bug-for-bug copy of …" docblocks across the REB-20393 changes preserve known CI2 bugs verbatim for parity. Example: `Reb20393Helpers::isOos` preserves CI2's "zero-variants-is-OOS" edge case because the original loop body never executes when there are no variants, leaving the count comparison `0 === 0`.

**Rule (encoded in agent memory as `feedback-carried-forward-bug-toggle-pattern`):** every "Bug-for-bug copy" annotation during this migration must be converted to:

1. Fix as default behavior (in this case: zero variants ≠ OOS — emit a clear value).
2. Narrow constructor/config toggle (default OFF) that re-enables the legacy buggy behavior for parity tests only.
3. `@deprecated` annotation + sunset condition pointing to the legacy endpoint retirement.
4. Entry in `~/code/rebuy/planning/REB-20393/CARRIED-FORWARD-BUGS.md`.

**Fix direction:** sweep every `Bug-for-bug` / `// preserves CI2 …` / `// matches legacy …` annotation in the REB-20393 changeset and apply the pattern. Don't ship a single carried-forward bug without a toggle and a removal anchor.

**Touch points:** all four REB-20393 branches; sweep grep pattern `'(?i)bug.?for.?bug|preserves (legacy|CI2)|matches (legacy|CI2)'`.
