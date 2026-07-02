# Orca Install Runbook

The operator-facing how-to.

Onboarding a host has three phases:

```
   1. install          2. discovery        3. pairing
   ──────────          ────────────        ─────────
   one command         automatic           auto-offer over mDNS;
   on the new host     mDNS broadcast      operator runs `pod accept
                       + auto-offer        <6-char-code>` on the joiner
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
8. Generates a per-host Ed25519 **bootstrap key** and begins advertising on mDNS (`_orca._tcp.local.`) as `unclaimed`. No token to capture — pairing is offer/accept (phase 3).

The caller is **not** responsible for the OS layer post-install — install
handles its own prerequisites. It just doesn't try to be a full
host-baseline tool.

### Re-installs are idempotent

Re-running install on a host that already has orca skips every step
whose state already matches. **It does not rotate the identity key or
the bootstrap key** unless you pass `--rotate`.

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
orca pod discover --unenrolled # just `unclaimed` candidates waiting to pair
orca pod discover --known      # candidates whose peer_id matches a prior roster entry
```

mDNS broadcasts start at install; the new host appears within seconds.
No flag, no command on the new host required.

If the new host won't appear: assumption is a trusted L2 segment. mDNS
across VLANs requires an mDNS reflector (Avahi `enable-reflector=yes`
on the gateway). On hostile networks, install with `--no-mdns` and
enroll by direct IP.

---

## Phase 3 — Pairing (auto-offer, then `pod accept`)

On a shared LAN this is fully automatic up to the accept step. Any
secure pod member that sees the joiner's `unclaimed` mDNS advertisement
automatically pushes a `pod/offer` over the bootstrap channel (TLS SNI
`pod-bootstrap.orca.local`, no client cert required). The offer carries
the mesh CA cert, the pod id, and the hash of a **6-character pairing
code**; the inviter prints the code in its daemon log.

```sh
# On the joiner:
orca pod pending                # shows the incoming offer
orca pod accept <6-char-code>   # dials the inviter (cert-pinned), sends CSRs, installs signed certs
```

What `pod accept` does:

1. Dials the inviter with TLS pinned to the bootstrap pubkey from the offer.
2. Sends two CSRs (client + server) plus the raw code.
3. Receives signed peer certs minted by the pod CA and installs them.
4. Records the inviter in `pod_peers`; the joiner is now a full member.

Secrets storage on the joiner stays **off** until the user opts in with
`orca pod self-secure on`.

### Manual fallback (no mDNS)

mDNS is link-local. Across subnets, firewalled, or when you want to be
explicit:

```sh
orca pod connect <ip[:port]>    # on the joiner — asks the addressed host for an offer
orca pod offer <ip[:port]>      # on the inviter — pushes an offer to a specific address
```

Both accept `host`, `ip`, `host:port`, `ip:port`, or `[ipv6]:port`; the
default port is the orca plugin port (12002 on default installs).

---

## Verify a fully onboarded host

```sh
ssh orca@host '~/.local/bin/orca daemon status'
curl -sS http://host:12000/api/health        # {"ok":true}
orca pod list                                 # new host appears as a paired member, healthy
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
