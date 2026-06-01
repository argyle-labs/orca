# Install + bootstrap — canonical scope

The host onboarding lifecycle is three phases — keep them straight:

```
   install            discovery           enrollment
   ──────             ─────────           ──────────
   universal          mDNS broadcast      out-of-band token
   daemon + prereqs   daemon announces    operator pastes
   host-identity      pod members         "orca pod add"
   key for native     auto-list           establishes mTLS
   secret store       candidates          trust + identity
                                          escrow

   ─ pre-enrollment ─ │ ─ pod member ─
```

- **Install** is universal across platforms (Alpine LXC, Debian bare metal, Proxmox host, macOS laptop, Unraid). Outputs: daemon running, host-identity-derived key in the orca-native secret store (usable locally immediately), mDNS service advertising, **one-time enroll token printed to install output / console**.
- **Discovery** is automatic — installed systems appear in `orca pod discover` / UI via mDNS. See [discovery-enrollment.md](discovery-enrollment.md).
- **Enrollment** is operator-driven. Operator pastes the OOB token into `orca pod add` on an existing pod member. This is **separate from** the install-time host-identity key — enrollment mints peer mTLS identity, escrows the host's identity key for DR (per [backup-restore.md](backup-restore.md) §4.4), and pulls the host into reconciler scope.

A host that has installed but not enrolled is a **candidate** — runs orca locally with local-only state, participates in no pod operations.

This doc owns the **install** phase. Discovery + enrollment own their own doc.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. What install does, exactly

Universal verb, identical surface across platforms. Platform adapters internal — caller does not branch.

1. **Detect platform**: OS / libc / arch / init system / package manager. Refuse on unknown combinations.
2. **Verify the binary signature** against the baked-in release key (cosign primary, minisign fallback — open decision in ROADMAP).
3. **Create the service user** (`orca`, `/var/lib/orca`, no sudo, linger on if systemd-user).
4. **Install the binary** and the platform service unit (systemd / OpenRC / launchd / rc.d / procd / Unraid go-file).
5. **Install minimal OS prerequisites the daemon itself needs**:
   - NTP (chrony or systemd-timesyncd) — required by cert validity, scheduler, audit, CRDT.
   - Firewall hole for the daemon ports (12000 / 12443 / 12002) on platforms with a host firewall.
   - Base packages adapters in scope need (`nfs-common`, `qemu-guest-agent`, etc.) — narrow, per-adapter, not a generic "baseline."
6. **Generate the host-identity-derived key** for the orca-native secret backend and write the encrypted store at `~/.orca/orca.db`. **The orca-native backend is usable locally immediately post-install.** No enrollment required for local-only operation.
7. **Start the daemon.** mDNS service advertisement starts here — no enrollment needed for discovery.
8. **Print the one-time enroll token.** Default TTL **15 minutes**, single-use, time-bounded. Consumed by `orca pod add` on a pod member.
9. **Report outcome.** Idempotent on failure.

### Scope boundaries

- Install handles its **own** prerequisites. The caller is **not** responsible for the OS layer.
- Install does **not** try to be a full host-baseline tool. Users, packages outside the daemon's needs, service deployments — all out of scope for install; they come from the config repo post-enrollment.
- Install does **not** escrow the identity key. Escrow happens at enrollment per [backup-restore.md](backup-restore.md) §4.4.

### Idempotent re-install

Re-running install on a host that already has orca is a true no-op:

- Binary already at target version → skip download.
- Service user exists → skip create.
- Prereqs already in place → skip.
- Systemd / init unit unchanged → skip re-render.
- Existing orca-native store + identity key → leave in place. **Do not rotate** the identity key or token unless `--rotate` is passed explicitly.
- Existing enroll token still valid → do not re-print; print "token still valid for Nm" instead.

---

## 2. Platform adapter pattern

`install.rs` is the universal entry point. Platform-specific bits live behind a `PlatformAdapter` trait:

