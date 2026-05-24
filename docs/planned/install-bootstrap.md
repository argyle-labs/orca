# Install + bootstrap — greenfield host onboarding

The "first install" path for orca. Covers the chicken-and-egg of
`orca host bootstrap` (you can't run an orca verb without orca on
the host yet) and the per-platform install flow.

End state: a fresh host runs **one command**, gets the right orca
binary, sets only the minimum permissions needed to operate, and
pairs into the mesh. From that point everything else (users,
packages, services, secrets) flows from the config repo via the
GitOps loop ([orca-as-logic-layer.md](orca-as-logic-layer.md) §3.5).

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. The one command

```sh
curl -fsSL https://install.orca.sh | sh -s -- --pair-token <token>
```

(Hosted via Caddy on baldur — see [caddy-plugin-scope.md](caddy-plugin-scope.md);
mirrored on GitHub Releases as a fallback raw URL.)

What the script does:

1. **Detects the platform**: OS (linux/darwin/freebsd), libc
   (glibc/musl), arch (x86_64/aarch64/armv7), init system
   (systemd/openrc/launchd/rc.d), package manager
   (apt/apk/pacman/dnf/brew). Refuses to proceed on unknown
   combinations rather than guessing.
2. **Downloads the matching binary** from GitHub Releases,
   verifying the release's signature (cosign / minisign — pick one
   in §6 open questions) against a baked-in public key.
3. **Lays down the service user** (default: `orca`, configurable).
   Creates the user with no shell, no password, owning
   `${ORCA_DIR}` (default `/var/lib/orca`).
4. **Installs the binary** to `/usr/local/bin/orca` (or platform
   equivalent) with mode `0755`.
5. **Installs the service unit** (systemd / openrc / launchd plist)
   running as the orca user with the minimum capabilities needed —
   see §3.
6. **Starts the daemon**, which auto-pairs into the mesh using the
   `--pair-token` (single-use, expires in 15 minutes).
7. **Reports outcome** to stdout and to the pairing peer's audit
   log. On failure, leaves no state behind (idempotent).

Re-running is safe: if orca is already installed and paired, the
script reports current version and exits 0. If a newer version is
available it offers `--upgrade`.

---

## 2. Platform detection matrix

| OS | Init | Pkg mgr | Service file | Binary target |
|---|---|---|---|---|
| Debian / Ubuntu | systemd | apt | `/etc/systemd/system/orca.service` | `x86_64-unknown-linux-gnu` or `aarch64-…-gnu` |
| Alpine | OpenRC | apk | `/etc/init.d/orca` | `…-linux-musl` |
| Fedora / RHEL | systemd | dnf | `…/systemd/system/orca.service` | `…-linux-gnu` |
| Arch | systemd | pacman | `…/systemd/system/orca.service` | `…-linux-gnu` |
| Unraid | rc.d | n/a (user scripts) | `/boot/config/plugins/orca/…` | `…-linux-gnu` |
| Proxmox host | systemd | apt | `…/systemd/system/orca.service` | `…-linux-gnu` |
| macOS | launchd | brew (optional) | `~/Library/LaunchAgents/sh.orca.plist` | `…-apple-darwin` |
| FreeBSD / OPNsense | rc.d | pkg | `/usr/local/etc/rc.d/orca` | `…-unknown-freebsd` |
| OpenWrt | procd | opkg | `/etc/init.d/orca` | `…-linux-musl` (uclibc edge case — track) |

The install script encodes this matrix. Unknown combinations
print a "supported platforms" list and exit 1.

---

## 3. Minimum required permissions

The hard rule: **orca runs with the smallest privilege set that
lets it do its job on that host**. Capabilities are granted per
role; the daemon never runs as root unless a specific managed
operation requires it (and then via a narrow ambient/file
capability, not a uid=0 process).

### 3.1 Default capability matrix

