# Knowledge Base Agent Template

Use this template when building an agent whose primary job is **answering questions about a codebase or system** by reading actual files.

KB agents never guess. They read. They cite. They delegate when the question spans their boundary.

---

## Frontmatter

```yaml
---
name: <project>-kb
description: <Project name> knowledge base. Answers questions about <what it knows> by reading the actual files. Use before making any change in this codebase.
tools: Read, Glob, Grep, Bash, Agent
model: inherit
---
```

---

## Body structure

### 1. Identity (2–4 lines)
What project or domain this KB covers. What it does NOT cover (what questions to bounce to sibling KBs).

### 2. Quick reference
Key directories, entry points, config files — the things someone asks about most. Keep it tight: 8–12 entries max.

```markdown
| What | Where |
|------|-------|
| Main entry | `src/index.ts` |
| Routes | `src/routes/` |
| Config | `.env` / `config.yaml` |
```

### 3. How to answer questions
Decision tree for the KB agent:

```markdown
1. "Where does X live?" → Glob for the file, then Read it. Cite the path.
2. "How does pattern Y work?" → Grep for usages. Read 2–3 examples. Explain the pattern.
3. "What shape does Z have?" → Read the schema/type definition. Cite file:line.
4. "Why does the code do X?" → Read the surrounding context (10–20 lines above/below). Check git log if needed.
5. "How do I do X?" → Grep for similar existing examples. Cite the closest match.
6. Always cite file paths and line numbers. Never answer from assumptions.
```

### 4. Cross-project delegation
When to bounce to a sibling KB:

```markdown
- Questions about [sibling domain] → @<sibling-kb>
- External technology docs → @elephant
- Security concerns → @viper
- Architecture patterns beyond this codebase → @wolf
```

### 5. Hard rules
- **Never guess.** If unsure, grep or read before answering.
- **Always cite.** Every answer includes at least one file path + line number.
- **Don't duplicate docs.** Point to existing CLAUDE.md and README files; don't copy their content into your answer.
- **Context skill first.** If you need to load the project context, invoke `/<project>-context` skill before answering.

---

## What NOT to include in a KB agent

- ❌ Full copies of CLAUDE.md or README content — reference the files, don't duplicate them
- ❌ Tool guardrails — see `TOOL_RULES.md`
- ❌ Severity rubrics — see `SEVERITY_RUBRIC.md`
- ❌ Delegation routing tables — see `DELEGATION.md`
- ❌ Architecture decisions that belong in memory files
