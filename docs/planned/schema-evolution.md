# Schema evolution + parity rule

How orca changes over time without breaking the systems it manages.

The hard rule, stated up front:

> **No piece of legacy behavior is retired until orca has been
> validated as producing the same result for that capability.**

This applies to:

- Shell scripts being replaced by orca verbs.
- Plugin code being subsumed by orca built-ins.
- Config files being moved into the config repo.
- Any external tool (Uptime Kuma, manual cron entries, hand-edited
  Caddy vhosts, the meerkat MCP server, …) being decommissioned.

If you're tempted to delete an old thing because the new orca thing
"works on baldur," stop. The rule is parity, not happy-path
functionality.

---

## 1. What parity means

For each capability being retired, parity has four checks. All
four must pass on every host that uses the capability before the
old implementation is removed.

| Check | Meaning |
|---|---|
| **Functional** | Same inputs produce the same observable outputs. For a backup script: same files end up in the same place with the same permissions. |
| **Side-effect** | Same external state changes: same systemd units enabled, same packages installed, same logs emitted, same notifications sent. |
| **Failure-mode** | When inputs are bad, the new implementation fails in the same direction. If the old script exited 1 and emitted a `[backup] failed` log line, the orca verb should too. |
| **Operational** | Same operators can invoke it the same way. If `meerkat backup` ran from cron under user `svc`, `orca backup` runs the same — no surprise privilege changes, no required interactive prompts. |

A capability is "at parity" when checks pass on **all** hosts that
exercise it, not just one. Mint and baldur differ enough that
"works on baldur" is not evidence the orca version is ready.

---

## 2. The parity-check workflow

For each retiring capability:

### 2.1 Inventory

Catalog every place the old behavior fires:

- Cron entries that invoke it.
- Shell scripts that wrap it.
- Plugin tools that call it.
- Documentation that says "run this."
- Manual procedures operators rely on.

Until this list is complete, parity can't be claimed — you don't
know what you're matching.

### 2.2 Dual-run period

Run the orca implementation **alongside** the legacy one for a
defined window (default: two full operational cycles — e.g., two
nightly backups, two scheduled snapshots, two weekly rotations).

Capture both outputs to disk and diff them. Any diff is a parity
gap. Common gaps to expect:

- Whitespace / formatting differences in generated config (Caddyfile,
  systemd units, fstab) — usually fine but needs explicit acceptance.
- Timestamp precision differences in log lines.
- Slightly different package versions installed (apt resolves
  differently after a year).
- Ordering of operations causing visibly different intermediate
  states even when end state matches.

Some gaps are acceptable (mark as "intentional divergence" with a
note). Others mean the orca verb isn't ready.

### 2.3 Shadow mode

Once dual-run looks clean, switch the legacy implementation to
**no-op** but leave it in place. Orca is now the only producer; the
legacy code is the canary. If the canary ever wakes up (anything
still calling it logs a warning), parity wasn't actually complete.

Shadow mode runs for at least one operational cycle.

### 2.4 Retirement

Only after shadow mode passes cleanly on **every** affected host
does the legacy code get deleted. The deletion is a single commit
that references the parity checklist in its message.

### 2.5 Rollback

For at least one cycle after retirement, keep the legacy code in
the git history at a known tag (`pre-orca-<capability>-v1`) so
rollback is `git revert` + redeploy, not archaeology.

---

## 3. Applying the rule to the meerkat migration

The phases in [orca-as-logic-layer.md](orca-as-logic-layer.md) §5
each name "retire" steps. None of those retirements happen without
the parity workflow above. Concretely:

| Retirement | Parity-critical observation |
|---|---|
| `nfs-monitor.sh` → orca nfs health | Same alert messages to ntfy. Same `WARN`/`CRIT` thresholds. Same mute behavior during planned downtime. |
| `pbs-backup-hook.sh` → orca pbs hooks | Same pre/post hook firing order. Same exit-code propagation to PBS. |
| `meerkat-agent.sh` → orca host verbs | Same operators can invoke from cron. Same sudoers footprint or smaller (never larger silently). |
| Meerkat Go binary → orca daemon | Same MCP tool names + arg shapes. Same auth surface for claude agents. |
| `plugins/{docker,nfs,…}` → orca built-ins | Tool names + arg shapes match. Any downstream that imports the plugin still works (rebuy is the canary). |
| Uptime Kuma → orca alerting | Every existing Kuma monitor reproduced in orca. Every notification channel reproduced. Run dual-alert for a full week before turning Kuma off. |

