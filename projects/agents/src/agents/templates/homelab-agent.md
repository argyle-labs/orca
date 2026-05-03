---
name: homelab-agent
description: Template for halvor homelab agents. Provides shared path rules, SSH patterns, repo locations, and safety rules. All halvor agents inherit from this template — topology authority is badger.md.
---

# Homelab Agent Template

Use this template when building an agent that operates against the halvor homelab — SSH operations, health checks, backup validation, deployment, or infrastructure inspection.

**Topology authority:** `badger.md` is the canonical source for node IPs, VMID assignments, service locations, and network layout. Homelab agents reference badger for topology context rather than embedding their own copies.

## Frontmatter

```yaml
---
name: halvor-<role>
description: <one-line role description>
tools: Bash, Read
model: inherit
color: <pick one>
---
```

Add Glob, Grep, Write, Edit only if the agent reads/writes local files beyond single reads.

## Path rules

All paths follow CLAUDE.md path resolution:
- Local halvor repo: `$HOME/code/halvor` (Bash) or `~/code/halvor` (Read/Glob/Grep)
- Remote halvor repo: `/opt/halvor` (on freyr and baldur)
- **Never hardcode `/Users/scottkey/` or `/home/skey/`** — use `$HOME` in all Bash commands

## SSH access

```bash
ssh root@10.10.10.8   # thor — Proxmox primary
ssh root@10.10.10.7   # frigg — Proxmox secondary
ssh root@10.10.10.9   # loki — router host only, go very slow
ssh root@10.10.10.15  # freyr — media stack Docker host (VPN-routed, management SSH normal)
ssh root@10.10.10.6   # baldur — utility Docker host
ssh skey@10.10.10.6   # baldur as non-root (docker group)
ssh root@10.10.10.17  # pbs — Proxmox Backup Server
```

## Halvor repo

Local: `$HOME/code/halvor`  
Remote: `/opt/halvor` (synced to freyr and baldur via `git pull`)  
CLI: `ssh root@10.10.10.8 '/opt/halvor/scripts/halvor <cmd>'` — must run on thor

## Safety rules

- **OPNsense (loki / 10.10.10.1)**: any firewall, WireGuard, DHCP, or DNS change requires explicit user confirmation. One change at a time. See `badger.md` OPNsense Protocol for the full confirmation checklist.
- **Never run destructive commands** (`rm -rf`, format, wipe, `qm destroy`, `pct destroy`) without explicit user confirmation with the exact command shown.
- **Do not deploy if the local repo has uncommitted changes** — the remote sync will miss them.
- If a container fails to start after a change, read logs and report — do not retry blindly.
- Before any non-trivial change, state what you intend to do and wait for confirmation.

## What to include in a halvor agent

1. **Role statement** — what this agent does in one sentence
2. **What it checks / operates on** — specific services, paths, or commands in scope
3. **Step-by-step workflow** — concrete bash commands, in order
4. **Output format** — what the agent reports and how
5. **Rules** — domain-specific constraints beyond the shared safety rules above

## Output format

Use the `━━━ CATEGORY ━━━` section header format consistent with all other agents. For health sweeps, categories are CRITICAL / WARNING / OK. For detailed reports, follow the audit-report-agent.md template.

## Delegation

| Need | Agent |
|------|-------|
| Full topology reference | `@badger` |
| Security audit of configs | `@viper` |
| CI/CD, backup pipeline | `@falcon` |
| Doc accuracy sweep | `@ibis` |
| Secret/PII sweep | `@hound` |

## Compliance checklist

Before publishing an agent built on this template, verify every item:

- [ ] All bash commands use `$HOME/code/halvor` — never hardcoded `/Users/...` or `/home/...`
- [ ] Destructive commands (rm, wipe, destroy) require explicit user confirmation — stated in Rules
- [ ] OPNsense changes follow the OPNsense Protocol in `badger.md` — referenced or summarized
- [ ] Output format uses `━━━ CATEGORY ━━━` section headers
- [ ] Full topology references `@badger` rather than embedding its own copy
- [ ] Agent added to wolf.md routing table
- [ ] Agent added to `~/orca/DELEGATION.md` specialist table
