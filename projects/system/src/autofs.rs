//! autofs renderer + privileged applier — turns the declarative `managed_mounts`
//! store into an autofs **direct map** so autofs owns the mount mechanics we
//! would otherwise hand-build: on-demand mounting, idle unmount, and — the
//! reason this exists — **replicated-server failover** across a mount's ordered
//! sources (primary → secondary). A map entry with multiple locations lets
//! autofs probe and pick a live server itself.
//!
//! ## Two-process split (privilege boundary)
//!
//! The orca daemon runs as the unprivileged `orca` user, but autofs config lives
//! in root-owned `/etc` and reloading autofs needs root. So the work is split:
//!
//! * **Daemon side** (`orca` user, has the encrypted DB): [`plan`] reads the
//!   store, renders the map, detects the host's master-file location + init
//!   system, take-over-merges the master file, and diffs against what's on disk
//!   to produce a [`PrivilegedOp`] describing exactly which files to write.
//! * **Root side** (`orca admin storage-apply`, invoked via `sudo -n`):
//!   [`execute_privileged`] validates every path against a fixed allowlist,
//!   writes atomically, and restarts autofs. It makes no decisions and never
//!   touches the DB — it just executes a validated plan.
//!
//! [`run_privileged`] is the daemon-side bridge that shells out to the helper.
//! The one failure mode autofs does *not* self-heal — an actively-held stale
//! `hard` mount — is handled by [`recover`] (the `storage.mount.update{action=recover}` tool) and the
//! per-host convergence loop in [`crate::mount_converge`]; its `umount -lf` also
//! needs root, so it routes through the same seam ([`PrivilegedOp::Release`]).
//!
//! The pure builders ([`render_map`], [`master_line`], [`merge_master`],
//! [`map_line`], [`autofs_options`]) unit-test without touching the host.

use crate::managed_mounts::{ManagedMount, ordered_sources};
use crate::source_election::{Election, RemountAggression, Transition, elect, transition};
use plugin_toolkit::storage::{
    MountEntry, MountHealth, RecoveryAction, mount_table, mount_table_of, probe_health,
    probe_source,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// Network filesystem types orca elects/reads sources for, learned at runtime by
/// unioning every registered storage backend's [`net_fstypes`]. Core holds no
/// fstype literal: nfs contributes `nfs4`/`nfs`, smb contributes `cifs`/`smbfs`,
/// and a disk/object backend contributes nothing. Deduped, sorted for stability.
///
/// [`net_fstypes`]: plugin_toolkit::storage::StorageBackend::net_fstypes
fn net_fstypes() -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for b in plugin_toolkit::storage::backends() {
        set.extend(b.net_fstypes());
    }
    set.into_iter().collect()
}

/// The direct map file. One line per managed mount, keyed by absolute target.
pub const MAP_FILE: &str = "/etc/auto.orca";

/// Root-written, 0600 ledger of what root itself mounted, one
/// `id<TAB>target<TAB>source` line per placement; it is the only authority for
/// [`PrivilegedOp::Release`]. Deliberately not in [`is_allowed_write`]: only the
/// helper itself may change it, never a payload.
pub const LEDGER_FILE: &str = "/etc/auto.orca.ledger";
/// Idle-unmount timeout (seconds) autofs applies to our mounts. `0` means
/// **persistent** — autofs never idle-expires these mounts.
///
/// Every orca-managed autofs mount here backs a long-running service or
/// container bind-mount, so idle expiry buys nothing and actively harms: on
/// container hosts, a Docker bind-mount of a subpath of an orca-managed NFS
/// mount races the idle expiry — the mount expires, a container (re)start
/// bind-mounts the now-unmounted path, Docker materializes an empty *local*
/// shadow dir, and that shadow dir blocks autofs from ever remounting.
/// Containers then go blind.
///
/// Failover does NOT depend on idle expiry: the convergence loop
/// ([`crate::mount_converge`]) actively stale-probes, force-unmounts
/// (`umount -lf`), and remounts onto the next live elected source, so making
/// mounts persistent leaves auto-failover fully intact while removing the race.
const TIMEOUT_SECS: u32 = 0;

const HEADER: &str =
    "# managed by orca — do not edit; source of truth is the managed_mounts store\n";
/// Delimiters for the orca-managed block inside the host's autofs master file.
/// Everything between them is ours to rewrite; everything outside is foreign
/// config we preserve verbatim (take-over-merge).
const BLOCK_BEGIN: &str = "# >>> orca managed (autofs) >>>";
const BLOCK_END: &str = "# <<< orca managed (autofs) <<<";

/// Init system, detected on the daemon side and carried to the root helper so
/// it restarts autofs the right way. Serialized on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Init {
    Systemd,
    OpenRc,
}

/// A single file the root helper must write. Paths are validated against a fixed
/// allowlist ([`is_allowed_write`]) before any write happens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWrite {
    pub path: String,
    pub contents: String,
    /// Explicit unix mode to enforce after writing (e.g. `0o600` for a
    /// secret-file). `None` leaves the mode at the process umask default — the
    /// behavior for the world-readable autofs map + master files. Serialized as a
    /// plain integer so the field crosses the JSON seam without a mode newtype.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// A direct-map entry's placement, recorded in the root ledger by `Apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapPlacement {
    pub id: String,
    pub target: String,
    pub source: String,
}

/// The mount fields a map line is rendered from, in wire form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapMount {
    pub backend: String,
    pub source: String,
    #[serde(default)]
    pub failover_sources: Option<String>,
    pub target: String,
    pub fstype: String,
    #[serde(default)]
    pub options: Option<String>,
}

/// Structured input of a rendered direct map: the enabled network-share mounts,
/// plus the per-target elected source when rendered with [`render_map_elected`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedMaps {
    pub mounts: Vec<MapMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elected: Option<std::collections::HashMap<String, String>>,
}

impl ManagedMaps {
    fn from_mounts(
        mounts: &[ManagedMount],
        elected: Option<&std::collections::HashMap<String, String>>,
    ) -> Self {
        Self {
            mounts: mounts
                .iter()
                .filter(|m| m.enabled && m.kind == "network_share")
                .map(|m| MapMount {
                    backend: m.backend.clone(),
                    source: m.source.clone(),
                    failover_sources: m.failover_sources.clone(),
                    target: m.target.clone(),
                    fstype: m.fstype.to_string(),
                    options: m.options.clone(),
                })
                .collect(),
            elected: elected.cloned(),
        }
    }

    fn managed_mounts(&self) -> Vec<ManagedMount> {
        self.mounts
            .iter()
            .map(|m| ManagedMount {
                name: String::new(),
                backend: m.backend.clone(),
                kind: "network_share".into(),
                source: m.source.clone(),
                failover_sources: m.failover_sources.clone(),
                target: m.target.clone(),
                fstype: m.fstype.as_str().into(),
                options: m.options.clone(),
                credential: None,
                remount_policy: None,
                placement_id: None,
                routes: Default::default(),
                enabled: true,
            })
            .collect()
    }

    fn render(&self) -> String {
        let mounts = self.managed_mounts();
        match &self.elected {
            Some(e) => render_map_elected(&mounts, e),
            None => render_map(&mounts),
        }
    }
}

/// The privileged operation handed to `orca admin storage-apply` over stdin.
/// A closed, validated vocabulary — the helper does exactly these and nothing
/// else, so the `sudo` grant is a narrow, auditable surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PrivilegedOp {
    /// Write the given config files, then restart autofs via `init`. The daemon
    /// only emits this when at least one file actually differs from disk.
    ///
    /// `keep_secret_files` is accepted for wire compatibility and ignored; the
    /// native `Mount` and `SecretFiles` ops own secret-file teardown.
    Apply {
        writes: Vec<FileWrite>,
        #[serde(default)]
        keep_secret_files: Vec<String>,
        init: Init,
        /// Placements whose map entries this apply writes; root records each in the
        /// ledger so autofs-managed mounts stay releasable by id.
        #[serde(default)]
        placements: Vec<MapPlacement>,
        /// The structured mounts the map text was rendered from. When present the
        /// helper validates each field and re-renders the map itself, refusing any
        /// map write that is not byte-identical to its own rendering.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        managed: Option<ManagedMaps>,
    },
    /// Release mounts root itself recorded, addressed only by placement id. Each id
    /// must be in the root ledger and its recorded target must still report the
    /// recorded source; anything else is refused.
    Release { ids: Vec<String> },
    /// Record a mount root never mounted itself, so it becomes releasable by id. The
    /// target must carry a live network mount whose source equals `source`, and
    /// neither the id nor the target may already be in the ledger.
    Adopt {
        id: String,
        target: String,
        source: String,
    },
    /// Realize mounts natively via `mount(8)` — the autofs-free apply path. Each
    /// [`MountReq`] is already rendered (source elected, options rendered by the
    /// owning backend); the root helper just runs the mount. This is the
    /// convergence loop's "ensure present" primitive.
    ///
    /// `keep_secret_files` is the authoritative set of secret-file paths that
    /// should exist after this apply (every currently-declared inline-secret mount's
    /// secret-file). The root helper reaps any secret-file in `SECRET_FILE_DIR` NOT
    /// in this set — the generic teardown for a deleted mount or one whose secret
    /// changed. Core reaps by path validity, never by the file's grammar.
    Mount {
        mounts: Vec<crate::mount_exec::MountReq>,
        #[serde(default)]
        keep_secret_files: Vec<String>,
    },
    /// Restart autofs unconditionally so it (re)parses the master + `/etc/auto.orca`
    /// direct map. The liveness-driven escalation: when a declared target is
    /// `Missing` yet the map on disk is already correct, autofs simply never loaded
    /// it (its map was written after autofs started, or the daemon was restarted
    /// without the map). A file-diff apply short-circuits in that case, so this op
    /// forces the reload the daemon otherwise never issues.
    Reload { init: Init },
    /// Repair a share whose SERVER-SIDE permissions drifted (the write-denied
    /// class): set the mode of `path` (and, when `recursive`, everything under it)
    /// to `mode`, so the mount identity regains write. Runs on the host that
    /// *serves* the share. Tightly constrained on the root side (see
    /// [`is_allowed_share_path`] + the mode guard in [`execute_privileged`]): `path`
    /// must canonicalize to a real subtree strictly under `/mnt/user/<share>` (no
    /// traversal, no symlink escape), and `mode` must keep the owner fully able and
    /// carry no setuid/setgid/sticky bits — an additive perms overlay that can
    /// never touch a system path or escalate privilege.
    /// Write each managed secret-file (path-validated, atomic, root-owned 0600)
    /// and reap any under `SECRET_FILE_DIR` not in `keep_secret_files`. Lets a
    /// rotated secret reach a host whose mount is already up.
    SecretFiles {
        files: Vec<crate::mount_exec::SecretFile>,
        #[serde(default)]
        keep_secret_files: Vec<String>,
    },
    SetShareMode {
        path: String,
        mode: u32,
        #[serde(default)]
        recursive: bool,
    },
    /// Repair OWNERSHIP on a share subtree. Separate from [`Self::SetShareMode`]
    /// because ownership drifts independently of mode and, alone, is enough to
    /// break writes (#620).
    SetShareOwner {
        path: String,
        uid: u32,
        gid: u32,
        #[serde(default)]
        recursive: bool,
    },
}

/// Result the helper prints back to the daemon as JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrivilegedResult {
    /// Files actually written (for `Apply`).
    pub changed: Vec<String>,
    /// Whether autofs was restarted.
    pub restarted: bool,
    /// Unmount targets that held nothing detachable (autofs layer only); still authorized.
    #[serde(default)]
    pub unchanged: Vec<String>,
    /// Non-fatal errors — collected, not thrown, so one bad step doesn't abort.
    pub errors: Vec<String>,
    /// Non-fatal problems after the operation itself succeeded (e.g. stale ledger row left behind).
    #[serde(default)]
    pub warnings: Vec<String>,
}

// ── Pure rendering ────────────────────────────────────────────────────────────

/// Render the direct-map body for every enabled network-share mount. Non-network
/// mounts (disk/object) are ignored. Rows are sorted by target so output is
/// byte-stable across runs (what makes the on-disk diff a reliable drift check).
pub fn render_map(mounts: &[ManagedMount]) -> String {
    let mut lines: Vec<String> = mounts
        .iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .map(map_line)
        .collect();
    lines.sort();

    let mut map = String::from(HEADER);
    for line in &lines {
        map.push_str(line);
        map.push('\n');
    }
    map
}

/// The single direct-map master line pointing autofs at [`MAP_FILE`].
pub fn master_line() -> String {
    format!("/-  {MAP_FILE} --timeout={TIMEOUT_SECS}")
}

/// The absolute mountpoints (direct-map keys) orca manages for a mount set —
/// used by [`merge_master`] to evict any foreign master entry that would shadow
/// them (e.g. an existing indirect mount at an ancestor path).
fn managed_targets(mounts: &[ManagedMount]) -> Vec<String> {
    mounts
        .iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .map(|m| m.target.clone())
        .collect()
}

/// One direct-map line: `<target>  -fstype=…,<opts>  <loc1> <loc2> …`. The
/// locations are the mount's ordered sources (primary first, then failovers);
/// autofs treats multiple locations as replicated servers and fails over
/// between them.
fn map_line(m: &ManagedMount) -> String {
    let opts = autofs_options(m);
    let locations = ordered_sources(&m.source, m.failover_sources.as_deref()).join(" ");
    format!("{}  {}  {}", escape_map_key(&m.target), opts, locations)
}

/// Render the direct-map body writing the **single elected source** per mount
/// instead of all ordered sources. `elected` maps a mount `target` to the source
/// its election chose; a mount absent from the map (no live source) is omitted
/// so autofs is never handed a dead location. This is the failback-correct
/// renderer the daemon uses — [`render_map`] (all sources on one line) is kept
/// only for the legacy no-election path.
///
/// Byte-stable (sorted by target, same header) so the on-disk diff stays a
/// reliable drift check.
pub fn render_map_elected(
    mounts: &[ManagedMount],
    elected: &std::collections::HashMap<String, String>,
) -> String {
    let mut lines: Vec<String> = mounts
        .iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .filter_map(|m| elected.get(&m.target).map(|src| map_line_for(m, src)))
        .collect();
    lines.sort();

    let mut map = String::from(HEADER);
    for line in &lines {
        map.push_str(line);
        map.push('\n');
    }
    map
}

/// One direct-map line pinned to a single elected `source`:
/// `<target>  -fstype=…,<opts>  <source>`. Same shape as [`map_line`] but with
/// exactly one location so autofs cannot silently drift to a lower-priority
/// server — orca owns source selection.
fn map_line_for(m: &ManagedMount, source: &str) -> String {
    let opts = autofs_options(m);
    format!("{}  {}  {}", escape_map_key(&m.target), opts, source)
}

/// Build the autofs `-fstype=…,opt,opt` option string for a mount. The option
/// string is produced by the owning storage backend's `render_options` (so the
/// backend owns its option grammar) rather than by a local comma-split; core then
/// strips the fstab/systemd-only options (`_netdev`, `nofail`, `x-systemd.*`,
/// `auto`/`noauto`) that are meaningless to — and would invalidate — an autofs map
/// entry. A mount whose backend is not registered falls back to the mount's raw
/// option string, so map rendering never depends on a live registry.
fn autofs_options(m: &ManagedMount) -> String {
    let rendered = render_backend_options(&m.backend, m.fstype.as_str(), m.options.as_deref());
    strip_fstab_only(&m.fstype, &rendered)
}

/// Render a mount's options through the registered backend named `backend`,
/// falling back to the raw declared string when no such backend is registered.
/// Kept separate so it is trivially testable without touching the global registry.
fn render_backend_options(backend: &str, _fstype: &str, options: Option<&str>) -> String {
    use plugin_toolkit::storage::{
        NormalizedSpec, OptionSet, SecretRef, backend as lookup, render_option_set,
    };
    match lookup(backend) {
        Some(b) => {
            // The autofs map is rendered synchronously and per-source, so we
            // render from the raw option string via the backend's own
            // `render_options`. A backend that has not migrated to a typed
            // `OptionSet` renders `Raw` verbatim — byte-identical to core's prior
            // behavior; a migrated backend applies its own grammar.
            let normalized = NormalizedSpec {
                backend: backend.to_string(),
                target: String::new(),
                fstype: _fstype.to_string(),
                source: String::new(),
                failover_sources: Vec::new(),
                options: OptionSet::Raw {
                    options: options.map(str::to_string),
                },
                credential: None::<SecretRef>,
                secret_file: None,
                remount_policy: None,
                enabled: true,
            };
            b.render_options(&normalized)
        }
        None => options
            .map(str::to_string)
            .map(|o| render_option_set(&OptionSet::Raw { options: Some(o) }))
            .unwrap_or_default(),
    }
}

/// Prepend `fstype=` and strip fstab/systemd-only options from a rendered option
/// string, producing the `-fstype=…,opt,opt` autofs map field. Splitting the
/// strip out from rendering keeps the backend's grammar (rendering) and autofs's
/// constraint (this filter) as separate, independently-tested concerns.
fn strip_fstab_only(fstype: &str, rendered: &str) -> String {
    let mut parts = vec![format!("fstype={fstype}")];
    parts.extend(
        rendered
            .split(',')
            .map(str::trim)
            .filter(|o| !o.is_empty() && !is_fstab_only(o))
            .map(str::to_string),
    );
    format!("-{}", parts.join(","))
}

/// Options that belong to fstab / systemd automount, not to an autofs map entry.
fn is_fstab_only(opt: &str) -> bool {
    let key = opt.split('=').next().unwrap_or(opt);
    key.starts_with("x-systemd")
        || matches!(key, "_netdev" | "nofail" | "auto" | "noauto" | "comment")
}

/// Is `mountpoint` an ancestor-or-equal of `target`? An indirect autofs mount at
/// an ancestor path would shadow our direct mounts, so those foreign entries are
/// evicted on take-over. `/mnt/pool` shadows `/mnt/pool/data`; `/mnt/poolX` does
/// not (component-boundary aware).
fn is_ancestor_or_equal(mountpoint: &str, target: &str) -> bool {
    let a = mountpoint.trim_end_matches('/');
    let t = target.trim_end_matches('/');
    a == t || (t.starts_with(a) && t.as_bytes().get(a.len()) == Some(&b'/'))
}

/// The mountpoint (first whitespace-delimited field) of a master-map line, or
/// `None` for blanks/comments.
fn master_mountpoint(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    Some(first_map_key(t))
}

/// Does a master-file line already register our direct map ([`MAP_FILE`])?
/// True for any non-comment line that mounts `/-` at `MAP_FILE` — the shape a
/// duplicate registration takes (whether hand-added or leaked from an
/// `auto.master.d` drop-in). Used by [`merge_master`] to keep exactly one.
fn registers_our_map(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return false;
    }
    t.split_whitespace().any(|field| field == MAP_FILE)
}

/// Take-over-merge the host's master file: preserve every foreign line except
/// those whose mountpoint would shadow a target we now manage, drop any prior
/// orca-managed block, and append a fresh orca block containing [`master_line`].
///
/// This is what lets orca *take over* an existing autofs setup (e.g. an indirect
/// `/mnt/pool` map) rather than fighting it with a parallel entry over the same
/// tree — the shadowing foreign entry is removed and replaced by our direct map.
pub fn merge_master(existing: &str, managed_targets: &[String]) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_BEGIN {
            in_block = true;
            continue;
        }
        if trimmed == BLOCK_END {
            in_block = false;
            continue;
        }
        if in_block {
            continue; // old orca block — regenerated below
        }
        // Evict a foreign entry that shadows one of our managed targets.
        if let Some(mp) = master_mountpoint(line)
            && managed_targets.iter().any(|t| is_ancestor_or_equal(&mp, t))
        {
            continue;
        }
        // Guard against double-registration of our own direct map: a foreign
        // line (or a stale `auto.master.d` drop-in copied into the master file)
        // that already points autofs at `MAP_FILE`. We re-add it inside the
        // orca block, so keeping this one would register `/etc/auto.orca` twice
        // and autofs would load the map twice. Drop it.
        if registers_our_map(line) {
            continue;
        }
        kept.push(line);
    }

    let mut out = String::new();
    // Trim trailing blank lines from the kept foreign config for tidiness.
    while matches!(kept.last(), Some(l) if l.trim().is_empty()) {
        kept.pop();
    }
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(BLOCK_BEGIN);
    out.push('\n');
    out.push_str(&master_line());
    out.push('\n');
    out.push_str(BLOCK_END);
    out.push('\n');
    out
}

/// Strip orca's autofs ownership from a master file: drop the orca-managed block
/// and any line that still registers our direct map ([`MAP_FILE`]), preserving
/// every foreign line untouched. The inverse of [`merge_master`] — used to
/// RETIRE the autofs direct map on hosts where native convergence
/// ([`crate::mount_converge`]) is now the sole mount owner. Pure; idempotent
/// (a master with no orca block returns unchanged modulo trailing-blank tidy).
pub fn retire_master(existing: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_BEGIN {
            in_block = true;
            continue;
        }
        if trimmed == BLOCK_END {
            in_block = false;
            continue;
        }
        // Drop the orca block and any bare direct-map registration (a hand-added
        // or drop-in-leaked `/-  /etc/auto.orca` line outside our markers).
        if in_block || registers_our_map(line) {
            continue;
        }
        kept.push(line);
    }
    while matches!(kept.last(), Some(l) if l.trim().is_empty()) {
        kept.pop();
    }
    let mut out = String::new();
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ── Host detection (daemon side; needs no root — these paths are readable) ─────

/// The autofs master file this host actually reads. Alpine ships
/// `/etc/autofs/auto.master`; Debian/RHEL/systemd use `/etc/auto.master`.
/// Detection prefers whichever exists, Alpine's location first.
pub fn detect_master_file() -> &'static str {
    if Path::new("/etc/autofs/auto.master").exists() {
        "/etc/autofs/auto.master"
    } else {
        "/etc/auto.master"
    }
}

/// systemd if `/run/systemd/system` exists (the canonical runtime probe), else
/// OpenRC. autofs on both is restarted (not reloaded): a master-map change is
/// only picked up by a full restart.
pub fn detect_init() -> Init {
    if Path::new("/run/systemd/system").exists() {
        Init::Systemd
    } else {
        Init::OpenRc
    }
}

/// Paths the root helper is permitted to write. Anything else is refused even
/// though the caller is trusted — defense in depth on the privileged surface.
fn is_allowed_write(path: &str) -> bool {
    path == MAP_FILE
        || path == "/etc/auto.master"
        || path == "/etc/autofs/auto.master"
        // A root-owned 0600 secret-file, but ONLY a path that is a legal,
        // traversal-proof secret-file inside `SECRET_FILE_DIR` (see
        // `storage::is_valid_secret_file_path`) — never an arbitrary path.
        || plugin_toolkit::storage::is_valid_secret_file_path(path)
}

// ── Daemon side: planning + bridge to the privileged helper ───────────────────

/// Outcome of applying a plan, surfaced by the `storage.mount` tool.
#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    /// Files whose contents actually changed (the drift set). Empty = the host
    /// already matched the store (clean no-op, no privileged call made).
    pub changed: Vec<String>,
    /// Whether autofs was restarted.
    pub reloaded: bool,
    /// Non-fatal errors.
    pub errors: Vec<String>,
}

/// Build the privileged [`PrivilegedOp::Apply`] for a mount set. Reads the
/// current master file (world-readable) to take-over-merge it, and diffs both
/// files against disk so only genuinely-changed files are written (an unchanged
/// host yields empty `writes`; the placements still ride along so root keeps its
/// ledger current).
pub async fn plan(mounts: &[ManagedMount]) -> PrivilegedOp {
    plan_with_map(
        mounts,
        render_map(mounts),
        map_placements(mounts, None),
        None,
    )
    .await
}

/// Ledger entries for the map's mounts that carry a placement id. With `elected`,
/// the recorded source is the elected one; a mount with none is not in the map and
/// is skipped.
fn map_placements(
    mounts: &[ManagedMount],
    elected: Option<&std::collections::HashMap<String, String>>,
) -> Vec<MapPlacement> {
    mounts
        .iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .filter_map(|m| {
            let source = match elected {
                Some(e) => e.get(&m.target)?.clone(),
                None => m.source.clone(),
            };
            Some(MapPlacement {
                id: m.placement_id.clone()?,
                target: m.target.clone(),
                source,
            })
        })
        .collect()
}

