# PR Review Format

Standard output format for all PR review agents. Reference this skill for the output template and structure. For severity definitions, see `SEVERITY_RUBRIC.md`.

---

## Output template

```markdown
## PR Review: <branch> → <base>

**Stated intent:** <one sentence from commit messages or PR description>
**Scope:** <N> files, +<additions>/-<deletions> lines, <K> commits

---

### CRITICAL (merge blocker)
- [`path/to/file.ts:42`] <issue>. Remediation: <concrete change — proposed diff or exact edit>.

### HIGH
- [`path/to/file.ts:67`] <issue>. Remediation: <concrete change>.

### MEDIUM
- [`path/to/file.ts:12`] <issue>. Remediation: <concrete change>.

### LOW
- [`path/to/file.ts:99`] <issue>. Note: <brief explanation>.

---

### What's missing
- Tests for <scenario> — specifically: <what test is needed>
- Docs for <API change>
- Monitoring / alerting for <new behavior>

### What's done well
- <objective positives — do not pad, but do not omit>

---

### Verdict
- [ ] Approve as-is
- [ ] Approve with conditions: <list conditions>
- [ ] Request changes: <top 3 blockers>
```

---

## Rules for using this format

- Every CRITICAL and HIGH finding includes file + line + concrete remediation. No vague findings.
- Do not list a finding at MEDIUM or LOW if it is actually CRITICAL — severity must match impact.
- "What's done well" is not filler. If something is genuinely good, say it. If nothing stands out, omit the section.
- Verdict is a checkbox — exactly one should be checked.
- Scope line comes from: `git diff --stat <base>..<branch>`
- Stated intent comes from: `git log --format='%s' <base>..<branch>`

---

## Severity quick reference

See `~/brain/ai/claude/SEVERITY_RUBRIC.md` for full definitions.

- **CRITICAL** — crashes prod, corrupts data, leaks secrets
- **HIGH** — wrong in realistic conditions, missing rollback, breaking change
- **MEDIUM** — subtle bugs, missing tests, schema drift
- **LOW** — style, naming, docs