| Capability | Why | When granted |
|---|---|---|
| Read `/proc`, `/sys` | Metrics (sysinfo, GPU sysfs) | Always (read-only) |
| Bind ports 12002 (mesh) and 12000 (UI) | Service | Always — use `CAP_NET_BIND_SERVICE` if <1024 needed |
| Access `/var/run/docker.sock` | Docker integration | If host has Docker — add orca user to `docker` group |
| Access Proxmox API socket | Proxmox integration | If host is a Proxmox node — group `www-data` or PVE API token |
| `pct enter`, `qm` commands | LXC/VM exec | Proxmox only — narrow sudoers entry for just these binaries |
| Read `/etc/shadow` | User-sync on secure hosts | Secure hosts only (see [orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6a). Granted via `CAP_DAC_READ_SEARCH` on the binary or group `shadow`. |
| Write `/etc/passwd`, `/etc/shadow`, `/etc/sudoers.d/`, `~user/.ssh/` | User reconciler | Secure hosts only. `CAP_CHOWN` + `CAP_FOWNER` + group `wheel`/`sudo` — never full root. |
| Manage systemd units | `orca host service install` | Where used — via `polkit` rules limited to units in the `orca.*` slice |
| Network admin (mount NFS, etc.) | NFS integration | Where used — `CAP_SYS_ADMIN` for mount(2), or shell-out to `mount` via narrow sudoers |
| Read journal | Log tailers | Group `systemd-journal` |

**Sudoers entries** (where unavoidable) are scoped to specific
binaries with no wildcards. Example, Proxmox:

```
orca ALL=(root) NOPASSWD: /usr/sbin/pct enter [0-9]*, \
                          /usr/sbin/pct exec  [0-9]* -- *, \
                          /usr/sbin/qm  guest exec [0-9]* -- *
```

No `meerkat.sh` style "single sudo-allowlisted entry point" — that
pattern collapses the privilege boundary. Per-action sudo entries
or capability grants only.

### 3.2 The install script grants only the minimum

Bootstrap is minimal. The install script grants **only** the
permissions orca needs to run as a daemon and join the mesh —
read `/proc`/`/sys` for self-metrics, bind its mesh + UI ports,
and write under `${ORCA_DIR}`. Nothing else.

```sh
curl … | sh -s -- --pair-token TOK --trust secure
```

Integration-specific capabilities (docker group membership, sudoers
for `pct`, NFS mount privileges, journal group, etc.) are **not**
granted at install. They're applied later by the reconciler when a
per-host config declares the integration is needed. See
[orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6 — install
gets the daemon running; per-host config drives everything after.

The reconciler adding a capability is an explicit, auditable step
(visible in `orca host capabilities`). Bootstrap doesn't pre-grant
or guess. If a host stops needing an integration, the reconciler
revokes the capability the same way.

### 3.3 Permission audit

`orca host capabilities` prints exactly what privileges the local
daemon currently holds and which integrations require each. Used
in security review and when promoting/demoting trust tier.

---

## 4. Future: orca manages OS users + service permissions

Once installed, orca takes over user/permission management for the
host as planned functionality. This extends [orca-as-logic-layer.md](orca-as-logic-layer.md)
§3.6 with **per-service permission grants**:

- **Linux users**: declared in `config/<host>/users.toml`,
  reconciled by `orca host users reconcile`. Already covered.
- **SMB shares allowed-users** (Unraid example): a per-share
  field in the share definition tells orca which users may access
  the share. Orca generates the Unraid SMB config and reconciles
  it — no logging into the Unraid UI to tick boxes.
- **NFS exports allowed-hosts/users**: same pattern — declared in
  the share definition, orca writes `/etc/exports`.
- **Docker socket access**: which OS users belong to the `docker`
  group is declarative.
- **PVE API tokens**: created and rotated by orca, scoped per
  integration, never shared between hosts.

The throughline: any permission grant a human would otherwise click
through becomes a row in the config repo, and orca reconciles it
the same way it reconciles a Caddy route.

This is **planned**, not in v1. It needs:

- A unified "principal" abstraction (OS user, PVE user, Unraid
  user, SMB user) so the same identity can have grants across
  multiple backends.
- Per-backend reconcilers (`orca smb reconcile`, `orca nfs reconcile`,
  `orca pve users reconcile`).

Track as its own scope doc when it's the next thing to land.

---

## 5. Bootstrapping the bootstrap — initial pairing

`--pair-token` solves the trust problem: someone with mesh access
mints a single-use token via:

```sh
orca system peer pairing create --ttl 15m
# → prints token + the URL to feed the install script
```

The new host's daemon presents the token on first connect; the
mesh validates it, exchanges certs, and the host becomes a peer
with `trust = "insecure"` by default. Promotion to `"secure"`
requires an explicit `orca system peer trust set --host X --trust secure`
from an existing secure peer.

Alternative bootstrap modes (for fully unattended provisioning):

- **cloud-init**: install script invoked from `runcmd:`, pair token
  delivered via the cloud-init `user_data` (single-use, expires
  with the boot).
- **PXE / image build**: orca pre-baked into the image; pairs on
  first boot using a token written to a known file by the imaging
  system. Image hardening removes the token file after first use.
- **Manual**: operator runs the curl-pipe-sh on the host with a
  token they minted by hand. The "homelab default."

---

## 6. Open questions

- **Binary signing**: cosign (sigstore) or minisign? Cosign integrates
  with the GitHub Actions release flow more naturally; minisign is
  trivial to verify offline. Lean cosign with minisign as fallback
  embedded in the install script.
- **Hosting the install script**: own domain (install.orca.sh) or
  GitHub-only? Own domain is cleaner UX but adds a Caddy route
  whose downtime breaks new installs. Mirror on raw GitHub as the
  always-up fallback.
- **No-internet hosts**: hosts that can't reach github.com (OPNsense
  in a strict outbound policy). Need an "orca install from local
  tarball" mode that an already-paired peer can deliver.
- **macOS bootstrap**: launchd plist needs full-disk-access prompts
  for some operations. Document the manual approval step, or restrict
  Mac hosts to a narrower default capability set.
- **Idempotency of the OpenRC and rc.d unit files**: confirm both
  init systems handle re-install cleanly without leaving stale
  symlinks.

---

## 7. Work breakdown

| # | Item | Size |
|---|---|---|
| B1 | Install script: platform detection matrix + binary download + sig verify | M |
| B2 | Service-unit templates (systemd, openrc, launchd, rc.d, procd) | M |
| B3 | Per-integration capability templates (sudoers fragments, polkit rules, setcap) | M |
| B4 | Pair-token mint + single-use validation in mesh | S |
| B5 | `orca host capabilities` audit command | S |
| B6 | cloud-init recipe + docs | S |
| B7 | Release pipeline: cross-compile + cosign sign + GitHub release | M |
| B8 | "Install from local tarball" mode for no-internet hosts | S |
| B9 | OpenWrt special-case (uclibc / opkg) | M |
| B10 | macOS launchd + full-disk-access docs | S |

B1+B2+B7 unblock the rest. B4 is needed before any host pairs.

---

## 8. Relationship to other planned docs

- [orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6 — provisioning
  framework; this doc is the first step of that.
- [host-lifecycle.md](host-lifecycle.md) — what happens after install:
  drivers (NVIDIA/AMD/Intel), OS updates, reboots, UPS-coordinated
  shutdowns.
- [pki-lifecycle.md](pki-lifecycle.md) — what the cert exchange
  during pairing looks like.
- [caddy-plugin-scope.md](caddy-plugin-scope.md) — hosts install.orca.sh.
- [observability.md](observability.md) — install script emits audit
  events that flow into the audit DB.
