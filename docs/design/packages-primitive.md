# Design: `packages` primitive — cross-platform developer-tool IaC

Status: **proposal** · Author: drafted with Claude · Depends on: reconcile
infrastructure (`mount_converge`, `containers/reconciler`), `plugin_toolkit`.

## Motivation

Developer-tool setups (CLI utilities, GUI apps, language runtimes) are currently
maintained by hand per machine. We want **one declarative source of truth** that
provisions a workstation's toolchain consistently across macOS, Bazzite,
CachyOS, and Fedora — expressed as an orca primitive that plugins declare
against, in the same model as every other orca resource.

orca is the unified orchestrator; the workstation plugins declare *what* a class
of machine wants; the primitive knows *how* to realize it on each OS.

## Model: native-primary backend + universal mise

Each host has **one primary native backend** for system/CLI/GUI packages, and
**mise is elected everywhere** for language runtimes.

| Host                    | Primary system backend | Runtimes |
|-------------------------|------------------------|----------|
| macOS                   | Homebrew               | mise     |
| Bazzite (Fedora atomic) | Homebrew¹              | mise     |
| CachyOS (Arch)          | pacman                 | mise     |
| Fedora (workstation)    | dnf                    | mise     |

¹ Bazzite is an atomic base; layering via `rpm-ostree` for dev tools is
discouraged, so Homebrew is the blessed path. `rpm-ostree` is **not** a
dev-tool backend here.

**Logical → native resolution.** A logical tool name (`ripgrep`, `fd`, `jq`) is
assumed identical across backends and used verbatim. Only the deltas
(`bat`→`batcat`, cask↔flatpak for GUI apps) are declared as per-backend
overrides. Runtimes (`node`, `python`, `ruby`, `dotnet`) always route to mise;
Rust stays on rustup and is out of scope for the package primitive.

## Fit with orca's primitive model

Grounded in the current architecture (`CRATE_RESPONSIBILITIES.md`):

- Primitives are Rust domains owned by a platform crate, exposing
  `#[orca_tool(domain, verb)]` functions that auto-register into MCP via
  `dispatch` — no server wiring. Tool `package.reconcile` → MCP
  `package_reconcile`.
- Persistent state is a table in the `db` crate (`apply_schema()` in
  `projects/db/src/lib.rs`), accessed only through a `db::<table>::*` CRUD module.
- Desired-state primitives add a **core reconcile loop**: pure `plan(desired ⋈
  actual → actions)` + async tick, per `projects/system/src/mount_converge.rs`
  and `projects/containers/src/reconciler.rs`.
- Plugins do **not** declare desired rows in `orca-plugin.toml`. They register
  backends/tools over the plugin seam and **call create-tools** to write desired
  rows, then trigger reconcile.

## Components

### 1. New crate `projects/packages/`
- Register in workspace `Cargo.toml` `[workspace.members]` and add a row to
  `CRATE_RESPONSIBILITIES.md` (required by the "justify new crate" rule).
- DTOs (`clap::Args` + `serde` + `schemars::JsonSchema`) + `#[orca_tool]`
  functions in `projects/packages/src/lib.rs`:
  `package.{list,detail,create,update,delete,reconcile,reconcile_dry}`.

### 2. Desired-state table `projects/db/src/packages.rs`
DDL added to `apply_schema()` (+ a migration for existing DBs). Row shape:

```
package(
  name            TEXT,     -- logical tool name
  kind            TEXT,     -- 'system' | 'runtime'
  ensure          TEXT,     -- 'present' | 'absent'
  host_selector   TEXT,     -- which hosts/classes this applies to (glob/tag)
  backend_override TEXT,    -- force a backend, else host-elected
  pkg_overrides   TEXT,     -- json: { brew: "...", pacman: "...", flatpak: "..." }
  version         TEXT,     -- optional pin (mise runtimes, brew @version)
  source_plugin   TEXT,     -- which plugin declared it
  created_at, updated_at
)
```

### 3. Backend adapters — **core, in `projects/packages/`**
A `PackageBackend` trait mirroring `storage::StorageBackend`:

```rust
trait PackageBackend {
    fn id(&self) -> &'static str;                 // "brew" | "pacman" | "dnf" | "flatpak" | "mise"
    async fn available(&self) -> bool;            // via utils::path::which
    async fn query_installed(&self) -> Result<Vec<InstalledPkg>>;
    async fn install(&self, pkgs: &[ResolvedPkg]) -> Result<()>;
    async fn remove(&self, pkgs: &[ResolvedPkg]) -> Result<()>;
    async fn upgrade(&self, pkgs: &[ResolvedPkg]) -> Result<()>;
}
```