/// [`plan`] against a pre-rendered map body. Shared by the legacy all-sources
/// path ([`render_map`]) and the elected single-source path
/// ([`render_map_elected`]); both need the same master take-over-merge + diff.
async fn plan_with_map(
    mounts: &[ManagedMount],
    map: String,
    placements: Vec<MapPlacement>,
    elected: Option<&std::collections::HashMap<String, String>>,
) -> PrivilegedOp {
    let master_path = detect_master_file();
    let existing_master = tokio::fs::read_to_string(master_path)
        .await
        .unwrap_or_default();
    let master = merge_master(&existing_master, &managed_targets(mounts));

    let mut writes = Vec::new();
    for (path, contents) in [(MAP_FILE, map), (master_path, master)] {
        let on_disk = tokio::fs::read_to_string(path).await.unwrap_or_default();
        if on_disk != contents {
            writes.push(FileWrite {
                path: path.to_string(),
                contents,
                mode: None,
            });
        }
    }

    // The autofs map path carries no secret-file materialization: a backend that
    // needs a root-owned secret-file (inline-SMB credentials) produces it as a
    // generic `SecretFile` on the native mount path (`PrivilegedOp::Mount`), so
    // core never renders any backend's credential grammar. The `keep_secret_files`
    // reaping set stays empty here; the native path owns secret-file teardown.
    PrivilegedOp::Apply {
        writes,
        keep_secret_files: Vec::new(),
        init: detect_init(),
        placements,
        managed: Some(ManagedMaps::from_mounts(mounts, elected)),
    }
}

/// Render + plan + apply for a mount set. Idempotent: an unchanged host makes no
/// privileged call at all.
pub async fn apply(mounts: &[ManagedMount]) -> ApplyOutcome {
    if let Some(outcome) = refuse_unrepresentable(mounts) {
        return outcome;
    }
    apply_op(plan(mounts).await).await
}

/// Render the **elected single-source** map, plan, and apply. This is the
/// failback-correct daemon path: each mount is pinned to the source its election
/// chose (see [`crate::source_election`]). Idempotent — no privileged call when
/// the on-disk map already matches.
pub async fn apply_elected(
    mounts: &[ManagedMount],
    elected: &std::collections::HashMap<String, String>,
) -> ApplyOutcome {
    if let Some(outcome) = refuse_unrepresentable(mounts) {
        return outcome;
    }
    apply_op(
        plan_with_map(
            mounts,
            render_map_elected(mounts, elected),
            map_placements(mounts, Some(elected)),
            Some(elected),
        )
        .await,
    )
    .await
}

fn unrepresentable_key_char(c: char) -> bool {
    ((c.is_control() || c.is_whitespace()) && c != ' ' && c != '\t') || c == '"' || c == '\''
}

/// A newline, NUL, other control/whitespace char or quote in a target cannot be expressed in an autofs map line, so
/// the whole apply is refused rather than writing a corrupt map.
fn refuse_unrepresentable(mounts: &[ManagedMount]) -> Option<ApplyOutcome> {
    let errors: Vec<String> = mounts
        .iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .filter_map(|m| validate_mount_fields(m).err())
        .collect();
    (!errors.is_empty()).then(|| ApplyOutcome {
        errors,
        ..ApplyOutcome::default()
    })
}

const FOREIGN_MAP_SCHEMES: &[&str] = &[
    "program:", "file:", "yp:", "nis:", "ldap:", "ldaps:", "sss:", "exec:", "hesiod:", "multi:",
];

fn token_char_forbidden(c: char) -> bool {
    c.is_control() || c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '`' | '$')
}

fn validate_location(source: &str) -> Result<(), String> {
    let lower = source.to_ascii_lowercase();
    if source.is_empty()
        || source.starts_with('-')
        || source.contains(token_char_forbidden)
        || FOREIGN_MAP_SCHEMES.iter().any(|p| lower.starts_with(p))
    {
        return Err(format!("source {source:?} is not a safe mount location"));
    }
    Ok(())
}

fn validate_options(options: &str) -> Result<(), String> {
    if options.contains(token_char_forbidden) {
        return Err(format!("options {options:?} contain a forbidden character"));
    }
    Ok(())
}

/// Every mount field that reaches a map line or mount argv, checked at the
/// root helper so an unprivileged caller cannot smuggle grammar through it.
fn validate_mount_fields(m: &ManagedMount) -> Result<(), String> {
    validate_mount_strs(
        &m.target,
        &m.source,
        m.failover_sources.as_deref().unwrap_or(""),
        m.options.as_deref().unwrap_or(""),
    )
}

fn validate_mount_strs(
    target: &str,
    source: &str,
    failovers: &str,
    options: &str,
) -> Result<(), String> {
    if target.contains(unrepresentable_key_char) || !target.starts_with('/') {
        return Err(format!(
            "mount target {target:?} is not a safe absolute path"
        ));
    }
    validate_location(source)?;
    for s in failovers.lines().map(str::trim).filter(|s| !s.is_empty()) {
        validate_location(s)?;
    }
    validate_options(options)
}

fn validate_map_contents(contents: &str) -> Result<(), String> {
    if let Some(c) = contents
        .chars()
        .find(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(format!("map contains control character {c:?}"));
    }
    for line in contents.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let key = first_map_key(t);
        if !key.starts_with('/') {
            return Err(format!("map key {key:?} is not an absolute path"));
        }
        let mut rest = t;
        let mut chars = t.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                ' ' | '\t' => {
                    rest = &t[i..];
                    break;
                }
                _ => {}
            }
            rest = "";
        }
        let mut fields = rest.split_whitespace();
        let opts = fields.next().ok_or("map entry has no options")?;
        if !opts.starts_with('-') {
            return Err(format!("map options {opts:?} must start with '-'"));
        }
        validate_options(opts)?;
        let mut n = 0;
        for loc in fields {
            validate_location(loc)?;
            n += 1;
        }
        if n == 0 {
            return Err("map entry has no location".into());
        }
    }
    Ok(())
}

/// The master file may register the orca-owned direct map; any other line must
/// already be present verbatim in the current on-disk master (`existing`), so
/// Apply can preserve pre-existing entries but never add or alter one.
fn validate_master_contents(contents: &str, existing: &str) -> Result<(), String> {
    if let Some(c) = contents
        .chars()
        .find(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(format!("master contains control character {c:?}"));
    }
    for line in contents.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        let timeout_ok = |o: &str| {
            o.strip_prefix("--timeout=")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        let ok = match f.as_slice() {
            ["/-", map] => *map == MAP_FILE,
            ["/-", map, o] => *map == MAP_FILE && timeout_ok(o),
            _ => false,
        };
        if !ok && !existing.lines().any(|l| l.trim() == t) {
            return Err(format!(
                "master entry {t:?} is neither the orca direct map nor already present"
            ));
        }
    }
    Ok(())
}

/// Checks the structured mounts and that the map write is exactly their
/// rendering, so nothing but the renderer's output can reach the map file.
/// A map write without structured mounts is refused outright. Retiring the map
/// needs zero mounts; its contents must be the header-only render or blank.
fn validate_managed(managed: Option<&ManagedMaps>, writes: &[FileWrite]) -> Vec<String> {
    let map_write = writes.iter().find(|w| w.path == MAP_FILE);
    let Some(managed) = managed else {
        return map_write
            .map(|_| {
                vec![format!(
                    "refused write {MAP_FILE}: map write requires structured mounts"
                )]
            })
            .unwrap_or_default();
    };
    let mut errors: Vec<String> = managed
        .managed_mounts()
        .iter()
        .filter_map(|m| validate_mount_fields(m).err())
        .collect();
    errors.extend(
        managed
            .elected
            .iter()
            .flatten()
            .filter_map(|(_, src)| validate_location(src).err()),
    );
    if errors.is_empty()
        && let Some(w) = map_write
        && w.contents != managed.render()
        && !(managed.mounts.is_empty() && w.contents.trim().is_empty())
    {
        errors.push(format!(
            "refused write {MAP_FILE}: map does not match its structured mounts"
        ));
    }
    errors
}

fn validate_apply_writes(writes: &[FileWrite], existing_master: &str) -> Vec<String> {
    writes
        .iter()
        .filter_map(|w| {
            let r = if !is_allowed_write(&w.path) {
                Err(format!("refused non-allowlisted path: {}", w.path))
            } else if w.path == MAP_FILE {
                validate_map_contents(&w.contents)
            } else if w.path.ends_with("auto.master") {
                validate_master_contents(&w.contents, existing_master)
            } else {
                Ok(())
            };
            r.err().map(|e| format!("refused write {}: {e}", w.path))
        })
        .collect()
}

/// Run a planned [`PrivilegedOp::Apply`], short-circuiting an empty diff with no
/// placements to record.
async fn apply_op(op: PrivilegedOp) -> ApplyOutcome {
    match op {
        PrivilegedOp::Apply {
            ref writes,
            ref placements,
            ..
        } if writes.is_empty() && placements.is_empty() => ApplyOutcome::default(),
        op => {
            let r = run_privileged(&op).await;
            ApplyOutcome {
                changed: r.changed,
                reloaded: r.restarted,
                errors: r.errors,
            }
        }
    }
}

/// Retire orca's autofs direct map on this host: rewrite the master file without
/// the orca block (and any bare `/-  MAP_FILE` registration), then restart autofs
/// so it stops mounting the retired targets. Native convergence
/// ([`crate::mount_converge`]) is the sole mount owner now, so the autofs direct
/// map is a second, competing writer of the same tree — it stacked mounts and
/// masked converge's active-source read, defeating option-drift reconcile.
///
/// Self-healing for the ALREADY-DEPLOYED fleet: called once on daemon startup.
/// Idempotent — a master with no orca block diffs clean and makes no privileged
/// call. Best-effort: errors are returned in [`ApplyOutcome`], never thrown, so a
/// host that can't rewrite its master still boots.
pub async fn retire_direct_map() -> ApplyOutcome {
    let master_path = detect_master_file();
    let existing = tokio::fs::read_to_string(master_path)
        .await
        .unwrap_or_default();
    let retired = retire_master(&existing);
    if retired == existing {
        return ApplyOutcome::default();
    }
    apply_op(PrivilegedOp::Apply {
        writes: vec![FileWrite {
            path: master_path.to_string(),
            contents: retired,
            mode: None,
        }],
        keep_secret_files: Vec::new(),
        init: detect_init(),
        placements: Vec::new(),
        managed: None,
    })
    .await
}

/// Bridge to the root helper: spawn `sudo -n <self> admin storage-apply` and
/// pipe the op as JSON on stdin, returning the parsed [`PrivilegedResult`]. A
/// spawn/parse failure (e.g. no sudoers grant) surfaces as an error in the
/// result rather than a panic.
pub async fn run_privileged(op: &PrivilegedOp) -> PrivilegedResult {
    let refused = |e: String| PrivilegedResult {
        errors: vec![e],
        ..Default::default()
    };
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => return refused(format!("serialize op: {e}")),
    };
    let call = utils::privileged::PrivilegedCall::helper(
        "storage-apply",
        payload,
        utils::privileged::STORAGE_APPLY_TIMEOUT,
    );
    match utils::privileged::run(call).await {
        Ok(out) if out.success => serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            refused(format!(
                "parse helper output: {e}: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ))
        }),
        Ok(out) => refused(format!(
            "helper exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => refused(format!("run sudo helper: {e}")),
    }
}

// ── Root side: the privileged executor (runs inside `orca admin storage-apply`) ─

async fn execute_release(ids: &[String]) -> PrivilegedResult {
    let mapped = read_map_file().unwrap_or_default();
    release_locked(
        LEDGER_FILE,
        ids,
        &declared_map_keys(&mapped),
        network_sources,
        |t| async move { force_unmount(&t).await },
    )
    .await
}

/// Holds the ledger lock from the authorization check through the last umount so
/// a concurrent Mount/Apply/Adopt cannot remount the target in between.
async fn release_locked<S, U, F>(
    path: &str,
    ids: &[String],
    map_keys: &std::collections::HashSet<String>,
    sources: S,
    umount: U,
) -> PrivilegedResult
where
    S: Fn(&str) -> Result<Vec<String>, String>,
    U: FnMut(String) -> F,
    F: std::future::Future<Output = Result<Unwound, String>>,
{
    let _lock = match lock_ledger(path).await {
        Ok(l) => l,
        Err(e) => {
            let mut res = PrivilegedResult::default();
            res.errors
                .extend(ids.iter().map(|id| format!("release {id}: refused: {e}")));
            return res;
        }
    };
    let (mut res, released) = guard_release(ids, || read_ledger(path), sources, umount).await;
    if let Err(e) = ledger_release(path, &released, map_keys).await {
        res.warnings
            .push(format!("released, but ledger cleanup failed: {e}"));
    }
    res
}

/// Drops released rows not still keyed in the map from one read of the ledger; an
/// unreadable ledger leaves every row in place. Caller must hold [`lock_ledger`].
async fn ledger_release(
    path: &str,
    released: &[String],
    map_keys: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if released.is_empty() {
        return Ok(());
    }
    let mut ledger = parse_ledger(&read_ledger(path)?)?;
    let drop = unmapped_ids(released.to_vec(), &ledger, map_keys);
    let before = ledger.len();
    for id in &drop {
        ledger.remove(id);
    }
    if ledger.len() == before {
        return Ok(());
    }
    write_atomic(path, &render_ledger(&ledger), Some(0o600))
        .await
        .map_err(|e| format!("write mount ledger {path}: {e}"))
}

fn read_map_file() -> Result<String, String> {
    match std::fs::read_to_string(MAP_FILE) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        r => r.map_err(|e| e.to_string()),
    }
}

/// Released ids whose ledger row stays: a target still keyed in the map is
/// remounted by autofs, so its row must outlive the release.
fn unmapped_ids(
    released: Vec<String>,
    ledger: &Ledger,
    map_keys: &std::collections::HashSet<String>,
) -> Vec<String> {
    released
        .into_iter()
        .filter(|id| ledger.get(id).is_none_or(|e| !map_keys.contains(&e.target)))
        .collect()
}

/// Ledger changes for an `Apply` that moved the map from `old_keys` to `new_keys`.
/// Placements are recorded only for targets keyed in the new map. A row whose
/// target left the map is dropped only when no network mount is still live there;
/// a live one stays so it remains releasable by id.
fn map_ledger_changes<S>(
    old_keys: &std::collections::HashSet<String>,
    new_keys: &std::collections::HashSet<String>,
    placements: &[MapPlacement],
    ledger: &Ledger,
    live_sources: S,
) -> (Vec<(String, LedgerEntry)>, Vec<String>, Vec<String>)
where
    S: Fn(&str) -> Result<Vec<String>, String>,
{
    let mut adds = Vec::new();
    let mut errors = Vec::new();
    for p in placements {
        let keyed = normalize_unmount_target(&p.target).is_ok_and(|t| new_keys.contains(&t));
        if keyed {
            adds.push((
                p.id.clone(),
                LedgerEntry {
                    target: p.target.clone(),
                    source: p.source.clone(),
                },
            ));
        } else {
            errors.push(format!(
                "refused ledger entry for {:?}: not a key of the written map",
                p.target
            ));
        }
    }
    let removes = ledger
        .iter()
        .filter(|(_, e)| old_keys.contains(&e.target) && !new_keys.contains(&e.target))
        .filter(|(_, e)| live_sources(&e.target).is_ok_and(|live| live.is_empty()))
        .map(|(id, _)| id.clone())
        .collect();
    (adds, removes, errors)
}

async fn sync_map_ledger(old_map: &str, placements: &[MapPlacement], res: &mut PrivilegedResult) {
    let new_map = match read_map_file() {
        Ok(m) => m,
        Err(e) => {
            res.errors.push(format!("read {MAP_FILE}: {e}"));
            return;
        }
    };
    let ledger = match read_ledger(LEDGER_FILE).and_then(|c| parse_ledger(&c)) {
        Ok(l) => l,
        Err(e) => {
            res.errors.push(e);
            return;
        }
    };
    let (adds, removes, errors) = map_ledger_changes(
        &declared_map_keys(old_map),
        &declared_map_keys(&new_map),
        placements,
        &ledger,
        network_sources,
    );
    res.errors.extend(errors);
    if let Err(e) = ledger_update(LEDGER_FILE, &adds, &removes).await {
        res.errors.push(e);
    }
}

/// Execute a validated [`PrivilegedOp`] as root. Called only from the
/// `admin storage-apply` CLI path (via `sudo`). Validates every write path,
/// writes atomically (temp + rename), and restarts autofs.
pub async fn execute_privileged(op: PrivilegedOp) -> PrivilegedResult {
    match op {
        PrivilegedOp::Apply {
            writes,
            keep_secret_files: _,
            init,
            placements,
            managed,
        } => {
            let mut res = PrivilegedResult::default();
            let existing_master = std::fs::read_to_string(detect_master_file()).unwrap_or_default();
            let mut refused = validate_apply_writes(&writes, &existing_master);
            refused.extend(validate_managed(managed.as_ref(), &writes));
            if !refused.is_empty() {
                res.errors = refused;
                return res;
            }
            let old_map = read_map_file().unwrap_or_default();
            for w in &writes {
                match write_atomic(&w.path, &w.contents, w.mode).await {
                    Ok(()) => {
                        res.changed.push(w.path.clone());
                    }
                    Err(e) => res.errors.push(format!("write {}: {e}", w.path)),
                }
            }
            if !res.changed.is_empty() {
                match restart_autofs(init).await {
                    Ok(()) => res.restarted = true,
                    Err(e) => res.errors.push(format!("restart autofs: {e}")),
                }
            }
            if !placements.is_empty() || res.changed.iter().any(|c| c == MAP_FILE) {
                sync_map_ledger(&old_map, &placements, &mut res).await;
            }
            res
        }
        PrivilegedOp::Release { ids } => execute_release(&ids).await,
        PrivilegedOp::Adopt { id, target, source } => {
            let mut res = PrivilegedResult::default();
            match adopt_with(
                LEDGER_FILE,
                &id,
                &target,
                &source,
                network_sources,
                orca_owned,
            )
            .await
            {
                Ok(target) => res.changed.push(target),
                Err(e) => res.errors.push(format!("adopt {id}: refused: {e}")),
            }
            res
        }
        PrivilegedOp::Mount {
            mounts,
            keep_secret_files,
        } => {
            let mut res = PrivilegedResult::default();
            let refused: Vec<String> = mounts
                .iter()
                .filter_map(|m| {
                    validate_mount_strs(&m.target, &m.source, "", &m.options)
                        .and_then(|()| {
                            m.fstype
                                .contains(token_char_forbidden)
                                .then(|| format!("fstype {:?} is forbidden", m.fstype))
                                .map_or(Ok(()), Err)
                        })
                        .err()
                })
                .collect();
            if !refused.is_empty() {
                res.errors = refused;
                return res;
            }
            for m in &mounts {
                match crate::mount_exec::run_mount(m).await {
                    Ok(()) => {
                        res.changed.push(m.target.clone());
                        match observed_ledger_entry(
                            plugin_toolkit::storage::mount_table::mount_table(),
                            &m.target,
                        ) {
                            Ok(entry) => {
                                if let Err(e) = ledger_update(
                                    LEDGER_FILE,
                                    &[(m.placement_id.clone(), entry)],
                                    &[],
                                )
                                .await
                                {
                                    res.errors.push(e);
                                }
                            }
                            Err(w) => res.warnings.push(w),
                        }
                    }
                    Err(e) => res
                        .errors
                        .push(format!("mount {} from {}: {e}", m.target, m.source)),
                }
            }
            // Reap secret-files not in the authoritative keep-set (deleted mount /
            // rotated secret), scoped to `SECRET_FILE_DIR`. Same generic reaper the
            // autofs `Apply` path uses — grammar-agnostic, path-validated.
            reap_orphan_secret_files(&keep_secret_files, &mut res).await;
            res
        }
        PrivilegedOp::Reload { init } => {
            let mut res = PrivilegedResult::default();
            match restart_autofs(init).await {
                Ok(()) => res.restarted = true,
                Err(e) => res.errors.push(format!("reload autofs: {e}")),
            }
            res
        }
        PrivilegedOp::SecretFiles {
            files,
            keep_secret_files,
        } => {
            let mut res = PrivilegedResult::default();
            for f in &files {
                match crate::mount_exec::materialize_secret_file(f).await {
                    Ok(()) => res.changed.push(f.path.clone()),
                    Err(e) => res.errors.push(e),
                }
            }
            reap_orphan_secret_files(&keep_secret_files, &mut res).await;
            res
        }
        PrivilegedOp::SetShareMode {
            path,
            mode,
            recursive,
        } => {
            let mut res = PrivilegedResult::default();
            // Defense in depth on the privileged surface (the caller is trusted,
            // but this is root): refuse any mode that would escalate privilege or
            // lock the owner out, and any path that is not a real subtree strictly
            // under a `/mnt/user/<share>`.
            if !is_allowed_share_mode(mode) {
                res.errors.push(format!(
                    "refused unsafe mode {mode:#o}: must be <= 0o777, no setuid/setgid/sticky, owner rwx"
                ));
            } else if !is_allowed_share_path(&path) {
                res.errors.push(format!(
                    "refused path outside a managed share subtree (/mnt/user/<share>/…): {path}"
                ));
            } else {
                match set_mode_tree(&path, mode, recursive) {
                    Ok(n) => {
                        res.changed.push(path.clone());
                        tracing::info!(path = %path, mode = format!("{mode:#o}"), entries = n, "share mode repaired");
                    }
                    Err(e) => res.errors.push(format!("chmod {path}: {e}")),
                }
            }
            res
        }
        PrivilegedOp::SetShareOwner {
            path,
            uid,
            gid,
            recursive,
        } => {
            let mut res = PrivilegedResult::default();
            if !is_allowed_share_owner(uid, gid) {
                res.errors.push(format!(
                    "refused unsafe owner {uid}:{gid}: a repair never chowns a share to root"
                ));
            } else if !is_allowed_share_path(&path) {
                res.errors.push(format!(
                    "refused path outside a managed share subtree (/mnt/user/<share>/…): {path}"
                ));
            } else {
                match set_owner_tree(&path, uid, gid, recursive) {
                    Ok(n) => {
                        res.changed.push(path.clone());
                        tracing::info!(path = %path, uid, gid, entries = n, "share ownership repaired");
                    }
                    Err(e) => res.errors.push(format!("chown {path}: {e}")),
                }
            }
            res
        }
    }
}

/// The owner a share-permission repair may set.
///
/// Never root. Chowning a share tree to uid/gid 0 is the escalation direction —
/// it would hand root ownership of operator data and can lock the real service
/// account out of its own tree. Every legitimate repair target is a service
/// account (PBS runs as 34; Unraid shares are 99:100).
fn is_allowed_share_owner(uid: u32, gid: u32) -> bool {
    uid != 0 && gid != 0
}

/// Set owner on `path`, and — when `recursive` — everything beneath it,
/// returning the number of entries changed.
///
/// Symlinks are changed with `lchown` semantics (never followed), so a symlink
/// planted in the tree cannot redirect ownership onto a file outside it — the
/// same containment [`set_mode_tree`] gets by refusing to chmod symlinks.
/// Callers must have already validated `path` with [`is_allowed_share_path`].
fn set_owner_tree(path: &str, uid: u32, gid: u32, recursive: bool) -> std::io::Result<usize> {
    let mut changed = 0usize;
    // `lchown`, not `chown`: acts on the link itself.
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    changed += 1;
    let meta = std::fs::symlink_metadata(path)?;
    if recursive && meta.file_type().is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            let child_str = child.to_string_lossy().to_string();
            let child_meta = std::fs::symlink_metadata(&child)?;
            if child_meta.file_type().is_dir() {
                changed += set_owner_tree(&child_str, uid, gid, true)?;
            } else {
                std::os::unix::fs::lchown(&child, Some(uid), Some(gid))?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}

/// The mode a share-permission repair may set. Additive, non-escalating: at most
/// `0o777` (no setuid/setgid/sticky bits), and owner always keeps `rwx` (so a
/// repair can never lock the share's owner out of its own tree). Everything else
/// is refused even from the trusted caller.
fn is_allowed_share_mode(mode: u32) -> bool {
    mode <= 0o777 && (mode & 0o700) == 0o700
}

/// Is `path` a real subtree strictly under a `/mnt/user/<share>` the repair is
/// permitted to touch? Canonicalizes first (resolving symlinks and `..`), so a
/// symlink or traversal that escapes the share root is refused; the target must
/// exist. `/mnt/user` itself (no share component) is refused — a repair acts on a
/// specific share subtree, never the whole array root.
fn is_allowed_share_path(path: &str) -> bool {
    path_under_share_root(path, SHARE_ROOT)
}

/// Core of [`is_allowed_share_path`], parameterized on the share root so it is
/// unit-testable without a real `/mnt/user`. Canonicalizes both sides (resolving
/// symlinks and `..`), then requires the target to sit strictly BELOW
/// `root/<share>` — at least one component past the root, all components normal.
fn path_under_share_root(path: &str, root: &str) -> bool {
    let (Ok(canon), Ok(root_canon)) = (std::fs::canonicalize(path), std::fs::canonicalize(root))
    else {
        return false;
    };
    let Ok(rel) = canon.strip_prefix(&root_canon) else {
        return false;
    };
    // At least the `<share>` component (never the root itself), and only normal
    // components (canonicalize has already collapsed any `.`/`..`).
    let mut comps = rel.components();
    let Some(std::path::Component::Normal(_)) = comps.next() else {
        return false;
    };
    rel.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The Unraid user-share root every managed share lives under.
const SHARE_ROOT: &str = "/mnt/user";

/// Set `mode` on `path`, and — when `recursive` — on everything beneath it,
/// returning the number of entries changed. Symlinks are neither chmod'd nor
/// followed, so a symlink planted in the tree can never redirect the mode change
/// outside it. Callers must have already validated `path` with
/// [`is_allowed_share_path`].
fn set_mode_tree(path: &str, mode: u32, recursive: bool) -> std::io::Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    let mut changed = 0usize;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        // Never chmod a symlink (would affect its target, potentially outside the
        // tree). The validated `path` root should not be one, but guard anyway.
        return Ok(0);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    changed += 1;
    if recursive && meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            let child_meta = std::fs::symlink_metadata(&child)?;
            if child_meta.file_type().is_symlink() {
                continue; // skip symlinks — don't chmod, don't recurse
            }
            let child_str = child.to_string_lossy();
            changed += set_mode_tree(&child_str, mode, true)?;
        }
    }
    Ok(changed)
}

