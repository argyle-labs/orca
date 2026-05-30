# Typecheck Workflow

Standard TypeScript type-checking workflow for all typecheck agents. The agent provides project-specific commands and type sources; this skill provides the process.

---

## Workflow

### Step 1 — Run the type checker

```
<typecheck-command> 2>&1 | head -200
```

A clean compile does not mean a clean review. But a dirty compile is an automatic finding.

### Step 2 — For each error: locate and understand

1. Read the flagged file at the reported line — and 10–20 lines of surrounding context
2. Understand what type is expected vs. what is provided
3. Look up the **correct type** in the project's canonical sources (see `CANONICAL_SOURCES.md`)
4. Do not read the error message in isolation — read the code

### Step 3 — Propose a typed fix

For each error:
- State what the correct type is and where it comes from
- Show the before/after diff
- Explain why this type is correct (what invariant it captures)

```
src/payments/charge.ts:67
Error: Argument of type 'string | null' is not assignable to parameter of type 'string'

Fix:
  Before: processCharge(payment.chargeId)
  After:  if (!payment.chargeId) throw new Error('Missing chargeId')
          processCharge(payment.chargeId)

Why: chargeId is nullable in the DB schema (CANONICAL_SOURCES.md → BOD API). The
     caller must guard before passing it into processCharge which does not accept null.
```

### Step 4 — Confirm before applying

Present all findings first. Apply fixes one at a time with user confirmation.
See `TOOL_RULES.md` — modification policy applies.

---

## Hard rules

- **Never suggest `as any` or `as unknown`** unless there is genuinely no alternative and you can explain why.
- **Never hand-edit generated files.** If tsc reports errors in `*.generated.ts` or `database-generated.d.ts`, fix the generator or migration — not the file.
- **Non-null assertions (`!`) are a last resort.** Guard with a conditional check instead.
- **If the correct type does not exist yet** — that is the real finding. Create the type; don't cast around its absence.
