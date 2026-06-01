# Host lifecycle — drivers, updates, reboots, UPS

> **HARD RULE — user-triggered changes only.** Orca detects drift,
> notifies, and waits. The user runs `orca apply <change-id>` (or
> accepts a UI prompt). No reconciler ever auto-applies. No
> scheduled apt-upgrade, no auto-reboot, no auto-driver-rebuild.
> See ROADMAP §1.11.

Beyond install ([install-bootstrap.md](install-bootstrap.md)), orca
owns the full life of a host: drivers, OS updates, reboots,
shutdowns, and coordinated shutdown orchestration driven by UPS
events. Single pane of glass for "this thing the operator would
otherwise SSH in for."

Out-of-doc scope: provisioning new users / package installs
([orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6) and
service-level deploy ([caddy-plugin-scope.md](caddy-plugin-scope.md),
backup-restore, …) are covered elsewhere.

Sizing: **S** ≤ 1 day · **M** 1–3 days · **L** 3–7 days · **XL** > 1 week.

---

## 1. Drivers — GPU and beyond

GPU drivers in particular are painful: vendor-supplied tarballs,
kernel-module rebuilds on every kernel upgrade, mismatched CUDA /
ROCm / Compute Runtime versions, license prompts. Today this
is hand-maintained per host. Becomes an orca verb.

### 1.1 Scope

| Vendor | Stack | Source |
|---|---|---|
| NVIDIA | `nvidia-driver`, `nvidia-modprobe`, optional CUDA toolkit, optional Container Toolkit (`nvidia-container-toolkit`) | Distro packages where viable (Debian/Ubuntu non-free, Fedora RPMFusion). Vendor `.run` installer as fallback. |
| AMD | `amdgpu` kernel driver (in-tree, normally fine), `ROCm` userspace, `mesa` for Vulkan/OpenGL | Distro packages + ROCm apt/dnf repo. |
| Intel | `i915` kernel driver (in-tree), `intel-media-driver` (iHD) for VAAPI, `intel-compute-runtime` for OpenCL/Level Zero, `intel-gpu-tools` for metrics | Distro packages, generally clean. |
| Network | `ixgbe`, `i40e`, `mlx5` tuning; `r8125` out-of-tree for some Realtek | Distro + DKMS for out-of-tree. |
| Storage controllers | LSI/Broadcom HBA firmware checks, `mpt3sas` quirks | Out-of-band — flag for the operator, don't auto-flash firmware. |
| ZFS (if used) | OpenZFS kmod via DKMS | Distro repo. |

### 1.2 Verbs

```sh
orca host driver list                    # what's installed, what's available
orca host driver install nvidia          # detects card, picks best stable
orca host driver install nvidia --version 550
orca host driver install --with cuda --with container-toolkit
orca host driver update nvidia           # respects pinned version unless --upgrade
orca host driver remove nvidia
orca host driver status                  # kernel module loaded? mismatch?
```

`status` is the load-bearing one: detects the common failure mode
where the kernel got upgraded but the DKMS rebuild silently failed,
leaving the host without the GPU module. Surfaces as a metric in
[observability.md](observability.md).

### 1.3 Declarative form

```toml
# config/<host>/drivers.toml
[[driver]]
vendor = "nvidia"
version = "550"           # or "latest-stable"
extras = ["cuda", "container-toolkit"]
pin_kernel = false        # if true, refuse kernel update without matching driver

[[driver]]
vendor = "intel"
extras = ["compute-runtime", "media-driver"]
```

GitOps loop reconciles. Kernel upgrade that would orphan a pinned
driver is blocked until both can move together.

### 1.4 Constraints

- **DKMS-aware**: if the driver ships as DKMS, orca runs `dkms
  install` post-kernel-upgrade and verifies the new module loads.
- **Container Toolkit integration**: NVIDIA Container Toolkit needs
  matching docker daemon config; that's a managed config edit, not
  a manual `/etc/docker/daemon.json` rewrite.
- **Never auto-flashes firmware**: HBA firmware updates and similar
  are surfaced as advisories. Operator triggers explicitly.

---

## 1.5 Proxmox guest lifecycle — pointer

LXC and VM lifecycle on Proxmox hosts is its own beast (config
drift, bind sources, tmpfs scratch, restore-aware start). It is
**not** covered here. See [lxc-vm-reconciler.md](lxc-vm-reconciler.md)
for the full design — declarative `pct`/`qm` configs from the
repo, diff-and-apply against `/etc/pve/{lxc,qemu-server}/*.conf`,
bind-source + inner-service readiness gates, PBS-snapshot-aware
restore wrapping.

### Tmpfs scratch volumes (host-level)

Today on frigg, `/var/lib/orca-transcode` is an 8G fstab-backed
tmpfs shared by CT 113 (jellyfin) and CT 114 (njord) via per-CT
`mp2` binds. **No per-CT quota** — either guest can fill it and
starve the other.

Orca should own:

- The host-side `*.mount` systemd unit + size policy (replaces the
  fstab line).
- Per-consumer subdir bind, with a quota floor (subdir-per-consumer
  in v1, project-quota where the FS supports it).
- The CT `mp*` reconcile into the bind, via
  [lxc-vm-reconciler.md](lxc-vm-reconciler.md) §2.5.

Tmpfs scratch is a "host lifecycle" concern because the host owns
the RAM; CT-side binds are downstream of the host policy.

---

## 2. OS / package updates

Update verbs are **shipped**: `projects/system/src/update.rs` plus
the tool defs in `projects/tools-def` under `orca_lifecycle`
(`update-check`, `update-apply`, `doctor`). Per-distro package-manager
drivers + declarative update policy below are the extension surface,
not greenfield.

`apt`, `apk`, `dnf`, `pacman`, `pkg`, `opkg` — all behind one verb,
per-distro logic in rust:

```sh
orca host update list           # available updates, security flagged
orca host update apply          # apply all, default policy
orca host update apply --security-only
orca host update apply --reboot if-needed
orca host update plan           # dry-run, shows what would change
```

### 2.1 Declarative policy

```toml
# config/<host>/updates.toml
[update]
mode = "scheduled"              # scheduled | manual | auto-security
schedule = "0 4 * * 0"          # weekly Sunday 04:00
security_apply = "immediate"    # security updates bypass schedule
reboot_if_needed = "scheduled"  # never | scheduled | immediate
reboot_window = "0 5 * * 0"     # only reboot during this window

[update.hold]
packages = ["nvidia-driver-550"]  # pinned, see drivers section
kernels = []                       # pin specific kernel versions if needed
```

### 2.2 Coordination across services

A `host update apply` that triggers a reboot must:

1. Drain managed services to other peers where possible (Caddy
   route fanout — [caddy-plugin-scope.md](caddy-plugin-scope.md) —
   already supports this via lb_policy).
2. Run pre-reboot hooks (gracefully stop docker containers in
   order, flush write buffers).
3. Trigger the reboot via §3.

### 2.3 Update advisory surface

Per-host update list visible in the orca UI under the host detail
panel, with:
- Total updates pending, security-flagged count
- Last successful update timestamp
- Last failed update + reason
- Pending reboot indicator

Aggregated fleet view: "12 hosts have security updates pending."

---

## 3. Reboot + shutdown

Same generic verb across distros:

```sh
orca host reboot baldur
orca host reboot baldur --after "2026-05-25T03:00:00Z"
orca host shutdown baldur
orca host shutdown baldur --graceful-timeout 5m
```

### 3.1 Pre-shutdown hooks

The hook chain is declarative per host:

```toml
# config/<host>/shutdown.toml
[[hook]]
order = 10
name  = "drain-caddy-routes"
run   = "orca caddy route drain --host baldur --timeout 30s"

[[hook]]
order = 20
name  = "stop-docker"
run   = "orca docker stop --all --timeout 60s"

[[hook]]
order = 30
name  = "unmount-nfs"
run   = "orca nfs unmount --all --lazy"
```

Hooks run in `order` ascending. A hook failure aborts the
shutdown by default (`on_failure = "abort"` overridable to
`"continue"`).

### 3.2 Distributed reboot

Cluster operations:

```sh
orca host reboot --selector "role=docker" --strategy rolling
orca host reboot --selector "role=docker" --strategy rolling --max-parallel 1
```

`rolling` waits for each host to come back healthy (orca daemon
reachable + post-boot hooks succeeded) before moving to the next.

---

## 4. UPS management + coordinated shutdown

The killer feature. The UPS sees mains power drop → broadcast
"shutdown in N minutes" → orca orchestrates an ordered shutdown
across the entire pod.

### 4.1 UPS visibility

Sources:
- **Unraid-attached UPS**: already exposed via Unraid GraphQL
  (battery %, load %, input voltage, status). Today: visible only
  on the host that owns the UPS USB connection.
- **APC NUT (Network UPS Tools)**: where the UPS is shared via NUT
  daemon on one host. Orca runs an NUT client integration.
- **Tripp Lite / CyberPower SNMP**: rack-mount UPSes with SNMP
  cards. Orca polls SNMP.

Each UPS appears in the system tree under the host it's primarily
attached to, but its events fan out to every host on the same
power circuit.

### 4.2 Event model

UPS events orca cares about:

| Event | Trigger | Meaning |
|---|---|---|
| `mains_lost` | Input voltage drops | Power outage. Battery countdown starts. |
| `battery_low` | Battery % below threshold (default 50%) | Begin orderly shutdown soon. |
| `battery_critical` | Battery % below threshold (default 20%) | Begin orderly shutdown **now**. |
| `mains_restored` | Input voltage returns | Cancel pending shutdown. Optionally power-cycle gracefully. |
| `load_high` | Output load above threshold | Capacity warning. |
| `replace_battery` | Self-test fails | Operator advisory. |

Events flow into orca's audit DB and through the alert surface.

### 4.3 Shutdown orchestration

A `mains_lost` event triggers the **shutdown plan**:

```toml
# config/cluster/shutdown-plan.toml
[plan]
trigger = "battery_low"           # which event starts the cascade
trigger_override_critical = true  # battery_critical short-circuits

# Hosts grouped by shutdown wave. Earlier waves shut down first;
# the host with the UPS shuts down last.

[[wave]]
order = 1
hosts = ["freyr", "mimir", "njord", "jellyfin"]  # media / non-essential
parallel = true                                  # all at once

[[wave]]
order = 2
hosts = ["baldur"]                               # primary services / caddy
parallel = false

[[wave]]
order = 3
hosts = ["maple", "willow"]                      # NAS / storage last
parallel = false
wait_between = "60s"

[[wave]]
order = 4
hosts = ["thor", "frigg", "loki"]                # Proxmox hosts last
parallel = false
wait_between = "60s"

# The host hosting the UPS-attached daemon and orca itself stays
# up longest; its own shutdown is triggered by `battery_critical`.
```

Execution:

1. UPS event arrives at the host with the UPS connection.
2. That host publishes the event to the mesh.
3. Each peer evaluates: "am I in a wave that's triggered by this
   event?"
4. Wave-1 hosts start their `shutdown` hook chain (per §3.1).
5. Mesh coordinator (the host running the UPS daemon) waits for
   each wave to confirm shutdown complete before triggering the
   next.
6. If `battery_critical` arrives before the cascade completes,
   coordinator skips remaining waves' graceful timeouts and issues
   immediate shutdowns.
7. Coordinator shuts itself down last after the last wave confirms.

### 4.4 Power restoration

On `mains_restored`:

- If shutdown is in progress and not yet past the point of no
  return (configurable per wave), abort the cascade.
- If shutdown completed, optionally trigger WoL on hosts that
  support it, in **reverse** wave order (storage first, then
  services, then media).
- Confirm post-boot health per host before triggering the next
  in the boot cascade.

### 4.5 Test mode

Critical: the cascade must be testable without actually killing
power.

```sh
orca ups simulate --event battery_low --plan cluster/shutdown-plan.toml --dry-run
```

Dry-run prints the cascade order with timings. Live mode (no
`--dry-run`) actually shuts down per the plan, useful for
validating the operational sequence before the first real outage.

---

## 5. Surfacing in the UI

Each host's detail panel gains a "Lifecycle" tab:

- **Driver status**: per-driver row with version, kernel-module
  loaded indicator, last update.
- **Updates pending**: list with security flags, last update
  timestamp, scheduled apply window.
- **Power state**: UPS source if any, battery %, last UPS event.
- **Shutdown wave**: which wave this host is in, what its hooks
  are.
- **Last reboot reason**: scheduled / manual / UPS-triggered /
  crash (extracted from journal).

Aggregated fleet view tab: "shutdown plan readiness" — every host
that should be in a wave but isn't, every host with stale driver
info, every host overdue for updates.

---

## 6. Work breakdown

| # | Item | Size |
|---|---|---|
| HL1 | `orca host update` verbs + per-distro impl + declarative policy | L |
| HL2 | `orca host reboot/shutdown` with hook chain + distributed selector | M |
| HL3 | `orca host driver` verbs + declarative spec + DKMS-aware kernel coordination | L |
| HL4 | NVIDIA driver impl (incl. Container Toolkit hook into docker config) | M |
| HL5 | AMD driver impl (ROCm) | M |
| HL6 | Intel driver impl (compute-runtime, media-driver) | M |
| HL7 | UPS event sources: NUT client, SNMP poll, Unraid GraphQL UPS fields | M |
| HL8 | Shutdown-plan TOML schema + wave orchestrator | L |
| HL9 | UPS event → mesh propagation + per-host wave evaluator | M |
| HL10 | Power-restoration cascade (WoL + post-boot health gate) | M |
| HL11 | `orca ups simulate` dry-run + live test mode | S |
| HL12 | Lifecycle UI tab in host detail panel | M |
| HL13 | Fleet "shutdown-plan readiness" aggregated view | S |

HL1+HL2 are independent and useful immediately. HL7+HL8+HL9 are
the UPS story and must land together to be useful. HL3+HL4-HL6 are
parallelizable per vendor.

---

## 7. Open questions

- **Who is the "coordinator" for UPS-triggered shutdown?** Today the
  obvious choice is the host with the UPS USB attached. But that
  host shuts down last, so it must be the most reliable for the
  longest period. Define explicitly per pod.
- **Wave dependencies vs simple ordering**: should waves have
  declared dependencies (NAS must be up for media to be up) or
  is ordinal ordering enough? Lean ordinal for v1, dependencies
  for v2.
- **Cross-pod UPS events**: a UPS in one site shouldn't trigger
  shutdowns in another. Plan is per-pod, evaluated locally — but
  worth being explicit.
- **What about IPMI / iDRAC / BMC?** Many of the same operations
  (power cycle, fan/temp monitoring) are also exposed via BMC.
  Orca should optionally talk to BMCs as another integration —
  out of scope here, track when needed.
- **Auto-update vs manual approval**: today the doc lets the
  config repo set `mode = "auto-security"`. Is that wise? Operator
  preference. Default conservative ("scheduled"); auto-security
  is opt-in per host.

---

## 8. Relationship to other planned docs

- [install-bootstrap.md](install-bootstrap.md) — what `--with`
  flags lay down at install; this doc covers what happens after.
- [orca-as-logic-layer.md](orca-as-logic-layer.md) §3.6 — package
  install (docker, tailscale, etc.) is the sibling capability;
  drivers fit the same pattern.
- [observability.md](observability.md) — UPS metrics, driver
  status, update-pending counts all surface there.
- [backup-restore.md](backup-restore.md) — restore drills should
  not conflict with scheduled update windows.
- Consuming-repo example: meerkat's `docs/planned/ups-shutdown-plan.md`
  is the scottkey-homelab instance of §4's shutdown plan.