/// Atomic write: create the parent dir, write a sibling temp file, then rename
/// over the target so a reader never sees a half-written map. When `mode` is set
/// (a secret-file), the temp file is created at that mode, so the bytes are never
/// visible at a laxer one.
pub(crate) async fn write_atomic(
    path: &str,
    contents: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let p = Path::new(path);
    if let Some(dir) = p.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = format!("{path}.orca.tmp");
    tokio::fs::remove_file(&tmp).await.ok();
    let mut open = tokio::fs::OpenOptions::new();
    open.write(true).create_new(true);
    if let Some(m) = mode {
        open.mode(m);
    }
    let mut f = open.open(&tmp).await?;
    let written = async {
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(m))
                .await?;
        }
        f.write_all(contents.as_bytes()).await?;
        f.sync_all().await?;
        drop(f);
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(e) = written {
        tokio::fs::remove_file(&tmp).await.ok();
        return Err(e);
    }
    if let Some(dir) = p.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        tokio::fs::File::open(dir).await?.sync_all().await?;
    }
    Ok(())
}

/// Remove secret-files under [`plugin_toolkit::storage::SECRET_FILE_DIR`] not in
/// the authoritative `keep` set — the teardown path. A deleted mount, or one whose
/// secret moved to file/guest/none, is absent from `keep`, so its stale secret-file
/// is reaped here rather than lingering with a resolved secret on disk. Only files
/// this scan can prove are orca secret-files (valid secret-file names) are touched;
/// any foreign file in the directory is left alone. If the directory does not
/// exist yet (no inline-secret mount has ever applied), this is a no-op.
///
/// Also performs a one-time migration: the legacy SMB-named directory
/// (`/etc/orca/smb-creds`, [`LEGACY_SECRET_FILE_DIR`]) is removed wholesale so no
/// root-owned secret files are orphaned after the seam was genericized. Remove
/// that step once the fleet has fully converged onto [`SECRET_FILE_DIR`].
async fn reap_orphan_secret_files(keep: &[String], res: &mut PrivilegedResult) {
    reap_orphan_secret_files_in(plugin_toolkit::storage::SECRET_FILE_DIR, keep, res).await;
    reap_legacy_secret_file_dir(res).await;
}

/// One-time filesystem migration: delete the legacy `/etc/orca/smb-creds` tree.
/// Best-effort — absent dir is a no-op; a removal error is recorded but not fatal.
async fn reap_legacy_secret_file_dir(res: &mut PrivilegedResult) {
    let legacy = plugin_toolkit::storage::LEGACY_SECRET_FILE_DIR;
    match tokio::fs::remove_dir_all(legacy).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => res
            .errors
            .push(format!("remove legacy secret-file dir {legacy}: {e}")),
    }
}

/// [`reap_orphan_secret_files`] against an explicit directory. Split out so the
/// teardown logic is testable without the fixed `SECRET_FILE_DIR` const. A file is
/// reaped iff its full path passes [`is_valid_secret_file_path`] (proving it is an
/// orca secret-file, not a foreign file) AND is absent from `keep`.
async fn reap_orphan_secret_files_in(dir: &str, keep: &[String], res: &mut PrivilegedResult) {
    use plugin_toolkit::storage::is_valid_secret_file_path;

    let kept: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();

    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(d) => d,
        Err(_) => return, // dir absent → nothing to reap
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let Some(path_str) = path.to_str() else {
            continue;
        };
        // Under the real SECRET_FILE_DIR the full path is validated. Under a test
        // dir the full path won't match SECRET_FILE_DIR, so validate the basename
        // shape via a synthesized SECRET_FILE_DIR-rooted path — same classification.
        let name = entry.file_name();
        let synth = format!(
            "{}/{}",
            plugin_toolkit::storage::SECRET_FILE_DIR,
            name.to_string_lossy()
        );
        if is_valid_secret_file_path(&synth)
            && !kept.contains(path_str)
            && let Err(e) = tokio::fs::remove_file(&path).await
        {
            res.errors.push(format!("reap secret-file {path_str}: {e}"));
        }
    }
}

/// Restart autofs for the detected init. A master-map change is only picked up
/// by a full restart (not a SIGHUP/reload), so we always restart.
async fn restart_autofs(init: Init) -> Result<(), String> {
    let (bin, args): (&str, &[&str]) = match init {
        Init::Systemd => ("systemctl", &["restart", "autofs"]),
        Init::OpenRc => ("rc-service", &["autofs", "restart"]),
    };
    let out = Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ── Trigger + self-heal (probes are unprivileged; unmount routes via helper) ───

/// Force an immediate mount of each target by accessing it (a direct-map
/// mountpoint mounts on access). Best-effort — used after an apply so declared
/// mounts come up now rather than on first consumer access.
pub async fn trigger(targets: &[String]) -> Vec<String> {
    trigger_with(
        targets,
        |t| async move {
            Command::new("stat")
                .arg("--")
                .arg(t)
                .kill_on_drop(true)
                .output()
                .await
                .map(|_| ())
        },
        || tokio::time::sleep(TRIGGER_TIMEOUT),
    )
    .await
}

/// `stat` on a wedged hard mount enters D-state and never returns, so each
/// access is abandoned (child killed on drop) after this bound.
const TRIGGER_TIMEOUT: Duration = Duration::from_secs(10);

async fn trigger_with<R, F, D, DF>(targets: &[String], run: R, deadline: D) -> Vec<String>
where
    R: Fn(String) -> F,
    F: std::future::Future<Output = std::io::Result<()>>,
    D: Fn() -> DF,
    DF: std::future::Future<Output = ()>,
{
    let mut errors = Vec::new();
    for t in targets {
        tokio::select! {
            r = run(t.clone()) => {
                if let Err(e) = r {
                    errors.push(format!("trigger {t}: {e}"));
                }
            }
            () = deadline() => {
                errors.push(format!("trigger {t}: timed out; still stale"));
            }
        }
    }
    errors
}

/// Release then re-trigger. A failed release leaves the network mount live, so
/// triggering it would block on the wedged server; skip it and report the errors.
async fn release_then_trigger<R, RF, T, TF>(target: &str, release: R, trigger: T) -> Vec<String>
where
    R: FnOnce() -> RF,
    RF: std::future::Future<Output = Vec<String>>,
    T: FnOnce() -> TF,
    TF: std::future::Future<Output = Vec<String>>,
{
    let mut errors = release().await;
    if errors.is_empty() {
        errors.extend(trigger().await);
    } else {
        errors.push(format!("trigger {target}: skipped, release failed"));
    }
    errors
}

/// Outcome of a [`recover`] self-heal sweep over autofs-managed targets.
#[derive(Debug, Clone, Default)]
pub struct RecoverOutcome {
    pub recovered: Vec<String>,
    pub still_stale: Vec<String>,
    pub healthy: Vec<String>,
    pub errors: Vec<String>,
    pub no_stale_found: bool,
}

/// Time-bounded liveness probe of one mountpoint, offloaded to the blocking pool
/// so a hung `stat` never stalls the async runtime for the whole timeout.
pub async fn probe(target: &str, health_timeout: Duration) -> MountHealth {
    let target = target.to_string();
    tokio::task::spawn_blocking(move || probe_health(&target, health_timeout))
        .await
        .unwrap_or(MountHealth::Error)
}

/// Probe every target and return those that need recovery — stale, hung
/// (`Timeout`), or not-mounted (`Missing`). Healthy and indeterminate (`Error`)
/// targets are omitted (never act on an ambiguous probe). This is the probe-only
/// half the self-heal loop calls each tick *without* acting.
pub async fn probe_stale(targets: &[String], health_timeout: Duration) -> Vec<String> {
    // Read the kernel mount table once so we can catch the false-positive
    // `probe_health` misses: a managed mountpoint that exists as a bare directory
    // with NOTHING mounted through it (no autofs trigger, no real fs). `stat`
    // succeeds on the empty dir, so `probe_health` returns `MountHealth::Ok` and the
    // mount looks healthy — but autofs never loaded the direct-map entry (the
    // freyr / frigg "map written but not reloaded" case). The only reliable
    // signal is the mount table itself. See [[project-orca-nfs-automaster-defect]].
    let table = mount_table().unwrap_or_default();
    let mut stale = Vec::new();
    for target in targets {
        let needs_recovery = matches!(
            probe(target, health_timeout).await,
            MountHealth::Stale | MountHealth::Timeout | MountHealth::Missing
        ) || target_absent_from_table(&table, target);
        if needs_recovery {
            stale.push(target.clone());
        }
    }
    stale
}

/// True when the kernel mount table has NO entry at `target` — neither an autofs
/// trigger (the direct-map key registered) nor a real filesystem mounted through
/// it. A managed direct-map target should always carry at least the autofs
/// trigger once the map is loaded; its total absence means autofs never loaded
/// the entry, so the mountpoint is a bare dir that `stat`s clean but serves
/// nothing. Pure over a supplied table so it is unit-testable without `/proc`.
fn target_absent_from_table(table: &[MountEntry], target: &str) -> bool {
    let t = target.trim_end_matches('/');
    !table
        .iter()
        .any(|e| e.mountpoint.trim_end_matches('/') == t)
}

/// Async convenience: read the live mount table and report whether `target` has
/// no entry. Offloaded to the blocking pool (reads `/proc/mounts`). Shared with
/// the native-mount convergence loop, which must catch the same bare-dir
/// false-positive `probe_health` misses.
pub async fn target_has_no_mount(target: &str) -> bool {
    let target = target.to_string();
    tokio::task::spawn_blocking(move || {
        target_absent_from_table(&mount_table().unwrap_or_default(), &target)
    })
    .await
    .unwrap_or(false)
}

// ── Source-liveness election + failback (the autofs-can't-do-it core) ──────────

/// The source currently mounted at `target`, read from the **live kernel mount
/// table** filtered to network filesystem types — not a `stat` on the autofs
/// trigger dir, which is a false positive (the trigger dir exists whether or not
/// anything is mounted through it). `None` means nothing network-shaped is
/// mounted there right now. Runtime-agnostic; offloaded to the blocking pool.
pub async fn current_source_for_target(target: &str) -> Option<String> {
    let target = target.to_string();
    let fstypes = net_fstypes();
    tokio::task::spawn_blocking(move || {
        let fstype_refs: Vec<&str> = fstypes.iter().map(String::as_str).collect();
        mount_table_of(&fstype_refs)
            .ok()?
            .into_iter()
            .find(|e| e.mountpoint == target)
            .map(|e| e.source)
    })
    .await
    .ok()
    .flatten()
}

/// Elect the first live source for one mount by TCP-probing its ordered sources
/// (real transport probe — NFS `:2049` / SMB `:445` — not a directory `stat`).
/// Deterministic: index 0 (primary) wins whenever live, so a recovered primary
/// always re-wins == fail-back. Returns [`Election::Empty`] if every source is
/// down. Each probe is offloaded so a black-holed host can't stall the runtime.
pub async fn elect_live_source(m: &ManagedMount, probe_timeout: Duration) -> Election {
    let sources = ordered_sources(&m.source, m.failover_sources.as_deref());
    let fstype = m.fstype.clone();
    let mut live = std::collections::HashSet::new();
    for src in &sources {
        let (s, f) = (src.clone(), fstype.clone());
        let ok = tokio::task::spawn_blocking(move || probe_source(&s, &f, probe_timeout))
            .await
            .unwrap_or(false);
        if ok {
            live.insert(src.clone());
        }
    }
    elect(&sources, |s| live.contains(s))
}

/// Is `target` currently held open by a process (a container reading it)?
/// Best-effort, unprivileged (`fuser -sm`): a busy mount must not be forcibly
/// remounted under the Safe policy — we log a pending failback instead. A probe
/// error is treated as **busy** (fail safe: never disrupt on uncertainty). A
/// probe that times out is treated as **not busy**: `fuser -m` stats the mount,
/// so a wedged mount that cannot answer must still reach the release path.
pub(crate) async fn is_busy(target: &str) -> bool {
    is_busy_with(
        target,
        |t| async move {
            Command::new("fuser")
                .args(["-sm", "--", &t])
                .kill_on_drop(true)
                .output()
                .await
                // `fuser -s` exits 0 when *something* holds the path, 1 when nothing does.
                .map(|out| out.status.success())
        },
        || tokio::time::sleep(TRIGGER_TIMEOUT),
    )
    .await
}

async fn is_busy_with<R, F, D, DF>(target: &str, run: R, deadline: D) -> bool
where
    R: Fn(String) -> F,
    F: std::future::Future<Output = std::io::Result<bool>>,
    D: Fn() -> DF,
    DF: std::future::Future<Output = ()>,
{
    tokio::select! {
        r = run(target.to_string()) => r.unwrap_or(true),
        () = deadline() => false,
    }
}

/// Reconcile one mount's live source: elect, compare to what's mounted, and
/// (when they differ) remount to the elected source per the `aggression` policy.
/// The map re-render is handled by the caller's `apply`; this drives the actual
/// mount swap. Returns the [`Transition`] taken so the caller logs it non-silently.
///
/// Safety (the Plex/Jellyfin guarantee): under [`RemountAggression::Safe`] a
/// **busy** mount is never force-swapped — the elected source is already in the
/// freshly-rendered map, so autofs serves it on the next idle re-trigger, and we
/// return the transition with a logged *pending* note. [`RemountAggression::Force`]
/// escalates a busy mount to a lazy force-unmount + retrigger.
pub async fn reconcile_source(
    m: &ManagedMount,
    aggression: RemountAggression,
    probe_timeout: Duration,
) -> (Transition, Vec<String>) {
    let mut errors = Vec::new();
    let sources = ordered_sources(&m.source, m.failover_sources.as_deref());
    let election = elect_live_source(m, probe_timeout).await;
    let current = current_source_for_target(&m.target).await;
    let trans = transition(&sources, current.as_deref(), &election);

    match &trans {
        // Nothing to do, or nothing we can do.
        Transition::Unchanged | Transition::EmptyTarget => {}
        // A swap is required (mount / degrade / failback). Choose safety.
        Transition::Mount { .. } | Transition::Degrade { .. } | Transition::FailBack { .. } => {
            let busy = is_busy(&m.target).await;
            match (aggression, busy) {
                // Not busy: a clean remount is safe under either policy.
                (_, false) => {
                    errors.extend(remount_to_elected(&m.target, probe_timeout).await);
                }
                // Busy + Safe (default): don't disrupt live I/O. The elected
                // source is already in the re-rendered map; autofs serves it on
                // next idle re-trigger. Caller logs the pending failback.
                (RemountAggression::Safe, true) => {}
                // Busy + Force (opt-in): escalate to lazy force-unmount.
                (RemountAggression::Force, true) => {
                    errors.extend(force_remount_to_elected(&m.target, probe_timeout).await);
                }
            }
        }
    }
    (trans, errors)
}

/// Release `target` through its recorded placement id; the root ledger, not the
/// path, authorizes it.
async fn release_by_id(target: &str) -> Vec<String> {
    match crate::mount_converge::recorded_placement_id(target) {
        Some(id) => {
            run_privileged(&PrivilegedOp::Release { ids: vec![id] })
                .await
                .errors
        }
        None => vec![format!(
            "release {target}: no placement id recorded; cannot release it"
        )],
    }
}

/// Clean remount of a not-busy target: lazy-detach the current mount so the next
/// access re-triggers autofs against the freshly-elected single-source map, then
/// re-access to bring it up now. Routes the unmount through the privileged seam.
async fn remount_to_elected(target: &str, _probe_timeout: Duration) -> Vec<String> {
    release_then_trigger(
        target,
        || release_by_id(target),
        || async { trigger(std::slice::from_ref(&target.to_string())).await },
    )
    .await
}

/// Force remount of a **busy** target (opt-in `Force` policy only). Same lazy
/// unmount + retrigger — `umount -lf` detaches the namespace entry even while
/// held, so open handles drain against the old server while new access hits the
/// elected source. Killing holders (`fuser -k`) is intentionally NOT done here;
/// it would be the only place to add it and stays out unless a future explicit
/// opt-in demands it. Loud by contract: the caller logs a `warn!`.
async fn force_remount_to_elected(target: &str, probe_timeout: Duration) -> Vec<String> {
    remount_to_elected(target, probe_timeout).await
}

/// Force an unconditional autofs restart (privileged) so it re-parses the master
/// map and the `/etc/auto.orca` direct map. Unlike [`apply`], this issues the
/// restart even when no file changed — the escalation for a `Missing` target whose
/// map is already correct on disk but was never loaded by the running autofs (map
/// written post-start, or the daemon restarted without it). Returns any errors
/// (empty on success).
pub async fn force_reload() -> Vec<String> {
    run_privileged(&PrivilegedOp::Reload {
        init: detect_init(),
    })
    .await
    .errors
}

/// Recover one confirmed-stale target and return `(recovered, errors)`.
///
/// Escalation ladder, driven by the *live* probe rather than by file contents —
/// this is the fix for the fleet-wide "map correct but nothing mounted" failure.
///
/// `allow_reload` gates the autofs-restart rungs on **source reachability**: an
/// autofs restart is global, so restarting it while a share's server is genuinely
/// down would churn and briefly disrupt every *other* healthy mount. Callers pass
/// `true` only when the elected source is reachable (server up, autofs just isn't
/// serving it — the freyr case) and `false` when the source is down (nothing a
/// reload can fix; just release the wedged handle).
///
/// 1. **`Missing`** + `allow_reload` (autofs isn't serving this direct map at all):
///    a `umount`/`stat` can't help — there's no trigger to fire. Force an autofs
///    **reload** so it parses `/etc/auto.orca`, then retrigger. This is the
///    freyr/thor/loki/baldur case after an NFS-server reboot: the map + master
///    include were correct, but autofs had never (re)loaded them, and the
///    idempotent file-diff apply refused to restart.
/// 2. **`Stale`/`Timeout`** (mounted but unresponsive): force-release the wedged
///    handle (`umount -lf`) + retrigger so autofs remounts / fails over.
/// 3. If still not live and `allow_reload`, escalate once more: reload + retrigger.
///
/// Each rung re-probes and returns as soon as the target is `MountHealth::Ok`.
pub async fn force_and_retrigger(
    target: &str,
    allow_reload: bool,
    health_timeout: Duration,
) -> (bool, Vec<String>) {
    let mut errors = Vec::new();
    let one = |t: &str| vec![t.to_string()];

    // Rung 1: autofs isn't providing the mount — reload, then trigger. Fires when
    // the path probes `Missing` OR when the mount table has no entry at all for the
    // target (bare-dir false positive: the direct-map trigger never loaded, so
    // `stat` succeeds and `probe` reports `Ok` even though nothing is mounted — the
    // frigg `/mnt/data` case). Gated on source reachability so a down server never
    // churns the global autofs restart. Success requires a REAL mount to appear
    // (mount-table entry), not merely a clean `stat` on the same bare dir.
    let missing_or_no_entry = matches!(probe(target, health_timeout).await, MountHealth::Missing)
        || target_has_no_mount(target).await;
    if allow_reload && missing_or_no_entry {
        errors.extend(force_reload().await);
        errors.extend(trigger(&one(target)).await);
        if matches!(probe(target, health_timeout).await, MountHealth::Ok)
            && !target_has_no_mount(target).await
        {
            return (true, errors);
        }
    }

    // Rung 2: lazy-unmount the (possibly wedged) handle + retrigger for failover.
    errors.extend(
        release_then_trigger(
            target,
            || release_by_id(target),
            || async { trigger(&one(target)).await },
        )
        .await,
    );
    if matches!(probe(target, health_timeout).await, MountHealth::Ok) {
        return (true, errors);
    }

    // Rung 3: last escalation — force a reload and retrigger once more. Still gated
    // on source reachability to avoid restarting autofs against a dead server.
    if allow_reload {
        errors.extend(force_reload().await);
        errors.extend(trigger(&one(target)).await);
    }
    let recovered = matches!(probe(target, health_timeout).await, MountHealth::Ok);
    (recovered, errors)
}

/// Self-heal the one failure mode autofs can't recover on its own: an
/// actively-held **stale** `hard` mount that never idles out. Probes each target
/// and immediately recovers any that are stale/hung/not-mounted. This is the
/// *manual* / on-demand path (the `storage.mount.update{action=recover}` tool) — it acts on the first
/// stale probe. The automated per-host loop instead confirms across several
/// ticks before acting (see [`crate::mount_converge`]).
pub async fn recover(targets: &[String], health_timeout: Duration) -> RecoverOutcome {
    let mut out = RecoverOutcome::default();

    for target in targets {
        // Classification is the shared storage decision table (`recovery_action`),
        // not a hand-rolled match — the same table nfs/smb use, so a new MountHealth
        // variant is classified once. Leave = mounted and usable (Ok, or
        // WriteDenied where only server-side perms are wrong — a remount can't fix
        // that); Indeterminate = never acted on (a probe glitch must not
        // force-release a healthy mount); Recover = force-release + retrigger.
        match probe(target, health_timeout).await.recovery_action() {
            RecoveryAction::Leave => out.healthy.push(target.clone()),
            RecoveryAction::Indeterminate => out.errors.push(format!(
                "probe {target}: indeterminate health, left untouched"
            )),
            RecoveryAction::Recover => {
                // On-demand `storage.mount.update{action=recover}`: user-initiated and one-shot, so allow
                // the reload escalation (no periodic-restart churn risk here).
                let (recovered, errs) = force_and_retrigger(target, true, health_timeout).await;
                out.errors.extend(errs);
                if recovered {
                    out.recovered.push(target.clone());
                } else {
                    out.still_stale.push(target.clone());
                }
            }
        }
    }

    out.no_stale_found = out.recovered.is_empty() && out.still_stale.is_empty();
    out
}

fn normalize_unmount_target(target: &str) -> Result<String, &'static str> {
    if target.chars().any(char::is_control) {
        return Err("control character in path");
    }
    let trimmed = target.trim_end_matches('/');
    let Some(rest) = trimmed.strip_prefix('/') else {
        return Err("not an absolute path");
    };
    if rest.split('/').any(|c| matches!(c, "" | "." | "..")) {
        return Err("path has an empty, `.` or `..` component");
    }
    Ok(trimmed.to_string())
}

/// autofs map keys end at the first unescaped whitespace; spaces, tabs and
/// backslashes in a target are backslash-escaped so the key survives intact.
fn escape_map_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        if c == ' ' || c == '\t' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn first_map_key(line: &str) -> String {
    let mut key = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => key.extend(chars.next()),
            ' ' | '\t' => break,
            c => key.push(c),
        }
    }
    key
}

fn declared_map_keys(map: &str) -> std::collections::HashSet<String> {
    map.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(first_map_key)
        .map(|k| k.trim_end_matches('/').to_string())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LedgerEntry {
    target: String,
    source: String,
}

type Ledger = std::collections::BTreeMap<String, LedgerEntry>;

fn is_canonical_placement_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id)
}

fn valid_ledger_source(source: &str) -> bool {
    !source.is_empty() && !source.chars().any(char::is_control)
}

/// Splits on unescaped tabs; an unescaped space or dangling backslash is malformed.
fn split_ledger_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => cur.push(chars.next()?),
            '\t' => fields.push(std::mem::take(&mut cur)),
            ' ' => return None,
            c => cur.push(c),
        }
    }
    fields.push(cur);
    Some(fields)
}

fn parse_ledger(contents: &str) -> Result<Ledger, String> {
    let corrupt = || format!("mount ledger {LEDGER_FILE} is corrupt");
    let mut out = Ledger::new();
    for line in contents.split('\n').filter(|l| !l.is_empty()) {
        let fields = split_ledger_fields(line).ok_or_else(corrupt)?;
        let [id, target, source] = <[String; 3]>::try_from(fields).map_err(|_| corrupt())?;
        let canonical = normalize_unmount_target(&target).is_ok_and(|k| k == target);
        if !is_canonical_placement_id(&id)
            || !canonical
            || !valid_ledger_source(&source)
            || out.values().any(|e| e.target == target)
            || out.insert(id, LedgerEntry { target, source }).is_some()
        {
            return Err(corrupt());
        }
    }
    Ok(out)
}

