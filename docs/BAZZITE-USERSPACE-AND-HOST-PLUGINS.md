# orca on Bazzite (user-space) + host-provisioning plugins

> Assessment + outline. Driven by `raccoon` (gaming setup/restore) and `beaver`
> (backup/restore) — two host tools that target **Bazzite** (Fedora atomic) and
> **CachyOS** and are built to become orca plugins.

## 1. Installing orca on Bazzite in user-only space

Bazzite is Fedora **atomic** (rpm-ostree): `/usr` is read-only, packages are
image-layered (reboot to apply), and `$HOME` lives at `/var/home/$USER`.
Everything a normal user installs goes to `~/.local`, user Flatpak, `systemd
--user`, Homebrew, or a distrobox/toolbox container.

### What already works today (no changes needed)

`orca install` is **already fully user-space** (`projects/system/src/install.rs`):

| Step | Target | Root? |
|---|---|---|
| binary | `~/.local/bin/orca` (on `$PATH` by default on Fedora) | no |
| state / vault | `~/.orca/…` | no |
| Claude config | `~/.claude/{CLAUDE.md,agents,skills,commands,settings.json}` | no |
| git guard hook | `~/.config/git/hooks/commit-msg` (+ `core.hooksPath`) | no |
| MCP registration | user MCP config | no |

No `/usr`, `/opt`, or root writes anywhere. **On Bazzite the binary install
works as-is.**

### Gaps to close for a first-class Bazzite install

1. **Daemon lifecycle** — install does not yet lay down a service unit. Add a
   `step_user_systemd_unit`:
   - write `~/.config/systemd/user/orca.service`, `systemctl --user enable --now orca`,
   - `loginctl enable-linger $USER` so the daemon (and scheduled **beaver**
     backups) run without an active graphical session.
