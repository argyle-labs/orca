# Orca Install Runbook

The operator-facing how-to.

Onboarding a host has three phases:

```
   1. install          2. discovery        3. enrollment
   ──────────          ────────────        ─────────────
   one command         automatic           operator pastes
   on the new host     mDNS broadcast      one-time token
                                           on a pod member
```

After phase 1 the host runs orca **locally** (the orca-native secret
store is usable immediately). After phase 3 it's a full pod member.

---

## Phase 1 — Install (one command, universal)

Same verb on every platform; platform adapters fill in OS-specific bits
internally.

```sh
curl -fsSL https://install.orca.sh | sh
# or, against a release tarball:
sh scripts/install.sh --version vX.Y.Z
```

What install does:

1. Detects platform (OS / libc / arch / init / pkg mgr).
2. Verifies the binary signature.
3. Creates the `orca` service user (`/var/lib/orca`, no sudo, linger on systemd).
4. Installs binary + platform service unit (systemd / OpenRC / launchd / rc.d / procd / Unraid go-file).
5. Installs **minimal daemon prerequisites**: NTP (chrony or systemd-timesyncd), firewall holes for `:12000` `:12443` `:12002`, base packages adapters need (`nfs-common`, `qemu-guest-agent`, etc).
6. Generates the **host-identity-derived key** for the orca-native secret backend. Orca-native is usable locally immediately.
7. Starts the daemon. **mDNS service advertising begins now** — no enrollment required to be discovered.
8. **Prints a one-time enroll token** (default TTL **15 min**, single-use). Capture this; phase 3 needs it.

The caller is **not** responsible for the OS layer post-install — install
handles its own prerequisites. It just doesn't try to be a full
host-baseline tool.

### Re-installs are idempotent

Re-running install on a host that already has orca skips every step
whose state already matches. **It does not rotate the identity key or
the enroll token** unless you pass `--rotate`.

### Push-mode for hosts without curl / GitHub reach

```sh
scripts/deploy-host.sh root@bravo            # latest RC
scripts/deploy-host.sh root@charlie --version vX.Y.Z
```

Controller scp's the binary + install.sh into `/tmp/`, then runs
`install.sh --from-file`. Target needs only `sh`, `mv`, `chmod`,
`mkdir`, `sha256sum`/`shasum`.

### Service user (admin pubkey)

When install runs as root, it creates `orca` and writes the admin
pubkey from `--admin-pubkey` / `ORCA_ADMIN_PUBKEY` to
`/var/lib/orca/.ssh/authorized_keys`. Pass the **`.pub` file contents**,
never the private key:

```sh
ssh root@host \
  "ORCA_ADMIN_PUBKEY=\"$(cat ~/.ssh/id_ed25519.pub)\" \
   sh -s -- --version vX.Y.Z" \
  < scripts/install.sh
```

Verify:

```sh
ssh "orca@$HOST" 'whoami'
ssh orca@$HOST '~/.local/bin/orca --version'
ssh orca@$HOST '~/.local/bin/orca daemon status'
```

---

## Phase 2 — Discovery (automatic)

From any existing pod member:

```sh
orca pod discover              # all candidates + members on the segment
orca pod discover --unenrolled # just candidates waiting on enrollment
orca pod discover --known      # candidates whose peer_id matches a prior roster entry
```

mDNS broadcasts start at install; the new host appears within seconds.
No flag, no command on the new host required.

If the new host won't appear: assumption is a trusted L2 segment. mDNS
across VLANs requires an mDNS reflector (Avahi `enable-reflector=yes`
on the gateway). On hostile networks, install with `--no-mdns` and
enroll by direct IP.

---

## Phase 3 — Enrollment (operator pastes the token)

```sh
orca pod add <new-host-name-or-ip> --token <oob-token>
```

What happens:

1. Token validated (single-use, TTL-bound).
2. mTLS cert exchange — new host gets a peer cert minted by the pod CA.
3. Pod roster updated (CRDT replicates to all members).
4. **Identity-key escrow** — the host's identity-derived key is split
   k-of-n across enrolled peers for DR. Install does **not**
   escrow; enrollment is the only place this
   happens.
5. Reconcilers in scope for this host begin operating.

### If the token expired

```sh
# On the new host (run as the orca user):
~/.local/bin/orca system pair-token show       # current valid token
~/.local/bin/orca system pair-token rotate     # mint a fresh one
```

### Re-enrollment (host was wiped, machine-id preserved)

```sh
orca pod discover --known                       # sees the host as previously known
orca pod rejoin <peer_id> --token <new-token>   # recovers escrowed identity key
```

If `/etc/machine-id` was rotated, the host enrolls clean; the old
roster entry remains as an audit tombstone.

---

## Verify a fully onboarded host

```sh
ssh orca@host '~/.local/bin/orca daemon status'
curl -sS http://host:12000/api/health        # {"ok":true}
orca pod list                                 # new host is enrolled=true, healthy
```

Expect `listening on 0.0.0.0:12002 (mTLS)` in the journal.

---

## Upgrades

Re-run the same install path with a newer `--version`:

```sh
ssh root@host "ORCA_ADMIN_PUBKEY=\"$(cat ~/.ssh/id_ed25519.pub)\" \
  sh -s -- --version vX.Y.Z" < scripts/install.sh
ssh orca@host 'systemctl --user restart orca'

# Or push-mode:
scripts/deploy-host.sh root@host --version vX.Y.Z
ssh orca@host 'systemctl --user restart orca'
```

`daemon install` does not need to re-run unless the unit shape changed.
Once `orca host update apply` lands (ROADMAP §1.2), this becomes a
single verb.

---

## Channel pinning

`install.sh` writes `~/.orca/channel` (or `/var/lib/orca/.orca/channel`)
based on the tag shape (`-rc.` → `rc`). Pass `--prerelease` to override.

---

## Platform matrix

| Platform | Path | Daemon | Notes |
|---|---|---|---|
| Debian / Ubuntu | pull or push | `systemctl --user` + linger | Reference / best-tested. |
| Alpine | pull or push | OpenRC user-session or s6 | See [`host-setup/host-setup-alpine.md`](host-setup/host-setup-alpine.md). |
| Fedora | pull or push | `systemctl --user` + linger | SELinux contexts on `/var/lib/orca` need labeling; see `host-setup-fedora.md`. |
| Proxmox host | pull or push, root-flow | `systemctl --user` | Pairs with the LXC + VM reconciler (ROADMAP §1.1). |
| LXC (unprivileged) | pull or push | user-systemd | UID 0 inside → 100000 on host. |
| Unraid | push only | `/mnt/user/appdata/orca/bin/`, started from `go` | `/boot` path retired. |
| macOS | manual (laptop) | launchd | Full-disk-access prompt on first run for some operations. |

---

## Known gotchas

- **`GITHUB_TOKEN` required for pull mode** (releases are private).
- **`--admin-pubkey` required when first creating the orca user** — without it the controller can't ssh back as orca.
- **`PATH` on non-login shells** — always invoke `~/.local/bin/orca` by absolute path in scripts.
- **Release artifact verification** — signing scheme (cosign vs minisign) is an open decision (ROADMAP "Open decisions" §1). Today install verifies sha256 only.
- **First-boot plugin-host warning on rc.11 and earlier** — one-time fix: `orca pki ca-init && systemctl --user restart orca` as the orca user. Fixed in tree for rc.13+.

---

## See also

- [`ROADMAP.md`](ROADMAP.md) §1.3 — install + enrollment hardening exit criteria.
- [`host-setup/`](host-setup/) — per-OS manual prereqs.