fn render_ledger(ledger: &Ledger) -> String {
    ledger
        .iter()
        .map(|(id, e)| {
            format!(
                "{id}\t{}\t{}\n",
                escape_map_key(&e.target),
                escape_map_key(&e.source)
            )
        })
        .collect()
}

fn read_ledger(path: &str) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("mount ledger {LEDGER_FILE} unreadable: {e}")),
    }
}

/// Exclusive flock on a sibling file (the ledger itself is replaced by rename, so
/// it cannot carry the lock); released when the returned handle drops.
async fn lock_ledger(path: &str) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock_path = format!("{path}.lock");
    tokio::task::spawn_blocking(move || {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| format!("open mount ledger lock {lock_path}: {e}"))?;
        rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|e| format!("lock mount ledger {lock_path}: {e}"))?;
        Ok(f)
    })
    .await
    .map_err(|e| format!("mount ledger lock task: {e}"))?
}

/// Root-side only. An add may refresh the source of an id already recorded for the
/// same target; rebinding an id to another target, or taking a target recorded
/// under another id, is an error. An add with a non-canonical id, unrepresentable
/// target or invalid source is also an error. On any error nothing is written.
async fn ledger_update(
    path: &str,
    add: &[(String, LedgerEntry)],
    remove: &[String],
) -> Result<(), String> {
    let mut adds = Vec::with_capacity(add.len());
    for (id, e) in add {
        let target = normalize_unmount_target(&e.target)
            .map_err(|why| format!("mount ledger: target {:?}: {why}", e.target))?;
        if !is_canonical_placement_id(id) || !valid_ledger_source(&e.source) {
            return Err(format!(
                "mount ledger: invalid placement id or source for {target}"
            ));
        }
        adds.push((
            id.clone(),
            LedgerEntry {
                target,
                source: e.source.clone(),
            },
        ));
    }
    if adds.is_empty() && remove.is_empty() {
        return Ok(());
    }
    let _lock = lock_ledger(path).await?;
    let mut ledger = parse_ledger(&read_ledger(path)?)?;
    let before = ledger.clone();
    for (id, e) in adds {
        if let Some(old) = ledger.get(&id)
            && old.target != e.target
        {
            return Err(format!(
                "mount ledger: placement {id} is already recorded for {}, refusing to rebind to {}",
                old.target, e.target
            ));
        }
        if let Some((other, _)) = ledger
            .iter()
            .find(|(other, old)| **other != id && old.target == e.target)
        {
            return Err(format!(
                "mount ledger: {} is already recorded under placement {other}",
                e.target
            ));
        }
        ledger.insert(id, e);
    }
    for r in remove {
        ledger.remove(r);
    }
    if ledger == before {
        return Ok(());
    }
    write_atomic(path, &render_ledger(&ledger), Some(0o600))
        .await
        .map_err(|e| format!("write mount ledger {path}: {e}"))
}

fn live_sources_match(live: &[String], recorded: &str) -> bool {
    live.iter()
        .all(|s| crate::mount_converge::same_source(s, recorded))
}

/// Root-side body of [`PrivilegedOp::Adopt`]. The check and the write share one
/// ledger lock so a concurrent mount cannot slip a conflicting row in between.
/// Returns the normalized target recorded.
async fn adopt_with<S, O>(
    path: &str,
    id: &str,
    target: &str,
    source: &str,
    sources: S,
    owned: O,
) -> Result<String, String>
where
    S: Fn(&str) -> Result<Vec<String>, String>,
    O: Fn(&str) -> Result<bool, String>,
{
    let target = normalize_unmount_target(target).map_err(|why| format!("target: {why}"))?;
    if !is_canonical_placement_id(id) {
        return Err("placement id is not a canonical lowercase UUID".into());
    }
    if !valid_ledger_source(source) {
        return Err("invalid source".into());
    }
    let live = sources(&target).map_err(|e| format!("cannot read mount table: {e}"))?;
    if live.is_empty() {
        return Err("no network mount present at the target".into());
    }
    if !live_sources_match(&live, source) {
        return Err("live source does not match the given source".into());
    }
    if !owned(&target)? {
        return Err("mount does not look orca-managed".into());
    }
    let _lock = lock_ledger(path).await?;
    let mut ledger = parse_ledger(&read_ledger(path)?)?;
    if ledger.contains_key(id) {
        return Err("placement id is already recorded".into());
    }
    if ledger.values().any(|e| e.target == target) {
        return Err("target is already recorded under another placement id".into());
    }
    ledger.insert(
        id.to_string(),
        LedgerEntry {
            target: target.clone(),
            source: source.to_string(),
        },
    );
    write_atomic(path, &render_ledger(&ledger), Some(0o600))
        .await
        .map_err(|e| format!("write mount ledger {path}: {e}"))?;
    Ok(target)
}

/// Root-side gate for [`PrivilegedOp::Release`]: an id is released only if the root
/// ledger holds it and a network layer is live at its recorded target. The live
/// source is deliberately not compared: after a failover the ledger names the
/// elected source while the wedged mount is still on the dead one. Returns the result (keyed by target) and the ids
/// to drop from the ledger.
async fn guard_release<G, S, U, F>(
    ids: &[String],
    read_ledger: G,
    sources: S,
    mut umount: U,
) -> (PrivilegedResult, Vec<String>)
where
    G: FnOnce() -> Result<String, String>,
    S: Fn(&str) -> Result<Vec<String>, String>,
    U: FnMut(String) -> F,
    F: std::future::Future<Output = Result<Unwound, String>>,
{
    let mut res = PrivilegedResult::default();
    let mut released = Vec::new();
    let ledger = match read_ledger().and_then(|c| parse_ledger(&c)) {
        Ok(l) => l,
        Err(e) => {
            res.errors
                .extend(ids.iter().map(|id| format!("release {id}: refused: {e}")));
            return (res, released);
        }
    };
    for id in ids {
        let Some(entry) = ledger.get(id) else {
            res.errors
                .push(format!("release {id}: refused: unknown placement id"));
            continue;
        };
        let checked = match sources(&entry.target) {
            Ok(live) if !live.is_empty() => live,
            Ok(_) => {
                res.unchanged.push(entry.target.clone());
                released.push(id.clone());
                continue;
            }
            Err(e) => {
                res.errors.push(format!(
                    "release {id}: refused: cannot read mount table: {e}"
                ));
                continue;
            }
        };
        match sources(&entry.target) {
            Ok(now) if now == checked => {}
            Ok(_) => {
                res.errors.push(format!(
                    "release {id}: refused: mount at target changed since authorization"
                ));
                continue;
            }
            Err(e) => {
                res.errors.push(format!(
                    "release {id}: refused: cannot read mount table: {e}"
                ));
                continue;
            }
        }
        match umount(entry.target.clone()).await {
            Ok(Unwound::Detached) => {
                res.changed.push(entry.target.clone());
                released.push(id.clone());
            }
            Ok(Unwound::NothingToDetach) => res.unchanged.push(entry.target.clone()),
            Err(e) => res.errors.push(format!("release {id}: {e}")),
        }
    }
    (res, released)
}

const MAX_STACKED_UNMOUNTS: usize = 16;

/// `umount -lf <target>` repeated until no network layer (nfs/cifs) remains at
/// the path. The autofs layer beneath, and any non-network layer, is never
/// detached. Runs root-side inside the helper. A non-zero exit (e.g. "not
/// mounted") surfaces as an error the caller collects but does not treat as fatal.
async fn force_unmount(target: &str) -> Result<Unwound, String> {
    force_unmount_with(target, stacked_fstypes, run_umount_lf).await
}

async fn force_unmount_with<L, U, F>(target: &str, layers: L, umount: U) -> Result<Unwound, String>
where
    L: Fn(&str) -> Result<Vec<String>, String>,
    U: Fn(String) -> F,
    F: std::future::Future<Output = Result<(), String>>,
{
    unwind_stacked(|| layers(target), || umount(target.to_string())).await
}

