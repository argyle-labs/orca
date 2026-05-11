# rebuy-cli-context

Load context for the **rebuy-cli** codebase — the developer CLI for all rebuy projects.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```
Read: ~/code/rebuy/rebuy-cli/CLAUDE.md
Read: ~/code/rebuy/rebuy-cli/README.md (first 80 lines)
```

## Step 2 — Orient in the codebase

```bash
ls ~/code/rebuy/rebuy-cli/src/commands/    # all 27 command modules
ls ~/code/rebuy/rebuy-cli/src/core/        # core utilities
ls ~/code/rebuy/rebuy-cli/src/utils/       # shared utils
cat ~/code/rebuy/rebuy-cli/package.json    # version, scripts, deps
```

## Step 3 — Surface structured context

Return a summary with these sections:

### Stack
- Node.js 22+, TypeScript, Commander.js, Jest, Simple-git
- Version from package.json

### Command categories
List all commands from `src/commands/` grouped by function:
- Project mode switching
- Repository management (status, pull, sync)
- Environment lifecycle (start, stop, monitor)
- 1Password integration
- Bitbucket PR creation
- K8s port-forward tunnels
- Shell completion
- Dependency alerts

### Key patterns (from CLAUDE.md)
- Always use `npm run build` after TypeScript changes
- Test via globally-linked `rebuy` command
- Prefer async (`execAsync`) over sync exec
- Use `@inquirer/prompts` for interactive input
- Never hardcode paths — use `rebuyProjectDir` from config

### Dev workflow
- Build: `npm run build`
- Test: `npm test`
- Integration smoke test: `test/integration/smoke.test.ts`
- Link globally: `npm link`

### Docs to update with command changes
- `README.md`
- `ROADMAP.md`
- `docs/` directory

---

**This skill is invoked by `@rebuy-kb`. The rebuy-cli is the primary developer tool — understanding its capabilities is essential for any env management or workflow question.**