Ship **brew, pacman, dnf, flatpak, mise** as core trait impls (thin shell-outs;
universal; needed everywhere — not third-party service integrations). The
`plugin_toolkit::packages` seam is left open for exotic backends later.

**Host election** reuses the OS-detection + `which()` idioms already in
`projects/system/src/package.rs`: elect the primary system backend by OS; always
add `mise` for `kind=runtime`.

### 4. Reconcile loop `projects/packages/src/package_converge.rs`
Shape copied from `mount_converge.rs`: pure `plan(desired_for_host ⋈ installed)`
→ `[Install | Remove | Upgrade]` grouped by elected backend, then an async tick
that shells out — user-level for brew/mise, polkit/sudo for pacman/dnf. Wired
into the daemon periodic scheduler. `package_reconcile_dry` returns the diff
without applying (parity check); `package_reconcile` applies.

### 5. Workstation plugins declare the *sets*
On apply, each plugin bulk-calls `package_create` then `package_reconcile`:

- **`macos-workstation-setup`** — brew-primary system set + mise runtime set,
  **seeded from the existing `dotfiles/Brewfile` + `~/.config/mise/config.toml`.**
- **`linux-workstation-setup`** — one plugin, branches on detected distro:
  pacman (CachyOS) / dnf (Fedora) / brew (Bazzite) system sets, sharing the same
  mise runtime set.

## Rollout

1. `packages` crate: table + CRUD + `reconcile_dry` (diff only).
2. Backend adapters: brew + mise first (validate on macOS), then pacman/dnf/flatpak.
3. Reconcile tick.
4. `macos-workstation-setup`, seeded from our Brewfile/mise config; iterate until
   `reconcile_dry` on this mac shows an empty diff (proves parity).
5. `linux-workstation-setup` per distro; validate on CachyOS/Bazzite/Fedora.
6. **`scottkey → skey` rename as the first real IaC re-provision**: rename via
   Apple Advanced Options (UID 501 preserved → ownership intact), then
   `package_reconcile` + `chezmoi apply` rebuild the toolchain under the new
   home. Empty post-diff = IaC proven.

## Key reference files to model against
- CRUD primitive: `projects/auth/src/secrets.rs` + `projects/db/src/secrets.rs`
- Reconcile loop: `projects/system/src/mount_converge.rs`,
  `projects/containers/src/reconciler.rs`
- Multi-backend trait seam: `projects/storage/src/{lib,mount_table}.rs`,
  `projects/plugin-toolkit/src/backend_def.rs`
- MCP auto-registration: `projects/dispatch/src/registry.rs`
- Host detection / package-format idioms: `projects/system/src/package.rs`

## POC findings (mint, 2026-07-31)

A pure-Python prototype of the reconcile plan (parse Brewfile[.host] + mise
config → query brew/mise → bidirectional diff) validated the whole logic on this
Mac before any Rust. It drove drift **38 → 0 (empty diff, parity proven)**; mise
runtimes were already at parity. Two lessons surfaced during the run and are now
**implemented in the prototype** as the reference behavior for the Rust port:

1. **`host_selector` must key off a stable host *class*, not the raw hostname.**
   `scutil --get LocalHostName` returned `mint-3` (DHCP-collision suffix), so the
   `Brewfile.mint` layer silently didn't apply and every mint-only tool showed as
   false drift. *Prototype fix:* normalize by stripping a trailing `-N`; the Rust
   loop should resolve a declared host class, never compare the raw hostname.
2. **Installer-type casks need an `install_method` in `pkg_overrides`.** A plain
   `brew install --cask` can't complete installer casks (e.g.
   `private-internet-access` — brew drops an `*Installer.app`, unquarantine fails,
   a network extension needs a manual GUI run). *Prototype fix:* the package
   declares `# install_method: installer; app: "<Name>"` (parsed into the same
   `pkg_overrides` shape the Rust column holds). Such packages are held out of the
   ordinary cask diff and reconciled by **app presence** (`/Applications/<Name>.app`)
   — "present" == installer delivered. If the app is missing, it is reported as a
   distinct *manual* action ("run the installer once"), never as diff that blocks
   parity. On mint the PIA client is present, so the diff is genuinely empty.

The prototype lives in the session scratchpad; its logic is the reference for
`package_converge.rs::plan`.

## Out of scope
- Rust toolchain (stays on rustup).
- `rpm-ostree` base-layer packages (system provisioning, not dev tools).
- Non-workstation service deploys (existing service/container primitives).