| Adapter | Init | Pkg mgr | Service file | Binary target |
|---|---|---|---|---|
| Debian / Ubuntu | systemd | apt | `/etc/systemd/system/orca.service` (or `--user`) | `x86_64-unknown-linux-gnu` / `aarch64-…-gnu` |
| Alpine | OpenRC | apk | `/etc/init.d/orca` | `…-linux-musl` |
| Fedora / RHEL | systemd | dnf | `…/systemd/system/orca.service` | `…-linux-gnu` |
| Arch | systemd | pacman | `…/systemd/system/orca.service` | `…-linux-gnu` |
| Unraid | rc.d via go-file | n/a | `/mnt/user/appdata/orca/bin/` | `…-linux-gnu` |
| Proxmox host | systemd | apt | `…/systemd/system/orca.service` | `…-linux-gnu` |
| LXC (unpriv) | user-systemd | apt/apk | `~/.config/systemd/user/orca.service` | matches host arch |
| macOS | launchd | brew (optional) | `~/Library/LaunchAgents/sh.orca.plist` | `…-apple-darwin` |
| FreeBSD / OPNsense | rc.d | pkg | `/usr/local/etc/rc.d/orca` | `…-unknown-freebsd` |
| OpenWrt | procd | opkg | `/etc/init.d/orca` | `…-linux-musl` |

The adapter knows how to: install the service unit, install prereq packages (NTP, firewall management, adapter base packages), open daemon ports in the host firewall, persist across reboots. The orca-layer install verb is identical regardless.

---

## 3. Minimum daemon permissions

Bootstrap is minimal. Install grants only what the daemon itself needs to run + discover + accept enrollment:

| Capability | Why | Granted at install |
|---|---|---|
| Read `/proc`, `/sys` | self-metrics | yes |
| Bind ports 12000/12443/12002 | service surface + mesh | yes (CAP_NET_BIND_SERVICE if <1024) |
| Multicast for mDNS | discovery | yes |
| Write under `${ORCA_DIR}` | local state | yes |
| Docker socket / Proxmox API / NFS mount / shadow / journal etc. | integrations | **no — granted later by reconciler** when a per-host config declares the integration is needed |

This matches "Bootstrap is minimal; per-host config drives everything" (meerkat `feedback_bootstrap_minimal.md`). The reconciler adding a capability is an explicit, auditable step (visible in `orca host capabilities`).

---

## 4. What's shipped, what's missing

### Shipped

- `projects/system/src/install.rs` (~772 LOC) — platform detect, service-user, daemon-minimum permissions, install / uninstall / doctor.
- `scripts/install.sh` — one-command bootstrap entry point.
- Pair-token mint + single-use validation in `projects/pod`.
- mDNS discovery + mTLS pairing + cert rotation in `projects/pod`.

### Missing — gaps to close

- **Binary signing decision wired in.** Today `install.sh` verifies sha256 only. Pick cosign vs minisign, bake the key into install.sh + the rust verify path.
- **Non-systemd unit templates as tested fixtures.** OpenRC, launchd, rc.d, procd, Unraid go-file. Live under `projects/system/src/templates/`.
- **Idempotent re-install diff path.** Diff current vs desired; apply only the delta. No stale unit symlinks on re-install.
- **NTP install step.** Install chrony / systemd-timesyncd if not already running.
- **Firewall hole step.** ufw / firewalld / nftables / pf adapter rules for 12000/12443/12002.
- **Host-identity key generation at install.** Today the orca-native secret store is a stub.
- **OOB enroll token printing.** Replace the journal-grep flow; token must be visible in install output / console with the operator command to paste it.
- **`orca pod add` (enrollment-side verb).** See [discovery-enrollment.md](discovery-enrollment.md).
- **`--rotate` flag** for explicit re-key on re-install.

---

## 5. Open questions

- Binary signing: cosign vs minisign (carried in ROADMAP Open Decisions §1).
- Hosting `install.orca.sh`: own domain via Caddy on baldur, raw GitHub mirror as fallback.
- No-internet hosts (OPNsense strict-egress): need an "install from local tarball" mode that an already-paired peer delivers.
- macOS full-disk-access prompts on launchd — document the manual approval step.

---

## 6. Relationship to other docs

- [discovery-enrollment.md](discovery-enrollment.md) — phases 2 + 3.
- [host-lifecycle.md](host-lifecycle.md) — what happens after enrollment: drivers, OS updates, reboots, UPS-coordinated shutdowns.
- [pki-lifecycle.md](pki-lifecycle.md) — the cert exchange during enrollment.
- [backup-restore.md](backup-restore.md) §4.4 — identity-key escrow at enrollment.
- [secrets-identity.md](secrets-identity.md) — what the orca-native backend looks like.
- [../install-runbook.md](../install-runbook.md) — operator runbook (the runnable version of this).