async fn run_umount_lf(target: String) -> Result<(), String> {
    let out = utils::privileged::host_command("umount")
        .map_err(|e| e.to_string())?
        .args(["-lf", "--no-canonicalize", "--", &target])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Filesystem types stacked at `target`, oldest first (topmost last). `/proc/mounts`
/// lists the namespace mount list in mount order, so a later row at the same path
/// is the one mounted over the earlier rows.
fn stacked_fstypes(target: &str) -> Result<Vec<String>, String> {
    stacked_from(plugin_toolkit::storage::mount_table::mount_table(), target)
}

/// Sources of the network layers stacked at `target`.
fn network_sources(target: &str) -> Result<Vec<String>, String> {
    plugin_toolkit::storage::mount_table::mount_table()
        .map(|t| network_sources_in(t, target))
        .map_err(|e| format!("cannot read mount table: {e}"))
}

/// Root-readable proof orca made the mount: the target is keyed in the direct
/// map, or a cifs/smb3 layer carries orca's own service credentials. Reads
/// /proc/mounts only, never stats the target.
fn orca_owned(target: &str) -> Result<bool, String> {
    let keys = declared_map_keys(&read_map_file()?);
    let table = plugin_toolkit::storage::mount_table::mount_table()
        .map_err(|e| format!("cannot read mount table: {e}"))?;
    Ok(orca_owned_in(&keys, &table, target))
}

fn orca_owned_in(
    keys: &std::collections::HashSet<String>,
    table: &[MountEntry],
    target: &str,
) -> bool {
    if keys.contains(target.trim_end_matches('/')) {
        return true;
    }
    let t = target.trim_end_matches('/');
    let mut layers = table
        .iter()
        .filter(|m| m.mountpoint.trim_end_matches('/') == t && is_network_fstype(&m.fstype))
        .peekable();
    layers.peek().is_some()
        && layers.all(|m| {
            matches!(m.fstype.as_str(), "cifs" | "smb3")
                && m.options.iter().any(|o| o == "username=orca")
        })
}

fn network_sources_in(table: Vec<MountEntry>, target: &str) -> Vec<String> {
    let target = target.trim_end_matches('/');
    table
        .into_iter()
        .filter(|m| m.mountpoint.trim_end_matches('/') == target && is_network_fstype(&m.fstype))
        .map(|m| m.source)
        .collect()
}

/// Ledger row for `target` built from the source the kernel reports for the
/// topmost network layer, never from caller-supplied text.
fn observed_ledger_entry(
    table: std::io::Result<Vec<MountEntry>>,
    target: &str,
) -> Result<LedgerEntry, String> {
    let table = table.map_err(|e| {
        format!("mount ledger not updated for {target}: cannot read mount table: {e}")
    })?;
    let source = network_sources_in(table, target).pop().ok_or_else(|| {
        format!("mount ledger not updated for {target}: no network mount observed")
    })?;
    Ok(LedgerEntry {
        target: target.to_string(),
        source,
    })
}

fn stacked_from(
    table: std::io::Result<Vec<MountEntry>>,
    target: &str,
) -> Result<Vec<String>, String> {
    table
        .map(|t| fstypes_at(&t, target))
        .map_err(|e| format!("cannot read mount table: {e}"))
}

fn fstypes_at(table: &[MountEntry], target: &str) -> Vec<String> {
    let target = target.trim_end_matches('/');
    if target.is_empty() {
        return Vec::new();
    }
    table
        .iter()
        .filter(|m| m.mountpoint.trim_end_matches('/') == target)
        .map(|m| m.fstype.clone())
        .collect()
}

fn is_network_fstype(fstype: &str) -> bool {
    matches!(fstype, "nfs" | "nfs4" | "cifs" | "smb3")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unwound {
    Detached,
    NothingToDetach,
}

async fn unwind_stacked<L, U, F>(mut layers: L, mut umount: U) -> Result<Unwound, String>
where
    L: FnMut() -> Result<Vec<String>, String>,
    U: FnMut() -> F,
    F: std::future::Future<Output = Result<(), String>>,
{
    let mut detached = false;
    let outcome = |detached| {
        if detached {
            Unwound::Detached
        } else {
            Unwound::NothingToDetach
        }
    };
    for attempt in 0..MAX_STACKED_UNMOUNTS {
        let stack = layers()?;
        match stack.last() {
            None if attempt == 0 => return Err("nothing mounted at target".to_string()),
            Some(top) if is_network_fstype(top) => {
                if let Err(e) = umount().await {
                    let after = layers()?;
                    if !after.iter().any(|f| is_network_fstype(f)) {
                        return Ok(Unwound::Detached);
                    }
                    return match after.last() {
                        Some(top) if !is_network_fstype(top) => Err(format!(
                            "network mount is covered by a `{top}` layer; refusing to detach it"
                        )),
                        _ => Err(e),
                    };
                }
                detached = true;
            }
            Some(top) if stack.iter().any(|f| is_network_fstype(f)) => {
                return Err(format!(
                    "network mount is covered by a `{top}` layer; refusing to detach it"
                ));
            }
            _ => return Ok(outcome(detached)),
        }
    }
    if layers()?.last().is_some_and(|t| is_network_fstype(t)) {
        return Err(format!(
            "network mount still present after {MAX_STACKED_UNMOUNTS} unmounts"
        ));
    }
    Ok(outcome(detached))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SetShareMode privileged-op guards (the security core of the perm-drift
    //    repair: an additive perms overlay that must never escalate or escape) ──

    #[test]
    fn share_mode_guard_rejects_escalating_and_lockout_modes() {
        // Allowed: additive, owner keeps rwx, no special bits.
        assert!(is_allowed_share_mode(0o777));
        assert!(is_allowed_share_mode(0o775));
        assert!(is_allowed_share_mode(0o755));
        assert!(is_allowed_share_mode(0o700));
        // Refused: setuid/setgid/sticky (privilege escalation).
        assert!(!is_allowed_share_mode(0o4777));
        assert!(!is_allowed_share_mode(0o2777));
        assert!(!is_allowed_share_mode(0o1777));
        // Refused: owner not fully able (a repair must never lock the owner out).
        assert!(!is_allowed_share_mode(0o077));
        assert!(!is_allowed_share_mode(0o000));
        assert!(!is_allowed_share_mode(0o477));
    }

    #[cfg(unix)]
    #[test]
    fn share_path_guard_accepts_subtree_rejects_root_traversal_and_escape() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let root_str = root.path().to_str().unwrap();
        // A real share subtree under the root is allowed.
        let share = root.path().join("data");
        std::fs::create_dir_all(share.join("photos")).unwrap();
        assert!(path_under_share_root(share.to_str().unwrap(), root_str));
        assert!(path_under_share_root(
            share.join("photos").to_str().unwrap(),
            root_str
        ));
        // The root itself (no <share> component) is refused.
        assert!(!path_under_share_root(root_str, root_str));
        // Traversal that escapes the root is refused (canonicalize collapses `..`).
        let escape = format!("{}/data/../../etc", root_str);
        assert!(!path_under_share_root(&escape, root_str));
        // A symlink that points outside the root is refused (canonicalize follows
        // it): `<root>/data/evil -> /etc`.
        let outside = tempfile::tempdir().unwrap();
        let evil = share.join("evil");
        symlink(outside.path(), &evil).unwrap();
        assert!(!path_under_share_root(evil.to_str().unwrap(), root_str));
        // A non-existent path is refused (can't canonicalize).
        assert!(!path_under_share_root(
            share.join("nope").to_str().unwrap(),
            root_str
        ));
    }

    #[cfg(unix)]
    #[test]
    fn set_mode_tree_recurses_but_never_follows_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("f");
        std::fs::write(&file, b"x").unwrap();
        // A symlink to an outside file must NOT be chmod'd/followed.
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret");
        std::fs::write(&outside_file, b"y").unwrap();
        std::fs::set_permissions(&outside_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&outside_file, sub.join("link")).unwrap();

        let n = set_mode_tree(dir.path().to_str().unwrap(), 0o777, true).unwrap();
        assert!(n >= 3, "root + sub + file all changed");
        for p in [dir.path(), sub.as_path(), file.as_path()] {
            let m = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(m, 0o777, "{p:?} should be 0o777");
        }
        // The symlink's target outside the tree is untouched.
        let outside_mode = std::fs::metadata(&outside_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(outside_mode, 0o600, "symlink target must not be chmod'd");
    }

    #[test]
    fn set_mode_tree_non_recursive_touches_only_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let n = set_mode_tree(dir.path().to_str().unwrap(), 0o755, false).unwrap();
        assert_eq!(n, 1, "only the root entry changed");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600,
            "child untouched when non-recursive"
        );
    }

    fn mount(name: &str, source: &str, failover: Option<&str>) -> ManagedMount {
        ManagedMount {
            name: name.into(),
            backend: "nfs".into(),
            kind: "network_share".into(),
            source: source.into(),
            failover_sources: failover.map(str::to_string),
            target: format!("/mnt/{name}"),
            fstype: "nfs4".into(),
            options: Some("_netdev,nofail,x-systemd.automount,vers=4.2,hard,nconnect=4".into()),
            credential: None,
            remount_policy: None,
            placement_id: None,
            routes: Default::default(),
            enabled: true,
        }
    }

    fn entry(mountpoint: &str, fstype: &str, source: &str) -> MountEntry {
        MountEntry {
            source: source.into(),
            mountpoint: mountpoint.into(),
            fstype: fstype.into(),
            options: Vec::new(),
        }
    }

    #[test]
    fn fstypes_at_matches_exact_mountpoint_only() {
        let table = vec![
            entry("/mnt/data", "autofs", "a"),
            entry("/mnt/data/sub", "nfs4", "b"),
            entry("/mnt/database", "nfs4", "c"),
            entry("/mnt/data", "cifs", "d"),
        ];
        assert_eq!(fstypes_at(&table, "/mnt/data"), vec!["autofs", "cifs"]);
    }

    #[test]
    fn fstypes_at_preserves_table_order() {
        let fwd = vec![entry("/m", "autofs", "a"), entry("/m", "cifs", "b")];
        let rev = vec![entry("/m", "cifs", "b"), entry("/m", "autofs", "a")];
        assert_eq!(fstypes_at(&fwd, "/m").last().unwrap(), "cifs");
        assert_eq!(fstypes_at(&rev, "/m"), vec!["cifs", "autofs"]);
    }

    #[test]
    fn fstypes_at_ignores_trailing_slashes() {
        let table = vec![entry("/mnt/data/", "cifs", "a")];
        assert_eq!(fstypes_at(&table, "/mnt/data"), vec!["cifs"]);
        assert_eq!(fstypes_at(&table, "/mnt/data/"), vec!["cifs"]);
        let table = vec![entry("/mnt/data", "cifs", "a")];
        assert_eq!(fstypes_at(&table, "/mnt/data/"), vec!["cifs"]);
    }

    #[test]
    fn fstypes_at_empty_when_absent_or_empty_target() {
        let table = vec![entry("/mnt/data", "cifs", "a"), entry("/", "nfs4", "r")];
        assert!(fstypes_at(&table, "/mnt/other").is_empty());
        for t in ["", "/", "//"] {
            assert!(fstypes_at(&table, t).is_empty(), "{t:?}");
        }
        assert_eq!(fstypes_at(&table, "/mnt/data"), vec!["cifs".to_string()]);
    }

    #[test]
    fn stacked_from_propagates_read_error() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "proc denied");
        let e = stacked_from(Err(err), "/mnt/data").unwrap_err();
        assert!(e.contains("cannot read mount table"), "{e}");
        assert!(e.contains("proc denied"), "{e}");
    }

    #[test]
    fn stacked_from_returns_stacked_pair() {
        let table = vec![
            entry("/mnt/data", "autofs", "a"),
            entry("/mnt/data", "cifs", "b"),
        ];
        assert_eq!(
            stacked_from(Ok(table), "/mnt/data").unwrap(),
            vec!["autofs", "cifs"]
        );
    }

    #[test]
    fn stacked_from_empty_when_no_matching_entry() {
        let table = vec![entry("/mnt/other", "cifs", "a")];
        assert!(stacked_from(Ok(table), "/mnt/data").unwrap().is_empty());
    }

    #[test]
    fn stacked_from_root_target_is_empty() {
        for t in ["", "/"] {
            let table = vec![entry("/", "ext4", "r")];
            assert!(stacked_from(Ok(table), t).unwrap().is_empty(), "{t:?}");
        }
    }

    #[test]
    fn fstypes_at_matches_mountpoint_with_space() {
        let table = vec![entry("/mnt/my share", "cifs", "a")];
        assert_eq!(fstypes_at(&table, "/mnt/my share"), vec!["cifs"]);
        assert!(fstypes_at(&table, "/mnt/my").is_empty());
    }

    #[test]
    fn fstypes_at_over_parsed_proc_mounts() {
        let raw = "systemd-1 /mnt/data autofs rw,relatime 0 0\n\
                   //srv/share /mnt/data cifs rw,vers=3.0 0 0\n\
                   //srv/other /mnt/my\\040share cifs rw 0 0\n\
                   /dev/sda1 /mnt/database ext4 rw 0 0\n";
        let table = plugin_toolkit::storage::mount_table::parse_linux_proc_mounts(raw);
        assert_eq!(fstypes_at(&table, "/mnt/data"), vec!["autofs", "cifs"]);
        assert_eq!(fstypes_at(&table, "/mnt/my share"), vec!["cifs"]);
    }

    #[test]
    fn target_absent_when_no_mount_table_entry() {
        // The frigg /mnt/data regression: the mountpoint is a bare dir (stat OK,
        // probe_health => Ok) but nothing is mounted there — no autofs trigger and
        // no nfs. It must be classified absent so self-heal reloads the map.
        let table = vec![
            entry("/mnt/backups", "autofs", "/etc/auto.orca"),
            entry("/mnt/backups", "nfs4", "10.0.0.10:/mnt/user/backups"),
        ];
        assert!(
            target_absent_from_table(&table, "/mnt/data"),
            "bare-dir mountpoint with no table entry must read as absent"
        );
    }

    #[test]
    fn target_present_with_autofs_trigger_only() {
        // A loaded-but-idle direct-map trigger (no real mount yet) is NOT absent —
        // access will mount it. Only total absence signals a failed map load.
        let table = vec![entry("/mnt/data", "autofs", "/etc/auto.orca")];
        assert!(!target_absent_from_table(&table, "/mnt/data"));
    }

    #[test]
    fn target_present_with_real_mount() {
        let table = vec![entry("/mnt/data", "nfs4", "10.0.0.10:/mnt/user/data")];
        assert!(!target_absent_from_table(&table, "/mnt/data"));
        // Trailing-slash normalization on both sides.
        assert!(!target_absent_from_table(&table, "/mnt/data/"));
    }

    #[test]
    fn map_line_lists_ordered_sources_and_strips_fstab_only_opts() {
        let m = mount(
            "data",
            "primary:/srv/pool/data",
            Some("secondary:/srv/pool/data"),
        );
        assert_eq!(
            map_line(&m),
            "/mnt/data  -fstype=nfs4,vers=4.2,hard,nconnect=4  \
             primary:/srv/pool/data secondary:/srv/pool/data"
        );
    }

    fn elected(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(t, s)| (t.to_string(), s.to_string()))
            .collect()
    }

    #[test]
    fn map_line_for_pins_single_elected_source() {
        let m = mount(
            "data",
            "primary:/srv/pool/data",
            Some("secondary:/srv/pool/data"),
        );
        // even with a failover declared, the elected line carries ONE source
        assert_eq!(
            map_line_for(&m, "secondary:/srv/pool/data"),
            "/mnt/data  -fstype=nfs4,vers=4.2,hard,nconnect=4  secondary:/srv/pool/data"
        );
    }

    #[test]
    fn render_map_elected_writes_only_elected_source() {
        let m = mount("data", "primary:/d", Some("secondary:/d"));
        let map = render_map_elected(&[m], &elected(&[("/mnt/data", "primary:/d")]));
        let body: Vec<&str> = map.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(body.len(), 1);
        assert!(body[0].ends_with("primary:/d"));
        assert!(!body[0].contains("secondary"));
    }

    #[test]
    fn render_map_elected_omits_mounts_with_no_live_source() {
        // `up` has an election, `down` does not → only `up` is rendered
        let up = mount("up", "primary:/u", None);
        let down = mount("down", "primary:/x", None);
        let map = render_map_elected(&[up, down], &elected(&[("/mnt/up", "primary:/u")]));
        let body: Vec<&str> = map.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(body.len(), 1);
        assert!(body[0].starts_with("/mnt/up"));
    }

    #[test]
    fn render_map_elected_empty_is_header_only() {
        let m = mount("x", "primary:/x", None);
        // no election for the mount → nothing rendered
        assert_eq!(render_map_elected(&[m], &elected(&[])), HEADER);
    }

    #[test]
    fn merge_master_evicts_duplicate_direct_map_registration() {
        // A leaked/duplicate `/-  /etc/auto.orca` registration (e.g. copied from
        // an auto.master.d drop-in) must NOT survive — we re-add it in the block.
        let existing = format!("/net\t-hosts\n/-  {MAP_FILE} --timeout=60\n");
        let out = merge_master(&existing, &[]);
        // exactly one registration of our map, and it's inside the orca block
        assert_eq!(out.matches(MAP_FILE).count(), 1);
        assert!(out.contains("/net\t-hosts"));
        assert!(out.contains(BLOCK_BEGIN));
    }

    #[test]
    fn registers_our_map_matches_only_map_registrations() {
        assert!(registers_our_map(&format!("/-  {MAP_FILE} --timeout=60")));
        assert!(registers_our_map(&format!("/-\t{MAP_FILE}")));
        assert!(!registers_our_map("/net\t-hosts"));
        assert!(!registers_our_map(&format!("# /-  {MAP_FILE}")));
        assert!(!registers_our_map(""));
    }

    #[test]
    fn render_map_sorts_enabled_network_shares_and_skips_others() {
        let mut disabled = mount("off", "primary:/o", None);
        disabled.enabled = false;
        let mut disk = mount("disk", "primary:/d", None);
        disk.kind = "disk_storage".into();
        let mounts = vec![
            mount("zeta", "primary:/z", None),
            mount("alpha", "primary:/a", Some("secondary:/a")),
            disabled,
            disk,
        ];
        let rendered = render_map(&mounts);
        let body: Vec<&str> = rendered.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(body.len(), 2);
        assert!(body[0].starts_with("/mnt/alpha"));
        assert!(body[1].starts_with("/mnt/zeta"));
    }

    #[test]
    fn targets_are_arbitrary_direct_map_keys() {
        let mut a = mount("a", "primary:/exports/a", None);
        a.target = "/mnt/data".into();
        let mut b = mount("b", "primary:/exports/b", None);
        b.target = "/mnt/pool/data".into();
        let mut c = mount("c", "primary:/exports/c", None);
        c.target = "/nfs/mnt/data".into();
        let rendered = render_map(&[a, b, c]);
        let keys: Vec<&str> = rendered
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split("  ").next().unwrap())
            .collect();
        assert_eq!(keys, ["/mnt/data", "/mnt/pool/data", "/nfs/mnt/data"]);
    }

    #[test]
    fn ancestor_matching_respects_component_boundaries() {
        assert!(is_ancestor_or_equal("/mnt/pool", "/mnt/pool/data"));
        assert!(is_ancestor_or_equal("/mnt/pool", "/mnt/pool"));
        assert!(is_ancestor_or_equal("/mnt/pool/", "/mnt/pool/data"));
        assert!(!is_ancestor_or_equal("/mnt/poolX", "/mnt/pool/data"));
        assert!(!is_ancestor_or_equal("/mnt/pool", "/mnt/poolside"));
    }

    #[test]
    fn merge_master_takes_over_shadowing_entry_and_preserves_foreign() {
        // host-e's real shape: an indirect /mnt/pool map + unrelated foreign
        // entries. We manage /mnt/pool/data, so /mnt/pool must be evicted while
        // /misc and /net survive untouched.
        let existing = "\
/misc\t/etc/autofs/auto.misc
/net\t-hosts
/mnt/pool  /etc/autofs/auto.pool  --timeout=60 --ghost
";
        let out = merge_master(existing, &["/mnt/pool/data".to_string()]);
        assert!(out.contains("/misc\t/etc/autofs/auto.misc"));
        assert!(out.contains("/net\t-hosts"));
        assert!(
            !out.contains("/etc/autofs/auto.pool"),
            "shadowing entry evicted"
        );
        assert!(out.contains(BLOCK_BEGIN));
        assert!(out.contains(&master_line()));
        assert!(out.contains(BLOCK_END));
    }

    #[test]
    fn merge_master_is_idempotent() {
        let targets = vec!["/mnt/pool/data".to_string()];
        let once = merge_master("/net\t-hosts\n", &targets);
        let twice = merge_master(&once, &targets);
        assert_eq!(once, twice);
    }

    #[test]
    fn merge_master_replaces_old_block_not_duplicates() {
        let targets = vec!["/mnt/data".to_string()];
        let first = merge_master("", &targets);
        let again = merge_master(&first, &targets);
        assert_eq!(first, again);
        assert_eq!(again.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn retire_master_strips_orca_block_and_preserves_foreign() {
        let targets = vec!["/mnt/data".to_string()];
        // A master orca took over (foreign /net line + orca block/direct map).
        let owned = merge_master("/net\t-hosts\n", &targets);
        assert!(owned.contains(MAP_FILE) && owned.contains(BLOCK_BEGIN));
        let retired = retire_master(&owned);
        assert!(!retired.contains(BLOCK_BEGIN), "orca block gone");
        assert!(!retired.contains(MAP_FILE), "direct map gone");
        assert!(retired.contains("/net"), "foreign line preserved");
    }

    #[test]
    fn retire_master_drops_bare_direct_map_registration_outside_block() {
        // A hand-added / drop-in-leaked direct map with no orca markers.
        let retired = retire_master("/net\t-hosts\n/-  /etc/auto.orca --timeout=0\n");
        assert!(!retired.contains(MAP_FILE), "bare direct map dropped");
        assert!(retired.contains("/net"));
    }

    #[test]
    fn retire_master_is_noop_when_no_orca_ownership() {
        let foreign = "/net\t-hosts\n/misc\t/etc/auto.misc\n";
        assert_eq!(retire_master(foreign), foreign);
    }

    #[test]
    fn retire_master_then_merge_round_trips() {
        // Retiring what merge_master produced, then leaving it retired, is stable.
        let targets = vec!["/mnt/data".to_string()];
        let owned = merge_master("", &targets);
        let retired = retire_master(&owned);
        assert_eq!(retire_master(&retired), retired, "retire is idempotent");
    }

    #[test]
    fn allowlist_rejects_arbitrary_paths() {
        assert!(is_allowed_write(MAP_FILE));
        assert!(is_allowed_write("/etc/auto.master"));
        assert!(is_allowed_write("/etc/autofs/auto.master"));
        assert!(!is_allowed_write("/etc/passwd"));
        assert!(!is_allowed_write("/etc/auto.master.d/../../shadow"));
    }

    // NOTE: the NFS safety floor (soft/softreval/timeo/retrans) moved into the nfs
    // plugin's rendering — core is fstype-agnostic and no longer injects it. Those
    // tests now live in `argyle-labs/nfs`.

    // ── autofs_options ────────────────────────────────────────────────────

    // These exercise the fstab-only strip + `-fstype=` framing that core applies
    // to whatever the backend's `render_options` produced. An unregistered backend
    // (the test path) renders its raw option string verbatim via `OptionSet::Raw`,
    // so `render_backend_options(_, fstype, opts)` then `strip_fstab_only` is the
    // exact prior `autofs_options(fstype, opts)` behavior — asserted byte-for-byte.
    fn autofs_options_raw(fstype: &str, options: Option<&str>) -> String {
        strip_fstab_only(fstype, &render_backend_options("nfs", fstype, options))
    }

    #[test]
    fn autofs_options_bare_fstype_when_no_options() {
        assert_eq!(autofs_options_raw("nfs4", None), "-fstype=nfs4");
    }

    #[test]
    fn autofs_options_empty_string_options_yields_bare_fstype() {
        assert_eq!(autofs_options_raw("nfs4", Some("")), "-fstype=nfs4");
    }

    #[test]
    fn autofs_options_keeps_real_opts_and_drops_fstab_only() {
        assert_eq!(
            autofs_options_raw(
                "nfs4",
                Some("_netdev,nofail,x-systemd.automount,vers=4.2,hard,nconnect=4,noauto,auto")
            ),
            "-fstype=nfs4,vers=4.2,hard,nconnect=4"
        );
    }

    #[test]
    fn autofs_options_trims_whitespace_around_opts() {
        assert_eq!(
            autofs_options_raw("cifs", Some(" ro , vers=3.0 ")),
            "-fstype=cifs,ro,vers=3.0"
        );
    }

    #[test]
    fn autofs_options_drops_empty_segments_from_double_commas() {
        assert_eq!(
            autofs_options_raw("nfs", Some("ro,,rw")),
            "-fstype=nfs,ro,rw"
        );
    }

    #[test]
    fn autofs_options_drops_comment_option() {
        assert_eq!(
            autofs_options_raw("nfs", Some("comment=x-gvfs-show,ro")),
            "-fstype=nfs,ro"
        );
    }

    // ── is_fstab_only ─────────────────────────────────────────────────────

    #[test]
    fn is_fstab_only_recognizes_systemd_and_fstab_opts() {
        assert!(is_fstab_only("_netdev"));
        assert!(is_fstab_only("nofail"));
        assert!(is_fstab_only("auto"));
        assert!(is_fstab_only("noauto"));
        assert!(is_fstab_only("comment=foo"));
        assert!(is_fstab_only("x-systemd.automount"));
        assert!(is_fstab_only("x-systemd.idle-timeout=60"));
    }

    #[test]
    fn is_fstab_only_passes_real_mount_opts() {
        assert!(!is_fstab_only("vers=4.2"));
        assert!(!is_fstab_only("hard"));
        assert!(!is_fstab_only("nconnect=4"));
        assert!(!is_fstab_only("ro"));
    }

    // ── master_line ───────────────────────────────────────────────────────

    #[test]
    fn master_line_points_at_map_file_with_timeout() {
        assert_eq!(
            master_line(),
            format!("/-  {MAP_FILE} --timeout={TIMEOUT_SECS}")
        );
    }

    #[test]
    fn master_line_renders_persistent_timeout() {
        // Persistent (never idle-expire) is the whole point: `--timeout=0` is
        // what stops Docker binds from racing an idle-expired mount. Assert the
        // literal so a future bump of TIMEOUT_SECS can't silently reintroduce
        // idle expiry.
        assert_eq!(TIMEOUT_SECS, 0);
        assert_eq!(master_line(), format!("/-  {MAP_FILE} --timeout=0"));
    }

    fn mount_at(target: &str) -> ManagedMount {
        ManagedMount {
            target: target.into(),
            ..mount("x", "h:/x", None)
        }
    }

    const SPECIAL_TARGETS: [&str; 3] = ["/mnt/my dir", "/mnt/back\\slash", "/mnt/tab\tdir"];

    #[test]
    fn rendered_map_keys_round_trip_special_characters() {
        for target in SPECIAL_TARGETS {
            let m = mount_at(target);
            let all = render_map(std::slice::from_ref(&m));
            assert_eq!(declared_map_keys(&all), [target.to_string()].into());
            let elected = [(target.to_string(), "h:/x".to_string())].into();
            let one = render_map_elected(std::slice::from_ref(&m), &elected);
            assert_eq!(declared_map_keys(&one), [target.to_string()].into());
        }
    }

    #[test]
    fn plain_target_rendering_is_unescaped() {
        let m = mount("data", "h:/d", None);
        assert!(map_line(&m).starts_with("/mnt/data  -fstype="));
    }

    #[test]
    fn first_map_key_splits_only_on_space_and_tab() {
        assert_eq!(
            first_map_key("/mnt/a\x0bb\x0cc\u{a0}d  opts"),
            "/mnt/a\x0bb\x0cc\u{a0}d"
        );
        assert_eq!(first_map_key("/mnt/a\tb"), "/mnt/a");
        assert_eq!(first_map_key("/mnt/a\\ b\\\tc  x"), "/mnt/a b\tc");
    }

    #[test]
    fn master_mountpoint_round_trips_escaped_keys() {
        for target in SPECIAL_TARGETS {
            let line = format!("{}  /etc/auto.x", escape_map_key(target));
            assert_eq!(master_mountpoint(&line).as_deref(), Some(target));
        }
    }

    #[test]
    fn merge_master_evicts_shadowing_entry_with_escaped_mountpoint() {
        let existing = "/mnt/my\\ dir  /etc/auto.pool\n/net\t-hosts\n";
        let out = merge_master(existing, &["/mnt/my dir/data".to_string()]);
        assert!(!out.contains("auto.pool"));
        assert!(out.contains("/net"));
    }

    #[tokio::test]
    async fn apply_refuses_targets_a_map_cannot_represent() {
        for target in [
            "/mnt/a\nb",
            "/mnt/a\rb",
            "/mnt/a\0b",
            "/mnt/a\x0bb",
            "/mnt/a\x0cb",
            "/mnt/a\u{85}b",
            "/mnt/a\u{a0}b",
            "/mnt/a\u{2028}b",
            "/mnt/a\"b",
            "/mnt/a'b",
        ] {
            let m = mount_at(target);
            let out = apply(std::slice::from_ref(&m)).await;
            assert!(out.changed.is_empty() && !out.reloaded);
            assert_eq!(out.errors.len(), 1);
            let out = apply_elected(std::slice::from_ref(&m), &Default::default()).await;
            assert_eq!(out.errors.len(), 1);
        }
    }

    // ── master_mountpoint ─────────────────────────────────────────────────

    #[test]
    fn master_mountpoint_extracts_first_field() {
        assert_eq!(
            master_mountpoint("/misc\t/etc/auto.misc").as_deref(),
            Some("/misc")
        );
        assert_eq!(
            master_mountpoint("  /mnt/pool  /etc/auto.pool --ghost").as_deref(),
            Some("/mnt/pool")
        );
    }

    #[test]
    fn master_mountpoint_none_for_blank_and_comment() {
        assert_eq!(master_mountpoint(""), None);
        assert_eq!(master_mountpoint("   "), None);
        assert_eq!(master_mountpoint("# a comment"), None);
        assert_eq!(master_mountpoint("   # indented comment"), None);
    }

    // ── render_map header + empty ─────────────────────────────────────────

    #[test]
    fn render_map_empty_input_is_header_only() {
        assert_eq!(render_map(&[]), HEADER);
    }

    #[test]
    fn render_map_all_disabled_is_header_only() {
        let mut m = mount("x", "primary:/x", None);
        m.enabled = false;
        assert_eq!(render_map(&[m]), HEADER);
    }

    #[test]
    fn render_map_starts_with_header_and_ends_with_newline() {
        let rendered = render_map(&[mount("a", "primary:/a", None)]);
        assert!(rendered.starts_with(HEADER));
        assert!(rendered.ends_with('\n'));
    }

    // ── map_line with multiline failovers ─────────────────────────────────

    #[test]
    fn map_line_joins_multiline_failover_sources() {
        let m = mount(
            "data",
            "primary:/srv/data",
            Some("secondary:/srv/data\ntertiary:/srv/data\n"),
        );
        assert_eq!(
            map_line(&m),
            "/mnt/data  -fstype=nfs4,vers=4.2,hard,nconnect=4  \
             primary:/srv/data secondary:/srv/data tertiary:/srv/data"
        );
    }

    #[test]
    fn map_line_single_source_when_no_failover() {
        let mut m = mount("solo", "primary:/s", None);
        m.options = None;
        // Core is fstype-agnostic: with no registered backend it renders the raw
        // option string verbatim (here empty), so the map carries only `-fstype`.
        // The NFS safety floor now lives in the nfs plugin's rendering.
        assert_eq!(map_line(&m), "/mnt/solo  -fstype=nfs4  primary:/s");
    }

    // ── merge_master edge cases ───────────────────────────────────────────

    #[test]
    fn merge_master_empty_input_yields_only_block() {
        let out = merge_master("", &[]);
        assert!(out.starts_with(BLOCK_BEGIN));
        assert!(out.contains(&master_line()));
        assert!(out.trim_end().ends_with(BLOCK_END));
        assert_eq!(out.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn merge_master_trims_trailing_blank_foreign_lines() {
        let out = merge_master("/net\t-hosts\n\n\n", &[]);
        // No blank line should sit between the foreign entry and the block.
        assert!(out.contains(&format!("/net\t-hosts\n{BLOCK_BEGIN}")));
    }

    #[test]
    fn merge_master_no_managed_targets_keeps_all_foreign() {
        let existing = "/mnt/pool  /etc/auto.pool\n/misc  /etc/auto.misc\n";
        let out = merge_master(existing, &[]);
        assert!(out.contains("/mnt/pool  /etc/auto.pool"));
        assert!(out.contains("/misc  /etc/auto.misc"));
    }

    #[test]
    fn merge_master_evicts_exact_and_ancestor_shadows_keeps_descendant() {
        // Managing `/mnt/pool` evicts the exact entry and any ANCESTOR entry
        // that would shadow it (`/mnt`), but keeps a more-specific descendant
        // (`/mnt/pool/data`) and unrelated foreign entries.
        let existing = "/mnt  /etc/auto.mnt\n/mnt/pool  /etc/auto.pool\n/mnt/pool/data  /etc/auto.data\n/keep  -hosts\n";
        let out = merge_master(existing, &["/mnt/pool".to_string()]);
        assert!(!out.contains("/etc/auto.mnt"));
        assert!(!out.contains("/etc/auto.pool"));
        assert!(out.contains("/etc/auto.data"));
        assert!(out.contains("/keep  -hosts"));
    }

    // ── PrivilegedOp::Unmount + PrivilegedResult serde ────────────────────

    const ID_A: &str = "0192f5a0-0000-7000-8000-00000000000a";
    const ID_B: &str = "0192f5a0-0000-7000-8000-00000000000b";

    #[test]
    fn older_payloads_without_newer_fields_still_parse() {
        let mount: PrivilegedOp = serde_json::from_str(r#"{"op":"mount","mounts":[]}"#).unwrap();
        assert!(matches!(
            mount,
            PrivilegedOp::Mount { keep_secret_files, .. } if keep_secret_files.is_empty()
        ));
        let r: PrivilegedResult =
            serde_json::from_str(r#"{"changed":[],"restarted":false,"errors":[]}"#).unwrap();
        assert!(r.unchanged.is_empty() && r.warnings.is_empty());
    }

    #[test]
    fn privileged_result_default_is_empty() {
        let r = PrivilegedResult::default();
        assert!(r.changed.is_empty() && r.errors.is_empty() && !r.restarted);
    }

    #[test]
    fn init_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Init::Systemd).unwrap(),
            "\"systemd\""
        );
        assert_eq!(serde_json::to_string(&Init::OpenRc).unwrap(), "\"open_rc\"");
    }

    // ── detect_master_file / detect_init are deterministic ────────────────

    #[test]
    fn detect_master_file_returns_a_known_path() {
        assert!(matches!(
            detect_master_file(),
            "/etc/autofs/auto.master" | "/etc/auto.master"
        ));
    }

    #[test]
    fn detect_init_returns_a_variant() {
        assert!(matches!(detect_init(), Init::Systemd | Init::OpenRc));
    }

    // ── write_atomic ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_atomic_creates_parent_and_leaves_no_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("deep/nested/auto.orca");
        let path = target.to_str().unwrap().to_string();
        write_atomic(&path, "body\n", None).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "body\n");
        assert!(!std::path::Path::new(&format!("{path}.orca.tmp")).exists());
    }

    #[tokio::test]
    async fn write_atomic_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("f");
        std::fs::write(&target, "old").unwrap();
        write_atomic(target.to_str().unwrap(), "new", None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    // ── execute_privileged: allowlist refusal (no root/restart needed) ────

    // ── generic secret-file seam (grammar-agnostic) ───────────────────────
    //
    // Core no longer stamps `credentials=<path>` into the autofs map nor resolves
    // any SMB credential grammar: a backend that needs a root-owned secret-file
    // produces the generic `SecretFile { path, contents }` on the native mount
    // path, and core only writes it 0600 (validating the path) and reaps it. The
    // per-backend inline-SMB creds tests live in `argyle-labs/smb`. The generic
    // primitives — path allowlist, 0600 write, orphan reaping — are tested below.

    #[test]
    fn allowlist_accepts_valid_secret_file_rejects_traversal() {
        use plugin_toolkit::storage::secret_file_path;
        // A legal secret-file path for a declared target is allowed.
        assert!(is_allowed_write(&secret_file_path("/mnt/media")));
        // The existing map/master paths still pass.
        assert!(is_allowed_write(MAP_FILE));
        // Traversal / out-of-scope paths are refused.
        assert!(!is_allowed_write("/etc/orca/secret-files/../../shadow"));
        assert!(!is_allowed_write("/etc/orca/secret-files/sub/x.secret"));
        assert!(!is_allowed_write("/etc/shadow"));
    }

    #[tokio::test]
    async fn write_atomic_secret_temp_is_never_wider_than_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("mnt_media.secret");
        let path = target.to_str().unwrap();
        std::fs::write(format!("{path}.orca.tmp"), "stale").unwrap();
        std::fs::set_permissions(
            format!("{path}.orca.tmp"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        write_atomic(path, "password=p\n", Some(0o600))
            .await
            .unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "password=p\n");
        assert!(!std::path::Path::new(&format!("{path}.orca.tmp")).exists());
    }

    #[tokio::test]
    async fn write_atomic_enforces_0600_on_secret_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("mnt_media.secret");
        let path = target.to_str().unwrap();
        write_atomic(path, "username=svc\npassword=p\n", Some(0o600))
            .await
            .unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "secret-file must be 0600");
        // No temp file left behind, and the secret is on disk exactly once.
        assert!(!std::path::Path::new(&format!("{path}.orca.tmp")).exists());
    }

    #[tokio::test]
    async fn reap_removes_orphan_secret_but_keeps_declared_and_foreign() {
        // Real end-to-end teardown: a declared (kept) secret-file, an orphan
        // secret-file (deleted mount), and a foreign file must be, respectively,
        // kept, removed, and left alone.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap().to_string();
        let kept = tmp.path().join("mnt_keep.secret");
        let orphan = tmp.path().join("mnt_gone.secret");
        let foreign = tmp.path().join("README.txt");
        std::fs::write(&kept, "username=a\npassword=b\n").unwrap();
        std::fs::write(&orphan, "username=x\npassword=y\n").unwrap();
        std::fs::write(&foreign, "not ours").unwrap();

        let keep = [kept.to_str().unwrap().to_string()];
        let mut res = PrivilegedResult::default();
        reap_orphan_secret_files_in(&dir, &keep, &mut res).await;

        assert!(kept.exists(), "declared secret-file must survive");
        assert!(!orphan.exists(), "orphan secret-file must be reaped");
        assert!(foreign.exists(), "foreign file must be left alone");
        assert!(
            res.errors.is_empty(),
            "clean reap has no errors: {:?}",
            res.errors
        );
    }

    fn apply_writes(map: &str, master: &str) -> Vec<FileWrite> {
        vec![
            FileWrite {
                path: MAP_FILE.into(),
                contents: map.into(),
                mode: None,
            },
            FileWrite {
                path: "/etc/auto.master".into(),
                contents: master.into(),
                mode: None,
            },
        ]
    }

    #[test]
    fn root_helper_accepts_real_rendered_master_and_map() {
        let ms = vec![mount("a", "host:/export", Some("h2:/export"))];
        let w = apply_writes(&render_map(&ms), &merge_master("", &managed_targets(&ms)));
        assert_eq!(validate_apply_writes(&w, ""), Vec::<String>::new());
        let mut el = std::collections::HashMap::new();
        el.insert("/mnt/a".to_string(), "host:/export".to_string());
        let w = apply_writes(&render_map_elected(&ms, &el), &merge_master("", &[]));
        assert!(validate_apply_writes(&w, "").is_empty());
    }

    #[test]
    fn root_helper_refuses_newline_in_source() {
        let m = mount("a", "host:/x\n/mnt/evil  -fstype=nfs  evil:/y", None);
        assert!(validate_mount_fields(&m).is_err());
        assert!(refuse_unrepresentable(&[m]).is_some());
        let w = apply_writes("/mnt/a  -fstype=nfs  host:/x\0\n", "");
        assert!(!validate_apply_writes(&w, "").is_empty());
    }

    #[test]
    fn root_helper_refuses_space_in_options() {
        let mut m = mount("a", "host:/x", None);
        m.options = Some("vers=4 evil:/y".into());
        assert!(validate_mount_fields(&m).is_err());
        assert!(refuse_unrepresentable(&[m]).is_some());
        let w = apply_writes("/mnt/a  vers=4  host:/x\n", "");
        assert!(!validate_apply_writes(&w, "").is_empty());
    }

    #[test]
    fn root_helper_refuses_program_master_entry() {
        for line in [
            "/- program:/x",
            "/- /etc/other.map",
            "/- /etc/auto.orca program:/x",
            "/mnt ldap:foo",
        ] {
            let w = apply_writes("", &format!("{line}\n"));
            assert!(!validate_apply_writes(&w, "").is_empty(), "{line}");
        }
    }

    #[test]
    fn root_helper_accepts_merge_preserving_existing_foreign_lines() {
        let existing = "/net -hosts\n+auto.master\n/mnt/pool /etc/auto.pool\n";
        let ms = vec![mount("a", "host:/export", None)];
        let w = apply_writes(
            &render_map(&ms),
            &merge_master(existing, &managed_targets(&ms)),
        );
        assert_eq!(validate_apply_writes(&w, existing), Vec::<String>::new());
    }

    #[test]
    fn root_helper_refuses_added_or_modified_foreign_master_lines() {
        let existing = "/net -hosts\n";
        for added in ["/- program:/x", "/evil /etc/other.map"] {
            let master = merge_master(&format!("{existing}{added}\n"), &[]);
            let w = apply_writes("", &master);
            assert!(!validate_apply_writes(&w, existing).is_empty(), "{added}");
        }
        let master = merge_master("/net -hosts,nosuid\n", &[]);
        let w = apply_writes("", &master);
        assert!(!validate_apply_writes(&w, existing).is_empty());
    }

    #[test]
    fn root_helper_tolerates_preexisting_program_master_line() {
        let existing = "/misc program:/usr/bin/x\n";
        let w = apply_writes("", &merge_master(existing, &[]));
        assert!(validate_apply_writes(&w, existing).is_empty());
    }

    #[test]
    fn root_helper_refuses_foreign_scheme_location() {
        let w = apply_writes("/mnt/a  -fstype=nfs  program:/x\n", "");
        assert!(!validate_apply_writes(&w, "").is_empty());
    }

    fn managed_apply(map: &str, mounts: &[ManagedMount]) -> PrivilegedOp {
        PrivilegedOp::Apply {
            writes: vec![FileWrite {
                path: MAP_FILE.into(),
                contents: map.into(),
                mode: None,
            }],
            keep_secret_files: Vec::new(),
            init: Init::Systemd,
            placements: Vec::new(),
            managed: Some(ManagedMaps::from_mounts(mounts, None)),
        }
    }

    #[test]
    fn managed_map_matching_render_is_accepted() {
        let ms = vec![mount("a", "h:/x", Some("h2:/x"))];
        let PrivilegedOp::Apply {
            writes, managed, ..
        } = managed_apply(&render_map(&ms), &ms)
        else {
            unreachable!()
        };
        assert!(validate_managed(managed.as_ref(), &writes).is_empty());
    }

    #[tokio::test]
    async fn managed_map_with_injected_line_is_refused_and_unwritten() {
        let ms = vec![mount("a", "h:/x", None)];
        let map = format!(
            "{}/mnt/evil  -fstype=nfs  evil:/x
",
            render_map(&ms)
        );
        let res = execute_privileged(managed_apply(&map, &ms)).await;
        assert!(res.changed.is_empty() && !res.restarted);
        assert!(res.errors.iter().any(|e| e.contains("does not match")));
    }

    #[tokio::test]
    async fn managed_source_with_newline_is_refused() {
        let ms = vec![mount("a", "h:/x\n/mnt/evil  -fstype=nfs  e:/x", None)];
        let res = execute_privileged(managed_apply(&render_map(&ms), &ms)).await;
        assert!(res.changed.is_empty() && !res.errors.is_empty());
    }

    #[tokio::test]
    async fn map_write_without_managed_is_refused_and_unwritten() {
        let ms = vec![mount("a", "h:/x", None)];
        let res = execute_privileged(PrivilegedOp::Apply {
            writes: apply_writes(&render_map(&ms), ""),
            keep_secret_files: Vec::new(),
            init: Init::Systemd,
            placements: Vec::new(),
            managed: None,
        })
        .await;
        assert!(res.changed.is_empty() && !res.restarted);
        assert!(res.errors.iter().any(|e| e.contains("requires structured")));
    }

    #[test]
    fn master_only_write_without_managed_is_accepted() {
        let w = vec![FileWrite {
            path: "/etc/auto.master".into(),
            contents: "/net\t-hosts\n".into(),
            mode: None,
        }];
        assert!(validate_managed(None, &w).is_empty());
    }

    #[test]
    fn empty_map_retire_with_zero_mounts_is_accepted() {
        let none = ManagedMaps::from_mounts(&[], None);
        for contents in [render_map(&[]), String::new()] {
            let w = vec![FileWrite {
                path: MAP_FILE.into(),
                contents,
                mode: None,
            }];
            assert!(validate_managed(Some(&none), &w).is_empty());
        }
    }

    #[test]
    fn empty_map_retire_with_extra_text_is_refused() {
        let none = ManagedMaps::from_mounts(&[], None);
        let w = vec![FileWrite {
            path: MAP_FILE.into(),
            contents: format!("{}/mnt/evil  -fstype=nfs  e:/x\n", render_map(&[])),
            mode: None,
        }];
        assert!(!validate_managed(Some(&none), &w).is_empty());
    }

    #[test]
    fn apply_payload_without_managed_deserializes() {
        let json = r#"{"op":"apply","writes":[],"init":"systemd"}"#;
        match serde_json::from_str::<PrivilegedOp>(json).unwrap() {
            PrivilegedOp::Apply { managed, .. } => assert!(managed.is_none()),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refused_apply_writes_nothing() {
        let res = execute_privileged(PrivilegedOp::Apply {
            writes: apply_writes(&render_map(&[mount("a", "h:/x", None)]), "/- program:/x\n"),
            keep_secret_files: vec![],
            init: Init::Systemd,
            placements: vec![],
            managed: Some(ManagedMaps::from_mounts(&[mount("a", "h:/x", None)], None)),
        })
        .await;
        assert!(res.changed.is_empty() && !res.restarted && !res.errors.is_empty());
    }

    #[tokio::test]
    async fn execute_privileged_refuses_non_allowlisted_path() {
        let op = PrivilegedOp::Apply {
            writes: vec![FileWrite {
                path: "/etc/passwd".into(),
                contents: "x".into(),
                mode: None,
            }],
            keep_secret_files: Vec::new(),
            init: Init::OpenRc,
            placements: Vec::new(),
            managed: None,
        };
        let res = execute_privileged(op).await;
        assert!(res.changed.is_empty());
        assert!(!res.restarted, "no restart when nothing written");
        assert!(
            res.errors
                .iter()
                .any(|e| e.contains("refused non-allowlisted"))
        );
    }

    // ── managed_targets ───────────────────────────────────────────────────

    #[test]
    fn managed_targets_keeps_only_enabled_network_shares() {
        let mut disabled = mount("off", "primary:/o", None);
        disabled.enabled = false;
        let mut disk = mount("disk", "primary:/d", None);
        disk.kind = "disk_storage".into();
        let mounts = vec![
            mount("alpha", "primary:/a", None),
            mount("beta", "primary:/b", None),
            disabled,
            disk,
        ];
        let targets = managed_targets(&mounts);
        assert_eq!(targets, vec!["/mnt/alpha", "/mnt/beta"]);
    }

    #[test]
    fn managed_targets_empty_when_nothing_enabled_network() {
        let mut disk = mount("disk", "primary:/d", None);
        disk.kind = "disk_storage".into();
        assert!(managed_targets(&[disk]).is_empty());
    }

    // ── render_backend_options fallback (unregistered backend) ────────────

    #[test]
    fn render_backend_options_unregistered_none_is_empty() {
        // No backend named this is registered in the `system` test build, so the
        // fallback `None` arm renders an empty string for absent options.
        assert_eq!(render_backend_options("no_such_backend", "nfs4", None), "");
    }

    #[test]
    fn render_backend_options_unregistered_renders_raw_verbatim() {
        // The fallback arm passes the raw option string through `OptionSet::Raw`
        // verbatim — byte-identical to core's prior behavior.
        assert_eq!(
            render_backend_options("no_such_backend", "nfs4", Some("ro,vers=4.2")),
            "ro,vers=4.2"
        );
    }

    // ── strip_fstab_only (direct) ─────────────────────────────────────────

    #[test]
    fn strip_fstab_only_all_fstab_opts_leaves_bare_fstype() {
        assert_eq!(
            strip_fstab_only("nfs4", "_netdev,nofail,x-systemd.automount,auto,noauto"),
            "-fstype=nfs4"
        );
    }

    #[test]
    fn strip_fstab_only_empty_rendered_is_bare_fstype() {
        assert_eq!(strip_fstab_only("cifs", ""), "-fstype=cifs");
    }

    // ── net_fstypes ───────────────────────────────────────────────────────

    #[test]
    fn net_fstypes_is_sorted_deduped_and_stable() {
        let a = net_fstypes();
        // Deterministic across calls.
        assert_eq!(a, net_fstypes());
        // Sorted ascending and free of duplicates (BTreeSet-backed).
        let mut sorted = a.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(a, sorted);
    }

    // ── is_ancestor_or_equal edge cases ───────────────────────────────────

    #[test]
    fn is_ancestor_or_equal_trailing_slash_on_target() {
        // Both sides trailing-slash normalized before comparison.
        assert!(is_ancestor_or_equal("/mnt/pool", "/mnt/pool/"));
        assert!(is_ancestor_or_equal("/mnt/pool/", "/mnt/pool/"));
    }

    #[test]
    fn is_ancestor_or_equal_deeply_nested_descendant() {
        assert!(is_ancestor_or_equal("/mnt/pool", "/mnt/pool/a/b/c"));
        assert!(!is_ancestor_or_equal("/mnt/pool/a", "/mnt/pool/ab/c"));
    }

    // ── map_line_for with no options ──────────────────────────────────────

    #[test]
    fn map_line_for_no_options_is_bare_fstype() {
        let mut m = mount("solo", "primary:/s", Some("secondary:/s"));
        m.options = None;
        assert_eq!(
            map_line_for(&m, "primary:/s"),
            "/mnt/solo  -fstype=nfs4  primary:/s"
        );
    }

    // ── render_map_elected sorting ────────────────────────────────────────

    #[test]
    fn render_map_elected_sorts_by_target() {
        let zeta = mount("zeta", "primary:/z", None);
        let alpha = mount("alpha", "primary:/a", None);
        let map = render_map_elected(
            &[zeta, alpha],
            &elected(&[("/mnt/zeta", "primary:/z"), ("/mnt/alpha", "primary:/a")]),
        );
        let body: Vec<&str> = map.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(body.len(), 2);
        assert!(body[0].starts_with("/mnt/alpha"));
        assert!(body[1].starts_with("/mnt/zeta"));
    }

    #[test]
    fn render_map_elected_skips_disabled_and_non_network() {
        let mut disabled = mount("off", "primary:/o", None);
        disabled.enabled = false;
        let mut disk = mount("disk", "primary:/d", None);
        disk.kind = "disk_storage".into();
        // Both have an election, but neither qualifies (disabled / non-network).
        let map = render_map_elected(
            &[disabled, disk],
            &elected(&[("/mnt/off", "primary:/o"), ("/mnt/disk", "primary:/d")]),
        );
        assert_eq!(map, HEADER);
    }

    // ── merge_master drops stale in-block content ─────────────────────────

    #[test]
    fn merge_master_drops_stale_orca_block_content() {
        // A prior orca block carrying stale lines must be fully regenerated, not
        // preserved, while foreign config outside the markers survives.
        let existing = format!(
            "/net\t-hosts\n{BLOCK_BEGIN}\n/-  {MAP_FILE} --timeout=999\nstale junk\n{BLOCK_END}\n"
        );
        let out = merge_master(&existing, &[]);
        assert!(!out.contains("stale junk"), "stale block content dropped");
        assert!(!out.contains("--timeout=999"), "stale registration dropped");
        assert_eq!(out.matches(BLOCK_BEGIN).count(), 1);
        assert_eq!(out.matches(MAP_FILE).count(), 1);
        assert!(out.contains("/net\t-hosts"));
    }

    // ── retire_master trailing-blank tidy ─────────────────────────────────

    #[test]
    fn retire_master_trims_trailing_blank_lines() {
        let retired = retire_master("/net\t-hosts\n\n\n");
        assert_eq!(retired, "/net\t-hosts\n");
    }

    #[test]
    fn retire_master_empty_input_is_empty() {
        assert_eq!(retire_master(""), "");
    }

    // ── apply_op short-circuits an empty diff (no privileged call) ─────────

    #[tokio::test]
    async fn apply_op_empty_writes_is_clean_noop() {
        // An Apply with no writes must return the default outcome WITHOUT shelling
        // out to the privileged helper — the idempotent-host fast path.
        let op = PrivilegedOp::Apply {
            writes: Vec::new(),
            keep_secret_files: Vec::new(),
            init: Init::OpenRc,
            placements: Vec::new(),
            managed: None,
        };
        let out = apply_op(op).await;
        assert!(out.changed.is_empty());
        assert!(!out.reloaded);
        assert!(out.errors.is_empty());
    }

    // ── outcome defaults ──────────────────────────────────────────────────

    #[test]
    fn apply_outcome_default_is_empty() {
        let o = ApplyOutcome::default();
        assert!(o.changed.is_empty() && !o.reloaded && o.errors.is_empty());
    }

    #[test]
    fn recover_outcome_default_is_empty() {
        let o = RecoverOutcome::default();
        assert!(o.recovered.is_empty());
        assert!(o.still_stale.is_empty());
        assert!(o.healthy.is_empty());
        assert!(o.errors.is_empty());
        assert!(!o.no_stale_found);
    }

    // ── FileWrite serde: mode is omitted when None, present when set ───────

    #[test]
    fn file_write_omits_none_mode_and_emits_some_mode() {
        let none = FileWrite {
            path: MAP_FILE.into(),
            contents: "x".into(),
            mode: None,
        };
        let s = serde_json::to_string(&none).unwrap();
        assert!(!s.contains("mode"), "None mode must be skipped: {s}");
        assert_eq!(serde_json::from_str::<FileWrite>(&s).unwrap(), none);

        let some = FileWrite {
            path: MAP_FILE.into(),
            contents: "x".into(),
            mode: Some(0o600),
        };
        let s = serde_json::to_string(&some).unwrap();
        assert!(s.contains("\"mode\":384"), "0o600 == 384: {s}");
        assert_eq!(serde_json::from_str::<FileWrite>(&s).unwrap(), some);
    }

    // ── PrivilegedOp::Apply deserializes with keep_secret_files defaulted ──

    #[test]
    fn apply_op_deserializes_without_keep_secret_files() {
        let json = format!(
            "{{\"op\":\"apply\",\"writes\":[{{\"path\":\"{MAP_FILE}\",\"contents\":\"x\"}}],\"init\":\"systemd\"}}"
        );
        let op: PrivilegedOp = serde_json::from_str(&json).unwrap();
        match op {
            PrivilegedOp::Apply {
                writes,
                keep_secret_files,
                init,
                ..
            } => {
                assert_eq!(writes.len(), 1);
                assert!(keep_secret_files.is_empty(), "defaulted to empty");
                assert_eq!(init, Init::Systemd);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    // ── PrivilegedOp::Mount serde roundtrip ───────────────────────────────

    // ── reap on an absent directory is a clean no-op ──────────────────────

    #[tokio::test]
    async fn reap_orphan_secret_files_in_absent_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does/not/exist");
        let mut res = PrivilegedResult::default();
        reap_orphan_secret_files_in(missing.to_str().unwrap(), &[], &mut res).await;
        assert!(res.errors.is_empty(), "absent dir yields no errors");
    }

    // ── target_absent_from_table with an empty table ──────────────────────

    #[test]
    fn target_absent_from_table_empty_table_is_absent() {
        assert!(target_absent_from_table(&[], "/mnt/data"));
    }

    // ── Init deserialize (snake_case, wire-symmetric with serialize) ──────

    #[test]
    fn init_deserializes_snake_case() {
        assert_eq!(
            serde_json::from_str::<Init>("\"systemd\"").unwrap(),
            Init::Systemd
        );
        assert_eq!(
            serde_json::from_str::<Init>("\"open_rc\"").unwrap(),
            Init::OpenRc
        );
        // Unknown / wrong-case tokens are rejected.
        assert!(serde_json::from_str::<Init>("\"openrc\"").is_err());
    }

    fn id_n(n: u128) -> String {
        uuid::Uuid::from_u128(n).hyphenated().to_string()
    }

    fn led_entry(target: &str, source: &str) -> LedgerEntry {
        LedgerEntry {
            target: target.into(),
            source: source.into(),
        }
    }

    #[test]
    fn ledger_updates_from_two_threads_both_land() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger").to_str().unwrap().to_string();
        for i in 0..50u128 {
            let run = |n: u128, t: String| {
                let path = path.clone();
                let t = (id_n(n), led_entry(&t, "h:/e"));
                std::thread::spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap()
                        .block_on(ledger_update(&path, &[t], &[]))
                        .unwrap();
                })
            };
            let (a, b) = (
                run(2 * i, format!("/mnt/a{i}")),
                run(2 * i + 1, format!("/mnt/b{i}")),
            );
            a.join().unwrap();
            b.join().unwrap();
        }
        let set = parse_ledger(&read_ledger(&path).unwrap()).unwrap();
        assert_eq!(set.len(), 100);
    }

    #[tokio::test]
    async fn ledger_lock_released_after_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        std::fs::write(&path, "relative\n").unwrap();
        let path = path.to_str().unwrap();
        let add = [(ID_B.to_string(), led_entry("/mnt/b", "h:/b"))];
        assert!(ledger_update(path, &add, &[]).await.is_err());
        let lock = lock_ledger(path).await.unwrap();
        drop(lock);
        std::fs::write(path, format!("{ID_A}\t/mnt/a\th:/a\n")).unwrap();
        ledger_update(path, &add, &[]).await.unwrap();
    }

    #[test]
    fn privileged_result_parses_without_unchanged() {
        let r: PrivilegedResult =
            serde_json::from_str(r#"{"changed":[],"restarted":false,"errors":[]}"#).unwrap();
        assert!(r.unchanged.is_empty());
    }

    // ── PrivilegedResult serde with populated fields ──────────────────────

    // ── PrivilegedOp::Mount roundtrip carrying a secret_file ──────────────

    // ── Apply op serde carries keep_secret_files when populated ───────────

    #[test]
    fn apply_op_serializes_keep_secret_files_when_present() {
        let op = PrivilegedOp::Apply {
            writes: Vec::new(),
            keep_secret_files: vec!["/etc/orca/secret-files/mnt_x.secret".into()],
            init: Init::Systemd,
            placements: Vec::new(),
            managed: None,
        };
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("mnt_x.secret"));
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
    }

    // ── reap_legacy_secret_file_dir is a no-op when the legacy dir is absent ─

    #[tokio::test]
    async fn reap_legacy_secret_file_dir_absent_is_clean() {
        // On a dev/CI host the legacy `/etc/orca/smb-creds` tree does not exist,
        // so the one-time migration must record no error.
        let mut res = PrivilegedResult::default();
        reap_legacy_secret_file_dir(&mut res).await;
        assert!(
            res.errors.is_empty(),
            "absent legacy dir yields no error: {:?}",
            res.errors
        );
    }

    // ── trigger is bounded and skipped after a failed release ──────────────

    #[tokio::test]
    async fn trigger_that_never_completes_reports_still_stale() {
        let errors = trigger_with(
            &["/mnt/x".to_string()],
            |_| std::future::pending::<std::io::Result<()>>(),
            || std::future::ready(()),
        )
        .await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("still stale"), "{errors:?}");
    }

    #[tokio::test]
    async fn busy_probe_reports_busy() {
        let busy = is_busy_with(
            "/mnt/x",
            |_| std::future::ready(Ok(true)),
            std::future::pending::<()>,
        )
        .await;
        assert!(busy);
    }

    #[tokio::test]
    async fn idle_probe_reports_not_busy() {
        let busy = is_busy_with(
            "/mnt/x",
            |_| std::future::ready(Ok(false)),
            std::future::pending::<()>,
        )
        .await;
        assert!(!busy);
    }

    #[tokio::test]
    async fn probe_that_never_answers_proceeds_to_release() {
        let busy = is_busy_with(
            "/mnt/x",
            |_| std::future::pending::<std::io::Result<bool>>(),
            || std::future::ready(()),
        )
        .await;
        assert!(!busy);
    }

    #[tokio::test]
    async fn refused_release_does_not_trigger() {
        let called = std::sync::atomic::AtomicBool::new(false);
        let errors = release_then_trigger(
            "/mnt/x",
            || async { vec!["refused".to_string()] },
            || async {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                Vec::new()
            },
        )
        .await;
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(errors.iter().any(|e| e == "refused"));
    }

    #[tokio::test]
    async fn clean_release_triggers() {
        let errors = release_then_trigger(
            "/mnt/x",
            || async { Vec::new() },
            || async { vec!["t".to_string()] },
        )
        .await;
        assert_eq!(errors, vec!["t".to_string()]);
    }

    // ── trigger only records spawn failures, not non-zero stat exits ───────

    #[tokio::test]
    async fn trigger_succeeds_on_existing_path() {
        // `stat` exists on the runner; an existing path stats clean, so trigger
        // returns no errors (it only collects spawn failures).
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().to_str().unwrap().to_string();
        let errors = trigger(std::slice::from_ref(&target)).await;
        assert!(
            errors.is_empty(),
            "existing path triggers cleanly: {errors:?}"
        );
    }

    // ── mount-table reads for an unmounted target ─────────────────────────

    #[tokio::test]
    async fn target_has_no_mount_true_for_bogus_path() {
        // Nothing is mounted at this synthetic path, so the live mount-table
        // read must report it absent.
        assert!(target_has_no_mount("/orca/definitely/not/mounted/xyz").await);
    }

    #[tokio::test]
    async fn current_source_for_target_none_for_bogus_path() {
        assert!(
            current_source_for_target("/orca/definitely/not/mounted/xyz")
                .await
                .is_none()
        );
    }

    // ── RecoverOutcome / ApplyOutcome are Clone + Debug (surfaced to tools) ─

    #[test]
    fn recover_outcome_clone_preserves_fields() {
        let o = RecoverOutcome {
            recovered: vec!["/mnt/a".into()],
            still_stale: vec!["/mnt/b".into()],
            healthy: vec!["/mnt/c".into()],
            errors: vec!["e".into()],
            no_stale_found: false,
        };
        let c = o.clone();
        assert_eq!(c.recovered, o.recovered);
        assert_eq!(c.still_stale, o.still_stale);
        assert_eq!(c.healthy, o.healthy);
        assert_eq!(c.errors, o.errors);
    }

    // ── reap_orphan_secret_files_in: teardown of stale secret-files ─────────
    //
    // Exercised against an explicit tempdir (not the fixed SECRET_FILE_DIR) via
    // the split-out `_in` helper: a file is reaped iff it has a valid secret-file
    // name AND is absent from the keep set. Foreign files are never touched.

    #[tokio::test]
    async fn reap_absent_dir_is_noop() {
        // A directory that does not exist is a clean no-op — no errors recorded.
        let mut res = PrivilegedResult::default();
        reap_orphan_secret_files_in("/no/such/orca/reap/dir", &[], &mut res).await;
        assert!(res.errors.is_empty());
    }

    #[tokio::test]
    async fn reap_removes_orphan_secret_file_keeps_foreign_and_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap().to_string();

        // A valid secret-file name that is NOT in keep → reaped.
        let orphan = tmp.path().join("mnt_data.secret");
        std::fs::write(&orphan, b"secret").unwrap();
        // A valid secret-file name that IS in keep → preserved.
        let kept = tmp.path().join("mnt_media.secret");
        std::fs::write(&kept, b"secret").unwrap();
        // A foreign (non-secret-file) name → never touched, even absent from keep.
        let foreign = tmp.path().join("notes.txt");
        std::fs::write(&foreign, b"hello").unwrap();

        let keep = vec![kept.to_str().unwrap().to_string()];
        let mut res = PrivilegedResult::default();
        reap_orphan_secret_files_in(&dir, &keep, &mut res).await;

        assert!(!orphan.exists(), "orphan secret-file must be reaped");
        assert!(kept.exists(), "kept secret-file must survive");
        assert!(foreign.exists(), "foreign file must never be touched");
        assert!(res.errors.is_empty(), "no errors on a clean reap: {res:?}");
    }

    // ── execute_privileged: Release arm refuses an id the ledger lacks ─────
    #[tokio::test]
    async fn execute_privileged_release_refuses_an_unledgered_id() {
        let op = PrivilegedOp::Release {
            ids: vec![ID_A.into()],
        };
        let res = execute_privileged(op).await;
        assert!(res.changed.is_empty(), "nothing was actually released");
        assert!(!res.restarted, "Release never restarts autofs");
        assert_eq!(res.errors.len(), 1, "one release error collected: {res:?}");
        assert!(res.errors[0].contains(&format!("release {ID_A}: refused")));
    }

    // ── execute_privileged: Mount arm rejects a non-allowlisted secret-file ─
    #[tokio::test]
    async fn execute_privileged_mount_records_secret_file_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("media").to_str().unwrap().to_string();
        let op = PrivilegedOp::Mount {
            mounts: vec![crate::mount_exec::MountReq {
                placement_id: ID_A.into(),
                source: "//host/share".into(),
                target,
                fstype: "cifs".into(),
                options: "rw".into(),
                secret_file: Some(crate::mount_exec::SecretFile {
                    path: "/etc/orca/secret-files/../../shadow".into(),
                    contents: "username=x\npassword=y\n".into(),
                }),
            }],
            keep_secret_files: Vec::new(),
        };
        let res = execute_privileged(op).await;
        assert!(res.changed.is_empty(), "no mount succeeded");
        assert_eq!(res.errors.len(), 1, "one mount error: {res:?}");
        assert!(
            res.errors[0].contains("non-allowlisted secret-file path"),
            "refusal surfaced: {:?}",
            res.errors
        );
    }

    // ── execute_privileged: Reload arm produces a coherent result ──────────
    #[tokio::test]
    async fn execute_privileged_reload_reports_restart_or_error() {
        let res = execute_privileged(PrivilegedOp::Reload {
            init: Init::Systemd,
        })
        .await;
        assert!(res.changed.is_empty(), "Reload writes nothing");
        assert!(
            res.restarted ^ !res.errors.is_empty(),
            "exactly one of restarted / errored: {res:?}"
        );
        if !res.restarted {
            assert!(res.errors[0].contains("reload autofs"));
        }
    }

    // ── elect_live_source: every source down elects nothing ────────────────
    #[tokio::test]
    async fn elect_live_source_empty_when_all_sources_down() {
        let m = mount("down", "192.0.2.1:/srv/x", None);
        let election = elect_live_source(&m, Duration::from_millis(150)).await;
        assert_eq!(election, Election::Empty);
    }

    // ── probe_stale: unmounted targets are all flagged for recovery ─────────
    #[tokio::test]
    async fn probe_stale_flags_all_unmounted_targets() {
        let targets = vec![
            "/orca/not/mounted/a".to_string(),
            "/orca/not/mounted/b".to_string(),
        ];
        let stale = probe_stale(&targets, Duration::from_millis(150)).await;
        assert_eq!(stale, targets, "both unmounted targets need recovery");
    }

    // ── probe: a bogus path yields a concrete MountHealth (no panic across the seam) ─
    #[tokio::test]
    async fn probe_returns_health_for_bogus_path() {
        let h = probe("/orca/not/mounted/probe", Duration::from_millis(150)).await;
        assert_ne!(h, MountHealth::Ok, "unmounted path must not probe healthy");
    }

    // ── recover: no targets is a clean, no-stale sweep ─────────────────────
    #[tokio::test]
    async fn recover_empty_targets_finds_no_stale() {
        let out = recover(&[], Duration::from_millis(150)).await;
        assert!(out.recovered.is_empty());
        assert!(out.still_stale.is_empty());
        assert!(out.healthy.is_empty());
        assert!(out.errors.is_empty());
        assert!(
            out.no_stale_found,
            "an empty sweep reports no stale mounts found"
        );
    }

    // ── recover: an existing dir probes healthy and is never acted on ──────
    #[tokio::test]
    async fn recover_healthy_target_is_classified_healthy_not_recovered() {
        // A real, existing directory stats clean → `probe_health` => MountHealth::Ok,
        // so `recover` must route it to `healthy` and take NO recovery action
        // (no reload, no unmount). Deterministic: no network, no privilege.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().to_str().unwrap().to_string();
        let out = recover(std::slice::from_ref(&target), Duration::from_millis(500)).await;
        assert_eq!(out.healthy, vec![target], "existing dir is healthy");
        assert!(
            out.recovered.is_empty(),
            "a healthy target is never recovered"
        );
        assert!(out.still_stale.is_empty());
        assert!(
            out.errors.is_empty(),
            "healthy sweep records no errors: {out:?}"
        );
        assert!(
            out.no_stale_found,
            "no recovered and no still-stale => no stale found"
        );
    }

    // ── plan: read-only take-over-merge + file diff (no privileged spawn) ─────
    //
    // `plan` only reads the host's (world-readable) master file and diffs the
    // rendered map/master against disk; it never shells out. The parts under our
    // control — the op variant, the empty `keep_secret_files` (the autofs path owns
    // no secret-file teardown), a concrete `init`, and the delegation of body
    // rendering to `render_map` — are deterministic regardless of host state.

    #[tokio::test]
    async fn plan_empty_mounts_is_apply_with_empty_keep_set_and_concrete_init() {
        let op = plan(&[]).await;
        match op {
            PrivilegedOp::Apply {
                keep_secret_files,
                init,
                ..
            } => {
                assert!(
                    keep_secret_files.is_empty(),
                    "the autofs apply path never reaps secret-files"
                );
                assert!(matches!(init, Init::Systemd | Init::OpenRc));
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_network_mount_is_apply_and_delegates_map_body_to_render_map() {
        let m = mount("data", "primary:/srv/data", Some("secondary:/srv/data"));
        let op = plan(std::slice::from_ref(&m)).await;
        match op {
            PrivilegedOp::Apply {
                writes,
                keep_secret_files,
                init,
                ..
            } => {
                assert!(keep_secret_files.is_empty());
                assert!(matches!(init, Init::Systemd | Init::OpenRc));
                // Every emitted write targets an allowlisted path — plan never
                // proposes a write the root helper would refuse.
                assert!(
                    writes.iter().all(|w| is_allowed_write(&w.path)),
                    "plan only proposes allowlisted writes: {:?}",
                    writes.iter().map(|w| &w.path).collect::<Vec<_>>()
                );
                // When the map file is (re)written, its body is exactly what
                // `render_map` produces for this mount set — plan delegates
                // rendering rather than re-deriving it.
                if let Some(w) = writes.iter().find(|w| w.path == MAP_FILE) {
                    assert_eq!(w.contents, render_map(std::slice::from_ref(&m)));
                    assert!(w.contents.contains("/mnt/data  -fstype=nfs4"));
                    assert!(w.contents.contains("primary:/srv/data secondary:/srv/data"));
                    assert!(w.mode.is_none(), "map file carries no explicit mode");
                }
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_guard_refuses_to_run_real_sudo() {
        let r = run_privileged(&PrivilegedOp::Release {
            ids: vec![ID_A.into()],
        })
        .await;
        assert_eq!(r.errors.len(), 1, "{r:?}");
        assert!(
            r.errors[0].contains("refused in test builds"),
            "{:?}",
            r.errors
        );
        assert!(!r.restarted);
        assert!(r.changed.is_empty());
    }

    #[tokio::test]
    async fn run_privileged_sends_the_op_to_the_storage_apply_helper() {
        use utils::privileged::testing::{Recorder, with_exec};
        let rec = Recorder::new(utils::privileged::PrivilegedOutput {
            success: true,
            stdout: br#"{"changed":[],"restarted":true,"errors":[]}"#.to_vec(),
            ..Default::default()
        });
        let op = PrivilegedOp::Release {
            ids: vec![ID_A.into()],
        };
        let r = with_exec(rec.clone(), run_privileged(&op)).await;
        assert!(r.restarted, "{r:?}");
        let calls = rec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].argv[1..], ["admin", "storage-apply"]);
        assert_eq!(calls[0].stdin, serde_json::to_vec(&op).unwrap());
    }

    #[tokio::test]
    async fn run_privileged_surfaces_a_helper_failure() {
        use utils::privileged::testing::{Recorder, with_exec};
        let rec = Recorder::new(utils::privileged::PrivilegedOutput {
            success: false,
            status: "exit status: 1".into(),
            stderr: b"sudo: a password is required".to_vec(),
            ..Default::default()
        });
        let r = with_exec(
            rec,
            run_privileged(&PrivilegedOp::Release {
                ids: vec![ID_A.into()],
            }),
        )
        .await;
        assert!(r.errors[0].contains("a password is required"), "{r:?}");
        assert!(!r.restarted);
    }

    #[tokio::test]
    async fn force_reload_surfaces_helper_errors_without_a_grant() {
        let errors = force_reload().await;
        assert!(!errors.is_empty(), "{errors:?}");
    }

    // ── force_and_retrigger: a permanently-missing target is never recovered ──
    #[tokio::test]
    async fn force_and_retrigger_missing_target_never_recovers() {
        // The target is a bogus path nothing ever mounts, so every escalation
        // rung (reload → unmount → reload) fails to make it appear. The result
        // must be `recovered == false` with the accumulated helper errors, and it
        // must not panic across the privileged seam.
        let (recovered, errors) =
            force_and_retrigger("/orca/not/mounted/xyz", true, Duration::from_millis(150)).await;
        assert!(!recovered, "an unmountable path can never be recovered");
        assert!(
            !errors.is_empty(),
            "the failed privileged escalations are collected: {errors:?}"
        );
    }

    // ── recover: a missing target escalates and lands in still_stale ──────────
    #[tokio::test]
    async fn recover_missing_target_is_still_stale_not_healthy() {
        // A bogus unmounted path probes as Missing (see `probe_stale_flags_...`),
        // so `recover` routes it through `force_and_retrigger`, which can never
        // bring it up → it is reported still-stale, never healthy or recovered.
        let target = "/orca/not/mounted/xyz".to_string();
        let out = recover(std::slice::from_ref(&target), Duration::from_millis(150)).await;
        assert!(out.healthy.is_empty(), "a missing target is not healthy");
        assert!(
            out.recovered.is_empty(),
            "an unmountable target is never recovered"
        );
        assert_eq!(
            out.still_stale,
            vec![target],
            "the missing target is reported still-stale: {out:?}"
        );
        assert!(
            !out.no_stale_found,
            "a still-stale target means stale was found"
        );
    }

    // ── reconcile_source under Force still no-ops when every source is down ────
    #[tokio::test]
    async fn reconcile_source_all_down_force_policy_is_empty_target() {
        // Even under the opt-in Force policy, an all-sources-down mount elects
        // nothing → EmptyTarget → no unmount/trigger shell-out is attempted, so
        // the outcome is identical to Safe and side-effect-free.
        let m = mount("down", "192.0.2.1:/srv/x", Some("192.0.2.2:/srv/x"));
        let (trans, errors) =
            reconcile_source(&m, RemountAggression::Force, Duration::from_millis(150)).await;
        assert_eq!(trans, Transition::EmptyTarget);
        assert!(
            errors.is_empty(),
            "EmptyTarget attempts no remount: {errors:?}"
        );
    }

    // ── reconcile_source: every source down => EmptyTarget, no swap attempted ─
    #[tokio::test]
    async fn reconcile_source_all_down_is_empty_target_with_no_errors() {
        // TEST-NET-1 addresses (192.0.2.0/24) are guaranteed unroutable, so every
        // source probes down → election Empty → transition EmptyTarget. The
        // EmptyTarget arm does nothing (no unmount/trigger shell-out), so the
        // result is deterministic and side-effect-free.
        let m = mount("down", "192.0.2.1:/srv/x", Some("192.0.2.2:/srv/x"));
        let (trans, errors) =
            reconcile_source(&m, RemountAggression::Safe, Duration::from_millis(150)).await;
        assert_eq!(trans, Transition::EmptyTarget);
        assert!(
            errors.is_empty(),
            "EmptyTarget performs no remount, so no errors: {errors:?}"
        );
    }

    // ── SetShareOwner guards (#620) ────────────────────────────────────────

    #[test]
    fn a_repair_never_chowns_a_share_to_root() {
        // Chowning operator data to root is the escalation direction, and it can
        // lock the real service account out of its own tree. Refused even from
        // the trusted caller, same posture as the mode guard.
        assert!(!is_allowed_share_owner(0, 0));
        assert!(!is_allowed_share_owner(0, 100));
        assert!(!is_allowed_share_owner(34, 0));
    }

    #[test]
    fn real_service_owners_are_allowed() {
        // The two that actually appear on this fleet: PBS runs as 34, Unraid
        // shares are 99:100.
        assert!(is_allowed_share_owner(34, 34));
        assert!(is_allowed_share_owner(99, 100));
    }

    #[test]
    fn owner_repair_refuses_a_path_outside_a_managed_share() {
        // Reuses the same path guard as the mode repair; assert the pairing
        // holds so ownership can never become the weaker of the two surfaces.
        assert!(!is_allowed_share_path("/etc"));
        assert!(!is_allowed_share_path("/mnt/user"));
    }

    #[tokio::test]
    async fn test_guard_refuses_to_spawn_real_umount() {
        assert!(
            run_umount_lf("/orca/not/mounted/xyz".to_string())
                .await
                .is_err(),
            "tests must never reach a real umount"
        );
    }

    #[tokio::test]
    async fn force_unmount_of_an_unmounted_path_refuses_without_umount() {
        let err = force_unmount("/orca/not/mounted/xyz").await.unwrap_err();
        assert!(err.contains("nothing mounted"), "{err}");
    }

    #[tokio::test]
    async fn force_unmount_with_reports_detached_after_releasing_the_cifs_layer() {
        let table = stack(&["autofs", "cifs"]);
        let calls = std::cell::Cell::new(0);
        let out = force_unmount_with(
            "/mnt/data",
            |t| {
                assert_eq!(t, "/mnt/data");
                Ok(table.borrow().clone())
            },
            |t| {
                assert_eq!(t, "/mnt/data");
                calls.set(calls.get() + 1);
                table.borrow_mut().pop();
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(out, Ok(Unwound::Detached));
        assert_eq!(calls.get(), 1);
        assert_eq!(*table.borrow(), vec!["autofs".to_string()]);
    }

    async fn run_release(
        ledger: Result<String, String>,
        ids: &[&str],
        live_source: &str,
    ) -> (PrivilegedResult, Vec<String>, Vec<String>) {
        let calls = std::cell::RefCell::new(Vec::new());
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        let (res, released) = guard_release(
            &ids,
            || ledger,
            |_| Ok(vec![live_source.to_string()]),
            |t| {
                calls.borrow_mut().push(t);
                async { Ok(Unwound::Detached) }
            },
        )
        .await;
        (res, released, calls.into_inner())
    }

    fn ledgered() -> Result<String, String> {
        Ok(format!("{ID_A}\t/mnt/data\t//willow/data\n"))
    }

    #[tokio::test]
    async fn release_holds_the_ledger_lock_through_the_umount() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ledger").to_str().unwrap().to_string();
        std::fs::write(&p, format!("{ID_A}\t/mnt/data\t//willow/data\n")).unwrap();
        let blocked = std::cell::Cell::new(false);
        let lock_path = format!("{p}.lock");
        let res = release_locked(
            &p,
            &[ID_A.to_string()],
            &keys(&[]),
            |_| Ok(vec!["//willow/data".to_string()]),
            |_| {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .mode(0o600)
                    .open(&lock_path)
                    .unwrap();
                let r = rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive);
                blocked.set(r.is_err());
                async { Ok(Unwound::Detached) }
            },
        )
        .await;
        assert!(blocked.get(), "Mount could take the lock mid-release");
        assert_eq!(res.changed, vec!["/mnt/data"]);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap();
        rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .expect("lock released after release");
    }

    #[tokio::test]
    async fn release_refuses_when_a_different_mount_appears_before_the_umount() {
        let calls = std::cell::Cell::new(0);
        let ids = vec![ID_A.to_string()];
        let (res, released) = guard_release(
            &ids,
            ledgered,
            |_| {
                calls.set(calls.get() + 1);
                Ok(vec![
                    if calls.get() == 1 {
                        "//willow/data"
                    } else {
                        "//rogue/x"
                    }
                    .to_string(),
                ])
            },
            |_| async { panic!("detached an unauthorized mount") },
        )
        .await;
        assert!(released.is_empty());
        assert_eq!(res.errors.len(), 1, "{res:?}");
        assert!(res.errors[0].contains("changed since authorization"));
    }

    #[tokio::test]
    async fn release_unmounts_a_ledgered_id_whose_live_source_matches() {
        let (res, released, calls) = run_release(ledgered(), &[ID_A], "//WILLOW/data/").await;
        assert_eq!(calls, vec!["/mnt/data"]);
        assert_eq!(res.changed, vec!["/mnt/data"]);
        assert_eq!(released, vec![ID_A]);
        assert!(res.errors.is_empty(), "{res:?}");
    }

    #[tokio::test]
    async fn release_refuses_an_unknown_id() {
        let (res, released, calls) = run_release(ledgered(), &[ID_B], "//willow/data").await;
        assert!(calls.is_empty() && released.is_empty());
        assert_eq!(
            res.errors,
            vec![format!("release {ID_B}: refused: unknown placement id")]
        );
    }

    #[tokio::test]
    async fn release_detaches_a_mount_live_from_a_different_source_than_the_ledger() {
        let (res, released, calls) = run_release(ledgered(), &[ID_A], "//dead/data").await;
        assert_eq!(calls, vec!["/mnt/data"]);
        assert_eq!(released, vec![ID_A]);
        assert!(res.errors.is_empty(), "{res:?}");
    }

    #[tokio::test]
    async fn release_refuses_an_unledgered_target_whatever_is_live() {
        let (res, released, calls) = run_release(Ok(String::new()), &[ID_A], "//willow/data").await;
        assert!(calls.is_empty() && released.is_empty());
        assert_eq!(
            res.errors,
            vec![format!("release {ID_A}: refused: unknown placement id")]
        );
    }

    #[tokio::test]
    async fn release_of_a_different_source_still_refuses_a_covering_non_network_layer() {
        let table = stack(&["autofs", "cifs", "tmpfs"]);
        let calls = std::cell::Cell::new(0);
        let ids = vec![ID_A.to_string()];
        let (res, released) = guard_release(
            &ids,
            ledgered,
            |_| Ok(vec!["//dead/data".to_string()]),
            |_| {
                unwind_stacked(
                    || Ok(table.borrow().clone()),
                    || {
                        calls.set(calls.get() + 1);
                        async { Ok(()) }
                    },
                )
            },
        )
        .await;
        assert_eq!(calls.get(), 0);
        assert!(released.is_empty());
        assert!(res.errors[0].contains("tmpfs"), "{res:?}");
    }

    #[tokio::test]
    async fn adopt_accepts_a_live_source_differing_only_by_host_case_and_trailing_slash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let t = adopt_with(
            path,
            ID_A,
            "/mnt/data",
            "//willow/data",
            |_| Ok(vec!["//WILLOW/data/".to_string()]),
            |_| Ok(true),
        )
        .await
        .unwrap();
        assert_eq!(t, "/mnt/data");
    }

    #[tokio::test]
    async fn release_refuses_everything_when_the_ledger_is_unreadable_or_corrupt() {
        for ledger in [
            Err("Permission denied (os error 13)".to_string()),
            Ok("/mnt/data\n".to_string()),
            Ok(format!("{ID_A}\t/mnt/data/\t//willow/data\n")),
            Ok(format!("{ID_A}\t/mnt/a\th:/a\n{ID_A}\t/mnt/b\th:/b\n")),
            Ok(format!("{ID_A}\t/mnt/a\th:/a\n{ID_B}\t/mnt/a\th:/b\n")),
        ] {
            let (res, released, calls) =
                run_release(ledger.clone(), &[ID_A], "//willow/data").await;
            assert!(calls.is_empty() && released.is_empty(), "{ledger:?}");
            assert_eq!(res.errors.len(), 1, "{ledger:?}: {res:?}");
            assert!(res.errors[0].starts_with(&format!("release {ID_A}: refused: ")));
        }
    }

    #[tokio::test]
    async fn release_is_idempotent_when_nothing_is_mounted_at_the_recorded_target() {
        let ids = vec![ID_A.to_string()];
        let (res, released) = guard_release(
            &ids,
            ledgered,
            |_| Ok(Vec::new()),
            |_| async { panic!("must not umount") },
        )
        .await;
        assert_eq!(released, vec![ID_A]);
        assert_eq!(res.unchanged, vec!["/mnt/data"]);
        assert!(res.errors.is_empty() && res.changed.is_empty(), "{res:?}");
    }

    #[tokio::test]
    async fn ledger_release_drops_unmapped_rows_and_keeps_mapped_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        std::fs::write(
            &path,
            format!("{ID_A}\t/mnt/a\th:/a\n{ID_B}\t/mnt/b\th:/b\n"),
        )
        .unwrap();
        let p = path.to_str().unwrap();
        ledger_release(p, &[ID_A.to_string(), ID_B.to_string()], &keys(&["/mnt/a"]))
            .await
            .unwrap();
        let left = parse_ledger(&read_ledger(p).unwrap()).unwrap();
        assert_eq!(left.keys().collect::<Vec<_>>(), vec![ID_A]);
    }

    #[tokio::test]
    async fn ledger_release_keeps_every_row_when_ledger_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        std::fs::write(&path, format!("{ID_A}\t/mnt/a\th:/a\ngarbage\n")).unwrap();
        let p = path.to_str().unwrap();
        let before = std::fs::read_to_string(p).unwrap();
        assert!(
            ledger_release(p, &[ID_A.to_string()], &keys(&[]))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(p).unwrap(), before);
    }

    #[tokio::test]
    async fn ledger_write_failure_is_not_a_release_failure_and_second_release_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_dir = dir.path().join("ledger");
        std::fs::create_dir(&ledger_dir).unwrap();
        let p = ledger_dir.to_str().unwrap();
        let ids = vec![ID_A.to_string()];
        let (mut res, released) = guard_release(
            &ids,
            ledgered,
            |_| Ok(vec!["//willow/data".to_string()]),
            |_| async { Ok(Unwound::Detached) },
        )
        .await;
        if let Err(e) = ledger_release(p, &released, &keys(&[])).await {
            res.warnings.push(e);
        }
        assert!(res.errors.is_empty(), "{res:?}");
        assert_eq!(res.changed, vec!["/mnt/data"]);
        assert_eq!(res.warnings.len(), 1);
        let (again, released) = guard_release(
            &ids,
            ledgered,
            |_| Ok(Vec::new()),
            |_| async { panic!("must not umount") },
        )
        .await;
        assert!(again.errors.is_empty() && released == vec![ID_A]);
    }

    #[tokio::test]
    async fn release_keeps_the_entry_when_umount_fails() {
        let ids = vec![ID_A.to_string()];
        let (res, released) = guard_release(
            &ids,
            ledgered,
            |_| Ok(vec!["//willow/data".to_string()]),
            |_| async { Err("busy".to_string()) },
        )
        .await;
        assert!(released.is_empty());
        assert_eq!(res.errors, vec![format!("release {ID_A}: busy")]);
    }

    fn stack(layers: &[&str]) -> std::cell::RefCell<Vec<String>> {
        std::cell::RefCell::new(layers.iter().map(|s| s.to_string()).collect())
    }

    async fn run_unwind(
        table: &std::cell::RefCell<Vec<String>>,
        calls: &std::cell::Cell<usize>,
    ) -> Result<Unwound, String> {
        unwind_stacked(
            || Ok(table.borrow().clone()),
            || {
                calls.set(calls.get() + 1);
                table.borrow_mut().pop();
                async { Ok(()) }
            },
        )
        .await
    }

    #[tokio::test]
    async fn unwind_stacked_detaches_cifs_and_keeps_autofs() {
        let table = stack(&["autofs", "cifs"]);
        let calls = std::cell::Cell::new(0);
        let out = run_unwind(&table, &calls).await.unwrap();
        assert_eq!(out, Unwound::Detached);
        assert_eq!(calls.get(), 1);
        assert_eq!(*table.borrow(), ["autofs"]);
    }

    #[tokio::test]
    async fn unwind_stacked_detaches_every_network_layer_over_autofs() {
        let table = stack(&["autofs", "cifs", "nfs4"]);
        let calls = std::cell::Cell::new(0);
        run_unwind(&table, &calls).await.unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(*table.borrow(), ["autofs"]);
    }

    #[tokio::test]
    async fn unwind_stacked_refuses_a_non_network_layer_on_top() {
        let table = stack(&["autofs", "cifs", "tmpfs"]);
        let calls = std::cell::Cell::new(0);
        let err = run_unwind(&table, &calls).await.unwrap_err();
        assert_eq!(calls.get(), 0);
        assert_eq!(*table.borrow(), ["autofs", "cifs", "tmpfs"]);
        assert!(err.contains("tmpfs"), "{err}");
    }

    #[tokio::test]
    async fn unwind_stacked_fully_unwinds_a_pure_cifs_stack() {
        let table = stack(&["cifs", "cifs"]);
        let calls = std::cell::Cell::new(0);
        run_unwind(&table, &calls).await.unwrap();
        assert_eq!(calls.get(), 2);
        assert!(table.borrow().is_empty());
    }

    #[tokio::test]
    async fn unwind_stacked_refuses_without_umount_when_nothing_is_mounted() {
        let table = stack(&[]);
        let calls = std::cell::Cell::new(0);
        let err = run_unwind(&table, &calls).await.unwrap_err();
        assert_eq!(calls.get(), 0);
        assert!(err.contains("nothing mounted"), "{err}");
    }

    #[tokio::test]
    async fn unwind_stacked_leaves_a_lone_autofs_untouched() {
        let table = stack(&["autofs"]);
        let calls = std::cell::Cell::new(0);
        let out = run_unwind(&table, &calls).await.unwrap();
        assert_eq!(out, Unwound::NothingToDetach);
        assert_eq!(calls.get(), 0);
        assert_eq!(*table.borrow(), ["autofs"]);
    }

    #[tokio::test]
    async fn unwind_stacked_reports_detached_for_a_lone_cifs() {
        let table = stack(&["cifs"]);
        let calls = std::cell::Cell::new(0);
        let out = run_unwind(&table, &calls).await.unwrap();
        assert_eq!(out, Unwound::Detached);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn normalize_refuses_control_characters() {
        for t in [
            "/mnt/a\n/srv/x",
            "/mnt/a\r",
            "/mnt/a\tb",
            "/mnt/a\u{7}",
            "/mnt/a\0",
        ] {
            assert!(normalize_unmount_target(t).is_err(), "{t:?}");
        }
    }

    #[tokio::test]
    async fn ledger_write_rejects_unrepresentable_adds_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        for (id, target, source) in [
            (ID_A, "/mnt/a\n/srv/x", "h:/a"),
            (ID_A, "/mnt/b\r", "h:/a"),
            ("not-a-uuid", "/mnt/a", "h:/a"),
            (ID_A, "/mnt/a", "h:/a\nx"),
            (ID_A, "/mnt/a", ""),
        ] {
            let add = [(id.to_string(), led_entry(target, source))];
            assert!(ledger_update(path, &add, &[]).await.is_err(), "{target:?}");
        }
        assert!(!std::path::Path::new(path).exists());
    }

    #[tokio::test]
    async fn ledger_round_trips_escaped_target_and_source_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let e = led_entry("/mnt/a b\\c ", "//h/my share ");
        ledger_update(path, &[(ID_A.to_string(), e.clone())], &[])
            .await
            .unwrap();
        let ledger = parse_ledger(&read_ledger(path).unwrap()).unwrap();
        assert_eq!(ledger, [(ID_A.to_string(), e)].into_iter().collect());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn ledger_add_refreshes_source_for_same_id_and_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let add = |id: &str, t: &str, s: &str| [(id.to_string(), led_entry(t, s))];
        ledger_update(path, &add(ID_A, "/mnt/a", "h:/1"), &[])
            .await
            .unwrap();
        ledger_update(path, &add(ID_A, "/mnt/a/", "h:/2"), &[])
            .await
            .unwrap();
        let ledger = parse_ledger(&read_ledger(path).unwrap()).unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[ID_A], led_entry("/mnt/a", "h:/2"));
    }

    #[tokio::test]
    async fn ledger_refuses_to_rebind_id_or_take_over_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let add = |id: &str, t: &str, s: &str| [(id.to_string(), led_entry(t, s))];
        ledger_update(path, &add(ID_A, "/mnt/a", "h:/1"), &[])
            .await
            .unwrap();
        assert!(
            ledger_update(path, &add(ID_A, "/mnt/b", "h:/1"), &[])
                .await
                .is_err()
        );
        assert!(
            ledger_update(path, &add(ID_B, "/mnt/a/", "h:/3"), &[])
                .await
                .is_err()
        );
        let ledger = parse_ledger(&read_ledger(path).unwrap()).unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[ID_A], led_entry("/mnt/a", "h:/1"));
    }

    fn net_entry(target: &str, source: &str) -> MountEntry {
        MountEntry {
            source: source.into(),
            mountpoint: target.into(),
            fstype: "nfs4".into(),
            options: Vec::new(),
        }
    }

    #[test]
    fn observed_entry_uses_kernel_source_not_caller_value() {
        let table = Ok(vec![net_entry("/mnt/a", "real:/export")]);
        let e = observed_ledger_entry(table, "/mnt/a").unwrap();
        assert_eq!(e, led_entry("/mnt/a", "real:/export"));
    }

    #[test]
    fn observed_entry_warns_when_table_unreadable() {
        let err = std::io::Error::other("boom");
        assert!(observed_ledger_entry(Err(err), "/mnt/a").is_err());
        assert!(observed_ledger_entry(Ok(vec![]), "/mnt/a").is_err());
    }

    #[test]
    fn ledger_entry_that_normalizes_differently_is_corrupt() {
        let a = ID_A;
        for line in [
            format!("{a}\t/a//b\th:/e\n"),
            format!("{a}\t/a/b/\th:/e\n"),
            format!("{a}\t/a/./b\th:/e\n"),
            format!("{a}\trel\th:/e\n"),
            format!("{a}\t/a\\\nb\th:/e\n"),
            format!("{a}\t/a b\th:/e\n"),
            format!("{a}\t/a\n"),
            format!("{a}\t/a\th:/e\textra\n"),
            format!("{a}\t/a\t\n"),
            "/a/b\n".to_string(),
            format!("{}\t/a\th:/e\n", ID_A.to_uppercase()),
            format!("{a}\t/a\th:/e\n{a}\t/b\th:/e\n"),
        ] {
            assert!(parse_ledger(&line).is_err(), "{line:?}");
        }
        assert!(parse_ledger(&format!("{a}\t/a/b\th:/e\n")).is_ok());
    }

    #[tokio::test]
    async fn unmount_of_a_lone_autofs_is_unchanged_and_stays_in_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let add = [(ID_A.to_string(), led_entry("/mnt/thor", "h:/t"))];
        ledger_update(path, &add, &[]).await.unwrap();
        let ledger = read_ledger(path).unwrap();
        let table = stack(&["autofs"]);
        let calls = std::cell::Cell::new(0);
        let ids = vec![ID_A.to_string()];
        let (res, released) = guard_release(
            &ids,
            || Ok(ledger),
            |_| Ok(vec!["h:/t".to_string()]),
            |_| async {
                calls.set(calls.get() + 1);
                unwind_stacked(|| Ok(table.borrow().clone()), || async { Ok(()) }).await
            },
        )
        .await;
        assert_eq!(calls.get(), 1);
        assert!(res.changed.is_empty() && res.errors.is_empty());
        assert_eq!(res.unchanged, vec!["/mnt/thor".to_string()]);
        assert!(released.is_empty());
        ledger_update(path, &[], &released).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{ID_A}\t/mnt/thor\th:/t\n")
        );
    }

    const _: () = assert!(MAX_STACKED_UNMOUNTS == 16);

    #[tokio::test]
    async fn unwind_stacked_is_bounded_when_the_mount_never_clears() {
        let calls = std::cell::Cell::new(0usize);
        let err = unwind_stacked(
            || Ok(vec!["autofs".to_string(), "cifs".to_string()]),
            || {
                calls.set(calls.get() + 1);
                assert!(calls.get() <= 64, "unwind_stacked has no bound");
                async { Ok(()) }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 16);
        assert!(err.contains("still present"), "{err}");
    }

    #[tokio::test]
    async fn unwind_stacked_surfaces_an_umount_failure() {
        let err = unwind_stacked(
            || Ok(vec!["cifs".to_string()]),
            || async { Err("busy".to_string()) },
        )
        .await
        .unwrap_err();
        assert_eq!(err, "busy");
    }

    async fn run_racy(
        reads: Vec<Result<Vec<String>, String>>,
        failing: &[usize],
    ) -> (Result<Unwound, String>, usize) {
        let reads = std::cell::RefCell::new(reads.into_iter());
        let calls = std::cell::Cell::new(0usize);
        let res = unwind_stacked(
            || reads.borrow_mut().next().expect("unexpected table read"),
            || {
                let n = calls.get();
                calls.set(n + 1);
                let r = if failing.contains(&n) {
                    Err("not mounted".to_string())
                } else {
                    Ok(())
                };
                async move { r }
            },
        )
        .await;
        (res, calls.get())
    }

    fn l(layers: &[&str]) -> Result<Vec<String>, String> {
        Ok(layers.iter().map(|s| s.to_string()).collect())
    }

    #[tokio::test]
    async fn unwind_stacked_ok_when_layer_vanishes_after_failed_umount() {
        let (res, calls) = run_racy(vec![l(&["autofs", "cifs"]), l(&["autofs"])], &[0]).await;
        assert_eq!(res, Ok(Unwound::Detached));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn unwind_stacked_returns_umount_error_when_layer_remains() {
        let (res, calls) = run_racy(vec![l(&["cifs"]), l(&["cifs"])], &[0]).await;
        assert_eq!(res, Err("not mounted".to_string()));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn unwind_stacked_refuses_when_covered_after_failed_umount() {
        let (res, calls) = run_racy(vec![l(&["nfs4"]), l(&["nfs4", "tmpfs"])], &[0]).await;
        assert_eq!(
            res,
            Err("network mount is covered by a `tmpfs` layer; refusing to detach it".to_string())
        );
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn unwind_stacked_covered_autofs_stack_after_failed_umount() {
        let (res, calls) = run_racy(
            vec![l(&["autofs", "cifs"]), l(&["autofs", "cifs", "tmpfs"])],
            &[0],
        )
        .await;
        assert_eq!(
            res,
            Err("network mount is covered by a `tmpfs` layer; refusing to detach it".to_string())
        );
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn unwind_stacked_ok_when_only_autofs_remains_after_failed_umount() {
        let (res, calls) = run_racy(vec![l(&["cifs"]), l(&["autofs"])], &[0]).await;
        assert_eq!(res, Ok(Unwound::Detached));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn unwind_stacked_returns_umount_error_when_network_layers_remain_on_top() {
        let (res, calls) = run_racy(vec![l(&["cifs"]), l(&["cifs", "cifs"])], &[0]).await;
        assert_eq!(res, Err("not mounted".to_string()));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn unwind_stacked_ok_when_everything_vanishes_after_failed_umount() {
        let (res, calls) = run_racy(vec![l(&["cifs"]), l(&[])], &[0]).await;
        assert_eq!(res, Ok(Unwound::Detached));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn unwind_stacked_returns_read_error_when_reread_fails() {
        let (res, calls) = run_racy(
            vec![
                l(&["cifs"]),
                Err("cannot read mount table: gone".to_string()),
            ],
            &[0],
        )
        .await;
        assert_eq!(calls, 1);
        assert!(res.unwrap_err().contains("cannot read mount table"));
    }

    #[tokio::test]
    async fn unwind_stacked_never_mounted_errors_without_calling_umount() {
        let (res, calls) = run_racy(vec![l(&[])], &[0]).await;
        assert_eq!(res, Err("nothing mounted at target".to_string()));
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn unwind_stacked_ok_when_second_layer_vanishes_concurrently() {
        let (res, calls) = run_racy(
            vec![
                l(&["autofs", "cifs", "cifs"]),
                l(&["autofs", "cifs"]),
                l(&["autofs"]),
            ],
            &[1],
        )
        .await;
        assert_eq!(res, Ok(Unwound::Detached));
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn unwind_stacked_errors_when_the_mount_table_is_unreadable() {
        let calls = std::cell::Cell::new(0usize);
        let err = unwind_stacked(
            || Err("cannot read mount table: denied".to_string()),
            || {
                calls.set(calls.get() + 1);
                async { Ok(()) }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 0);
        assert!(err.contains("cannot read mount table"), "{err}");
    }

    #[tokio::test]
    async fn unwind_stacked_errors_when_the_table_fails_after_an_umount() {
        let calls = std::cell::Cell::new(0usize);
        let err = unwind_stacked(
            || {
                if calls.get() == 0 {
                    Ok(vec!["cifs".to_string(), "cifs".to_string()])
                } else {
                    Err("cannot read mount table: gone".to_string())
                }
            },
            || {
                calls.set(calls.get() + 1);
                async { Ok(()) }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(err.contains("cannot read mount table"), "{err}");
    }

    #[tokio::test]
    async fn failed_release_keeps_the_ledger_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let add = [(ID_A.to_string(), led_entry("/mnt/thor", "h:/t"))];
        ledger_update(path, &add, &[]).await.unwrap();
        let ledger = read_ledger(path).unwrap();
        let ids = vec![ID_A.to_string()];
        let (res, released) = guard_release(
            &ids,
            || Ok(ledger.clone()),
            |_| Ok(vec!["h:/t".to_string()]),
            |_| async { Err("busy".to_string()) },
        )
        .await;
        assert!(res.changed.is_empty());
        ledger_update(path, &[], &released).await.unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), ledger);
    }

    #[tokio::test]
    async fn release_clears_the_ledger_entry_and_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let add = [
            (ID_A.to_string(), led_entry("/mnt/a", "h:/a")),
            (ID_B.to_string(), led_entry("/mnt/b", "h:/b")),
        ];
        ledger_update(path, &add, &[]).await.unwrap();
        let ledger = read_ledger(path).unwrap();
        let ids = vec![ID_A.to_string()];
        let (res, released) = guard_release(
            &ids,
            || Ok(ledger),
            |_| Ok(vec!["h:/a".to_string()]),
            |_| async { Ok(Unwound::Detached) },
        )
        .await;
        assert_eq!(res.changed, vec!["/mnt/a".to_string()]);
        ledger_update(path, &[], &released).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{ID_B}\t/mnt/b\th:/b\n")
        );
    }

    #[tokio::test]
    async fn release_payload_cannot_write_the_ledger() {
        assert!(!is_allowed_write(LEDGER_FILE));
        let r = execute_privileged(PrivilegedOp::Apply {
            writes: vec![FileWrite {
                path: LEDGER_FILE.into(),
                contents: format!("{ID_A}\t/mnt/evil\th:/e\n"),
                mode: None,
            }],
            keep_secret_files: vec![],
            init: Init::Systemd,
            placements: Vec::new(),
            managed: None,
        })
        .await;
        assert!(r.errors[0].contains("refused non-allowlisted path"));
    }

    #[tokio::test]
    async fn mount_records_a_ledger_entry_keyed_by_placement_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger");
        let path = path.to_str().unwrap();
        let req = crate::mount_exec::MountReq {
            placement_id: ID_A.into(),
            source: "h:/srv".into(),
            target: "/mnt/my dir/".into(),
            fstype: "nfs4".into(),
            options: String::new(),
            secret_file: None,
        };
        let e = LedgerEntry {
            target: req.target.clone(),
            source: req.source.clone(),
        };
        ledger_update(path, &[(req.placement_id.clone(), e)], &[])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{ID_A}\t/mnt/my\\ dir\th:/srv\n")
        );
    }

    #[tokio::test]
    async fn release_unmounts_a_wedged_mount_dropped_from_the_elected_map() {
        let m = mount_at("/mnt/wedged");
        let map = render_map_elected(std::slice::from_ref(&m), &Default::default());
        assert!(!declared_map_keys(&map).contains("/mnt/wedged"));
        let ledger = format!("{ID_A}\t/mnt/wedged\th:/x\n");
        let (res, released, calls) = run_release(Ok(ledger), &[ID_A], "h:/x").await;
        assert_eq!(calls, vec!["/mnt/wedged".to_string()]);
        assert_eq!(res.changed, vec!["/mnt/wedged".to_string()]);
        assert_eq!(released, vec![ID_A.to_string()]);
    }

    fn keys(ks: &[&str]) -> std::collections::HashSet<String> {
        ks.iter().map(|k| k.to_string()).collect()
    }

    fn placement(id: &str, target: &str) -> MapPlacement {
        MapPlacement {
            id: id.into(),
            target: target.into(),
            source: "h:/x".into(),
        }
    }

    #[test]
    fn map_placements_record_the_elected_source_and_skip_unelected_or_idless() {
        let with_id = |name: &str, id: Option<&str>| ManagedMount {
            placement_id: id.map(str::to_string),
            ..mount(name, "h:/x", None)
        };
        let mounts = [
            with_id("a", Some(ID_A)),
            with_id("b", Some(ID_B)),
            with_id("c", None),
        ];
        let elected = [("/mnt/a".to_string(), "e:/a".to_string())].into();
        assert_eq!(
            map_placements(&mounts, Some(&elected)),
            vec![MapPlacement {
                id: ID_A.into(),
                target: "/mnt/a".into(),
                source: "e:/a".into()
            }]
        );
        assert_eq!(map_placements(&mounts, None).len(), 2);
    }

    #[test]
    fn apply_records_only_placements_keyed_in_the_new_map() {
        let (adds, removes, errors) = map_ledger_changes(
            &keys(&[]),
            &keys(&["/mnt/a"]),
            &[placement(ID_A, "/mnt/a"), placement(ID_B, "/etc")],
            &Ledger::new(),
            |_| Ok(vec![]),
        );
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].0, ID_A);
        assert!(removes.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn apply_drops_a_removed_entry_only_when_no_network_mount_is_live() {
        let ledger: Ledger = [
            (ID_A.to_string(), led_entry("/mnt/a", "h:/a")),
            (ID_B.to_string(), led_entry("/mnt/b", "h:/b")),
        ]
        .into();
        let live = |t: &str| {
            Ok(if t == "/mnt/b" {
                vec!["h:/b".into()]
            } else {
                vec![]
            })
        };
        let (_, removes, _) =
            map_ledger_changes(&keys(&["/mnt/a", "/mnt/b"]), &keys(&[]), &[], &ledger, live);
        assert_eq!(removes, vec![ID_A.to_string()]);
        let (_, removes, _) = map_ledger_changes(
            &keys(&["/mnt/a", "/mnt/b"]),
            &keys(&[]),
            &[],
            &ledger,
            |_| Err("unreadable".to_string()),
        );
        assert!(removes.is_empty());
    }

    #[test]
    fn apply_keeps_rows_whose_entry_is_still_in_the_map_or_was_never_mapped() {
        let ledger: Ledger = [
            (ID_A.to_string(), led_entry("/mnt/a", "h:/a")),
            (ID_B.to_string(), led_entry("/mnt/native", "h:/n")),
        ]
        .into();
        let (_, removes, _) =
            map_ledger_changes(&keys(&["/mnt/a"]), &keys(&["/mnt/a"]), &[], &ledger, |_| {
                Ok(vec![])
            });
        assert!(removes.is_empty());
    }

    #[test]
    fn release_keeps_the_row_of_a_target_still_in_the_map() {
        let ledger: Ledger = [
            (ID_A.to_string(), led_entry("/mnt/a", "h:/a")),
            (ID_B.to_string(), led_entry("/mnt/b", "h:/b")),
        ]
        .into();
        let out = unmapped_ids(
            vec![ID_A.to_string(), ID_B.to_string()],
            &ledger,
            &keys(&["/mnt/a"]),
        );
        assert_eq!(out, vec![ID_B.to_string()]);
    }

    // ── PrivilegedOp::Adopt ───────────────────────────────────────────────
    const ADOPT_ID: &str = "0192f5a0-0000-7000-8000-00000000000c";

    fn live(src: &'static str) -> impl Fn(&str) -> Result<Vec<String>, String> {
        move |_| Ok(vec![src.to_string()])
    }

    fn adopt_path(dir: &tempfile::TempDir) -> String {
        dir.path().join("ledger").to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn adopt_records_the_row_when_the_live_source_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        let t = adopt_with(
            &path,
            ADOPT_ID,
            "/mnt/data/",
            "//willow/data",
            live("//WILLOW/data/"),
            |_| Ok(true),
        )
        .await
        .unwrap();
        assert_eq!(t, "/mnt/data");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{ADOPT_ID}\t/mnt/data\t//willow/data\n")
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    fn owned_over(
        map: &'static str,
        table: Vec<MountEntry>,
    ) -> impl Fn(&str) -> Result<bool, String> {
        move |t| Ok(orca_owned_in(&declared_map_keys(map), &table, t))
    }

    fn opt_entry(fstype: &str, source: &str, opts: &[&str]) -> MountEntry {
        MountEntry {
            options: opts.iter().map(|o| o.to_string()).collect(),
            ..entry("/mnt/data", fstype, source)
        }
    }

    async fn adopt_over(map: &'static str, table: Vec<MountEntry>) -> Result<String, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        let t = table.clone();
        adopt_with(
            &path,
            ADOPT_ID,
            "/mnt/data",
            "h:/x",
            move |tg| Ok(network_sources_in(t.clone(), tg)),
            owned_over(map, table),
        )
        .await
    }

    #[tokio::test]
    async fn adopt_accepts_a_map_keyed_target() {
        let t = vec![opt_entry("nfs4", "h:/x", &["rw"])];
        assert!(adopt_over("/mnt/data -fstype=nfs h:/x\n", t).await.is_ok());
    }

    #[tokio::test]
    async fn adopt_accepts_a_cifs_mount_with_orca_credentials() {
        let t = vec![opt_entry("cifs", "h:/x", &["rw", "username=orca"])];
        assert!(adopt_over("", t).await.is_ok());
    }

    #[tokio::test]
    async fn adopt_refuses_an_unrelated_nfs_mount() {
        let t = vec![opt_entry("nfs4", "h:/x", &["rw"])];
        let e = adopt_over("/mnt/other -fstype=nfs h:/y\n", t)
            .await
            .unwrap_err();
        assert!(e.contains("orca-managed"), "{e}");
    }

    #[tokio::test]
    async fn adopt_refuses_a_cifs_mount_with_another_username() {
        let t = vec![opt_entry("cifs", "h:/x", &["username=admin"])];
        let e = adopt_over("", t).await.unwrap_err();
        assert!(e.contains("orca-managed"), "{e}");
    }

    #[tokio::test]
    async fn adopt_refuses_an_orca_cifs_layer_stacked_under_a_foreign_one() {
        let t = vec![
            opt_entry("cifs", "h:/x", &["username=orca"]),
            opt_entry("nfs4", "h:/x", &["rw"]),
        ];
        assert!(adopt_over("", t).await.is_err());
    }

    #[tokio::test]
    async fn adopt_refuses_a_source_mismatch_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        let e = adopt_with(
            &path,
            ADOPT_ID,
            "/mnt/data",
            "//willow/data",
            live("//other/data"),
            |_| Ok(true),
        )
        .await
        .unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert!(!std::path::Path::new(&path).exists());
    }

    #[tokio::test]
    async fn adopt_refuses_when_no_network_layer_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        let e = adopt_with(
            &path,
            ADOPT_ID,
            "/mnt/data",
            "h:/x",
            |_| Ok(Vec::new()),
            |_| Ok(true),
        )
        .await
        .unwrap_err();
        assert!(e.contains("no network mount"), "{e}");
    }

    #[tokio::test]
    async fn adopt_refuses_an_autofs_only_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        let table = || vec![entry("/mnt/data", "autofs", "/etc/auto.orca")];
        let e = adopt_with(
            &path,
            ADOPT_ID,
            "/mnt/data",
            "/etc/auto.orca",
            |t| Ok(network_sources_in(table(), t)),
            |_| Ok(true),
        )
        .await
        .unwrap_err();
        assert!(e.contains("no network mount"), "{e}");
    }

    #[tokio::test]
    async fn adopt_refuses_a_noncanonical_id_or_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        let upper = ADOPT_ID.to_uppercase();
        assert!(
            adopt_with(&path, &upper, "/mnt/data", "h:/x", live("h:/x"), |_| Ok(
                true
            ))
            .await
            .is_err()
        );
        assert!(
            adopt_with(
                &path,
                ADOPT_ID,
                "/mnt/../etc",
                "h:/x",
                live("h:/x"),
                |_| Ok(true)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn adopt_refuses_an_existing_id_or_target_without_replacing() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        let add = [(ID_A.to_string(), led_entry("/mnt/a", "h:/a"))];
        ledger_update(&path, &add, &[]).await.unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let e = adopt_with(&path, ID_A, "/mnt/b", "h:/b", live("h:/b"), |_| Ok(true))
            .await
            .unwrap_err();
        assert!(e.contains("id is already recorded"), "{e}");
        let e = adopt_with(&path, ADOPT_ID, "/mnt/a/", "h:/a", live("h:/a"), |_| {
            Ok(true)
        })
        .await
        .unwrap_err();
        assert!(e.contains("target is already recorded"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn adopt_fails_closed_on_a_corrupt_or_unreadable_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        std::fs::write(&path, "garbage line\n").unwrap();
        let e = adopt_with(&path, ADOPT_ID, "/mnt/data", "h:/x", live("h:/x"), |_| {
            Ok(true)
        })
        .await
        .unwrap_err();
        assert!(e.contains("corrupt"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "garbage line\n");

        let unreadable = dir.path().join("as-dir");
        std::fs::create_dir(&unreadable).unwrap();
        let e = adopt_with(
            unreadable.to_str().unwrap(),
            ADOPT_ID,
            "/mnt/data",
            "h:/x",
            live("h:/x"),
            |_| Ok(true),
        )
        .await
        .unwrap_err();
        assert!(e.contains("unreadable"), "{e}");
    }

    #[tokio::test]
    async fn an_adopted_id_can_then_be_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = adopt_path(&dir);
        adopt_with(
            &path,
            ADOPT_ID,
            "/mnt/data",
            "//willow/data",
            live("//willow/data"),
            |_| Ok(true),
        )
        .await
        .unwrap();
        let ledger = read_ledger(&path).unwrap();
        let ids = vec![ADOPT_ID.to_string()];
        let (res, released) = guard_release(
            &ids,
            || Ok(ledger),
            live("//willow/data"),
            |_| async { Ok(Unwound::Detached) },
        )
        .await;
        assert_eq!(res.changed, vec!["/mnt/data".to_string()]);
        assert_eq!(released, vec![ADOPT_ID.to_string()]);
        assert!(res.errors.is_empty(), "{res:?}");
    }
}