2. **Plugin distribution / toolchain** — plugins are `cdylib`s (`lib<name>.so`)
   `dlopen`ed from the install dir. You cannot `cargo build` on the atomic host
   without a toolchain (distrobox/brew). So: ship **prebuilt `x86_64` `.so`** via
   GitHub Releases / OCI and have `plugin.install` fetch them. (Depends on the
   multi-capability ABI, #13.)
3. **Host command execution** — orca and its host plugins must run host tools
   (`rpm-ostree`, `flatpak`, `systemctl --user`, `restic`, `steam`). Fine when
   orca runs **host-native**. Under a sandbox it must shell via
   `flatpak-spawn --host` (see below).

### Flatpak — assessment (we might need one)

**Pros:** one-command user install (`flatpak install`), atomic updates, no
`$PATH` fiddling, Flathub/OCI distribution — matches Bazzite's Flatpak-first
ethos and is the most "native" way to hand someone a GUI on an atomic OS.

**Cons:** the sandbox fights orca's whole job. The core orchestrator needs the
host filesystem (`~/.claude`, `/var/home`), host command exec
(`rpm-ostree`/`flatpak`/`systemctl`), the docker/podman socket, `dlopen` of
plugin cdylibs, and the ability to spawn Claude Code. Granting all of that
(`--filesystem=host`, `--talk-name=org.freedesktop.Flatpak` + `flatpak-spawn
--host`, `--device=all`, `--socket=…`) erodes the sandbox to near-nothing — you
pay Flatpak's complexity without its isolation benefit.

**Recommendation:**
- **Primary install = host-native binary in `~/.local/bin`** (already
  implemented; simplest and correct for an orchestrator that drives the host).
- **Flatpak = optional, and best aimed at the _frontend_**, not the core. The
  web UI is slated to become a removable `FrontendProvider` plugin
  (`frontend-as-plugin`); shipping *that* as a Flatpak is clean because a GUI
  actually benefits from the sandbox. If we ever Flatpak the core, wrap every
  host call in `flatpak-spawn --host` and grant `--filesystem=host`.
- Revisit once #13 (prebuilt-plugin distribution) and the FrontendProvider land.

## 1a. Modularity — each system installs only the plugins it needs

**Core tenet: an orca install pulls only the capability plugins relevant to
*that* machine.** A Bazzite gaming workstation loads `raccoon` + `beaver` and
nothing else; a NAS loads `jellyfin` / `*arr` / storage; a Proxmox node loads
the deploy-target + backup plugins. No box ever carries the whole catalog —
that is the entire point of the plugin architecture.

Consequences that shape everything above:

- **Distribution is pull, per-host, on demand.** `plugin.install` fetches only
  the prebuilt `.so`s a given machine asks for — never a bundled superset. This
  is *why* the prebuilt-`.so` fetch (gap #2) matters more than a fat installer:
  the atomic host should download three plugins, not thirty.
- **The daemon loads only what's present.** The loader already scans the install
  dir and registers whatever `.so`s are there; keeping that dir minimal per host
  is the whole mechanism — no global enable/disable list to maintain.
- **Host detection can seed the set.** raccoon/beaver already detect the distro;
  install-time host detection (Bazzite vs CachyOS vs a server) can *suggest* the
  relevant plugin set, but the selection stays explicit and per-machine.
- **Profiles, not monoliths.** A machine's plugin set is its profile; two
  gaming boxes share `{raccoon, beaver}`, a media server shares a different set.
  Same orca binary everywhere, different (small) plugin dirs.

## 2. `raccoon` and `beaver` as orca plugins

Both are **host-provisioning / backup tools** (bash), per-machine — a different
shape from the network-service ServiceBackends (jellyfin, etc.). They already
expose a **stable subcommand contract** on purpose, so the plugin is a thin
wrapper.

### Capability mapping — reuse `ServiceBackend`, unit = the workstation

The `service.*` verbs fit host provisioning cleanly ("any thing can be a
service"): the managed unit is *the machine itself*.

| Plugin | `deploy` / `configure` | `backup` / `restore` | `status` |
|---|---|---|---|
| **raccoon** | `bootstrap.sh`, `scripts/install-*.sh`, `setup-*.sh` (launchers, configs, tweaks) | — | which launchers/tweaks are present |
| **beaver** | — | `backup.sh` / `restore.sh` (+ `lib/state.sh capture\|restore`) | last backup, configured destinations |

- The cdylib maps each `op` → a script subcommand over the existing JSON-proxy
  `invoke` boundary. No new ABI needed beyond what backends already use.
- **beaver is cross-domain**: it also implements the core **BackupMethod**
  capability (restic → NAS / external / cloud). One cdylib, two registrations
  (`ServiceBackend` + `BackupMethod`) — the exact "one plugin, many
  capabilities" property (#13).

### Runtime & scheduling

- **beaver**: orca manages a `systemd --user` timer (needs `enable-linger`) for
  scheduled backups — the daemon-lifecycle gap above is a prerequisite.
- **raccoon**: on-demand (`deploy`/`configure`). Flag privileged steps
  (`setup-controller-wake` writes udev → needs sudo; rpm-ostree layering →
  reboot) distinctly in `status`, since they can't complete silently in
  user-space.

### Distribution

- The plugin **vendors its scripts** (or clones/updates the repo into
  `~/.orca/plugins/<name>/`) and invokes them; the descriptor is the single
  source of truth for deploy + docs + MCP surface + icon (icons already added).
- Prebuilt `.so` per the Bazzite distribution note above.

### Bazzite specifics the plugins must honor

- `rpm-ostree` layered packages need a **reboot**; Flatpak app-list
  capture/restore (beaver) uses **user** Flatpak; paths under `/var/home`.
- CachyOS (Arch) parity: pacman/AUR + Flatpak — capture/restore is per-distro,
  data backup shared (already the beaver design).

## Sequencing

1. Land #13 (multi-capability ABI) + prebuilt-plugin fetch — unblocks shipping
   `.so`s to an atomic host.
2. Add `step_user_systemd_unit` + linger to `orca install` (daemon + timers).
3. Wrap `beaver` first (ServiceBackend backup/restore **+** BackupMethod) — it
   exercises scheduling and cross-domain registration.
4. Wrap `raccoon` (ServiceBackend deploy/configure) — exercises privileged/
   reboot-gated steps and status reporting.
5. Decide Flatpak once the FrontendProvider plugin exists.