---

## 4. Config-repo schema evolution

The config repo (meerkat, or any other) is also a schema. When
orca's understanding of a TOML structure changes:

### 4.1 Three categories of change

1. **Additive** — new optional field. Old repos keep working unchanged.
   Default applied. **No special handling.**
2. **Renaming** — field renamed. Orca reads both old and new for at
   least one minor-version window; the GitOps loop logs a deprecation
   warning when the old name is present; eventually warns hard then
   removes support.
3. **Breaking** — field removed, semantics changed, or required field
   added with no default. Ships with an **in-repo migration**: orca
   emits a PR against the config repo that rewrites the affected
   files. The new orca version refuses to apply against an unmigrated
   repo (with a clear error pointing at the migration PR).

### 4.2 In-repo migrations

```
orca config migrate --from v0.7 --to v0.8 --repo /path/to/meerkat
```

Reads the repo, rewrites files in place, opens a PR via the
git-provider API (hybrid rule: provider trait — octocrab / Gitea /
GitLab — for hosted repos, libgit2 working-tree for airgap / DR /
generic remotes). Idempotent: re-running on an already-migrated
repo is a no-op.

### 4.2.1 In-repo DB migrations — concrete pattern

Orca's own SQLite schema follows the same parity rule. Migrations
live at:

```
projects/db/migrations/<YYYYMMDDHHMMSS>__<slug>.up.sql
projects/db/migrations/<YYYYMMDDHHMMSS>__<slug>.down.sql
```

Recent examples (verified 2026-06-01):

- `20260530120000__plugin_tools_namespace.{up,down}.sql`
- `20260530130000__plugins_drop_mode.{up,down}.sql`
- `20260530140000__plugins_drop_mcp_transport.{up,down}.sql`

Each schema change also mirrors the `CREATE` in `apply_schema()` in
`projects/db/src/lib.rs` — fresh installs use `apply_schema`,
existing DBs use the timestamped migration. Both must land in the
same commit; one without the other is a parity gap by definition.

Migrations are versioned with the orca release; each breaking one
ships with a test that round-trips a known fixture repo.

### 4.3 Version pinning

The config repo declares the orca version range it supports:

```toml
# config/orca.toml
[orca]
min_version = "0.8.0"
max_version = "0.9.x"
```

A daemon outside the range refuses to apply and logs a clear
upgrade-or-pin instruction. This is the safety against accidental
"orca auto-updated and broke the homelab."

---

## 5. Versioning + release discipline

- **SemVer with intent**: minor versions may rename fields with
  the deprecation window above; major versions may break with a
  required migration.
- **Migration tests**: every breaking change ships with a fixture
  repo + a passing `orca config migrate` round-trip test in CI.
- **Release notes** spell out every parity-affecting change in the
  same shape as the parity checklist (functional / side-effect /
  failure-mode / operational).

---

## 6. What this rule explicitly forbids

- "I tested it on baldur, the script's deleted."
- "It works, I'll fix the alerting later."
- "Cleanup PR to drop the legacy plugin" without a referenced parity check.
- "Schema breaking change" without an in-repo migration shipping in the same release.
- "We can always git revert" as a substitute for parity — revert
  is a tool for rollback **after** a parity gap is discovered, not
  a license to skip the gap-finding step.

---

## 7. Open questions

- **How long is "one operational cycle"?** Varies per capability —
  nightly for backups, weekly for some snapshots, hourly for
  health checks. Define per-capability when authoring the parity
  checklist.
- **Who signs off** that parity is met? In a one-operator homelab,
  the operator. In a team, a documented review step. Either way it
  goes in the deletion commit message.
- **Does dual-run cost matter?** For things like backups,
  dual-running means double the disk write. Sometimes that's
  acceptable; sometimes the parity check is "we kept the new
  output for one cycle and verified manually" without continuous
  dual-run. Acceptable variant, must be explicit.

---

## 8. Relationship to other planned docs

- [orca-as-logic-layer.md](orca-as-logic-layer.md) — every
  retirement in its phasing flows through this workflow.
- [observability.md](observability.md) — Uptime Kuma retirement
  is the canonical case study.
- [backup-restore.md](backup-restore.md) — same rule applies to
  meerkat's backup scripts → orca backup verbs.
