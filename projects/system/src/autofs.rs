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
//! needs root, so it routes through the same seam ([`PrivilegedOp::Unmount`]).
//!
//! The pure builders ([`render_map`], [`master_line`], [`merge_master`],
//! [`map_line`], [`autofs_options`]) unit-test without touching the host.

use crate::managed_mounts::{ManagedMount, ordered_sources};
use crate::source_election::{Election, RemountAggression, Transition, elect, transition};
use plugin_toolkit::storage::{
    Health, MountEntry, mount_table, mount_table_of, probe_health, probe_source,
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

/// The privileged operation handed to `orca admin storage-apply` over stdin.
/// A closed, validated vocabulary — the helper does exactly these and nothing
/// else, so the `sudo` grant is a narrow, auditable surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PrivilegedOp {
    /// Write the given config files, then restart autofs via `init`. The daemon
    /// only emits this when at least one file actually differs from disk.
    ///
    /// `keep_secret_files` is the authoritative set of secret-file paths that
    /// should exist after this apply (every currently-declared inline-secret mount).
    /// The root helper reaps any secret-file in `SECRET_FILE_DIR` NOT in this set —
    /// the teardown path for a deleted mount or one whose secret changed. It is
    /// distinct from `writes` because an unchanged secret-file is not rewritten but
    /// must still be kept.
    Apply {
        writes: Vec<FileWrite>,
        #[serde(default)]
        keep_secret_files: Vec<String>,
        init: Init,
    },
    /// Force-release wedged mounts (`umount -lf`) so autofs can remount + fail
    /// over. Used by the self-heal path.
    Unmount { targets: Vec<String> },
    /// Realize mounts natively via `mount(8)` — the autofs-free apply path. Each
    /// [`MountReq`] is already rendered (source elected, options rendered by the
    /// owning backend); the root helper just runs the mount. This is the
    /// convergence loop's "ensure present" primitive.
    ///
    /// `keep_secret_files` is the authoritative set of secret-file paths that
    /// should exist after this apply (every currently-declared inline-secret mount's
    /// secret-file). The root helper reaps any secret-file in `SECRET_FILE_DIR` NOT
    /// in this set — the generic teardown for a deleted mount or one whose secret
    /// changed, mirroring `Apply`'s `keep_secret_files`. Core reaps by path validity,
    /// never by the file's grammar.
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
}

/// Result the helper prints back to the daemon as JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrivilegedResult {
    /// Files actually written (for `Apply`).
    pub changed: Vec<String>,
    /// Whether autofs was restarted.
    pub restarted: bool,
    /// Non-fatal errors — collected, not thrown, so one bad step doesn't abort.
    pub errors: Vec<String>,
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
    format!("{}  {}  {}", m.target, opts, locations)
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
    format!("{}  {}  {}", m.target, opts, source)
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
fn master_mountpoint(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    t.split_whitespace().next()
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
            && managed_targets.iter().any(|t| is_ancestor_or_equal(mp, t))
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
/// host yields empty `writes` — an idempotent no-op).
pub async fn plan(mounts: &[ManagedMount]) -> PrivilegedOp {
    plan_with_map(mounts, render_map(mounts)).await
}

/// [`plan`] against a pre-rendered map body. Shared by the legacy all-sources
/// path ([`render_map`]) and the elected single-source path
/// ([`render_map_elected`]); both need the same master take-over-merge + diff.
async fn plan_with_map(mounts: &[ManagedMount], map: String) -> PrivilegedOp {
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
    }
}

/// Render + plan + apply for a mount set. Idempotent: an unchanged host makes no
/// privileged call at all.
pub async fn apply(mounts: &[ManagedMount]) -> ApplyOutcome {
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
    apply_op(plan_with_map(mounts, render_map_elected(mounts, elected)).await).await
}

/// Run a planned [`PrivilegedOp::Apply`], short-circuiting an empty diff.
async fn apply_op(op: PrivilegedOp) -> ApplyOutcome {
    match op {
        PrivilegedOp::Apply { ref writes, .. } if writes.is_empty() => ApplyOutcome::default(),
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
    })
    .await
}

/// Bridge to the root helper: spawn `sudo -n <self> admin storage-apply` and
/// pipe the op as JSON on stdin, returning the parsed [`PrivilegedResult`]. A
/// spawn/parse failure (e.g. no sudoers grant) surfaces as an error in the
/// result rather than a panic.
pub async fn run_privileged(op: &PrivilegedOp) -> PrivilegedResult {
    use tokio::io::AsyncWriteExt;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return PrivilegedResult {
                errors: vec![format!("resolve current exe: {e}")],
                ..Default::default()
            };
        }
    };
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => {
            return PrivilegedResult {
                errors: vec![format!("serialize op: {e}")],
                ..Default::default()
            };
        }
    };

    let mut child = match Command::new("sudo")
        .arg("-n")
        .arg(&exe)
        .args(["admin", "storage-apply"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return PrivilegedResult {
                errors: vec![format!("spawn sudo helper: {e}")],
                ..Default::default()
            };
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _written = stdin.write_all(&payload).await;
        let _shut = stdin.shutdown().await;
    }

    match child.wait_with_output().await {
        Ok(out) if out.status.success() => {
            serde_json::from_slice(&out.stdout).unwrap_or_else(|e| PrivilegedResult {
                errors: vec![format!(
                    "parse helper output: {e}: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                )],
                ..Default::default()
            })
        }
        Ok(out) => PrivilegedResult {
            errors: vec![format!(
                "helper exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )],
            ..Default::default()
        },
        Err(e) => PrivilegedResult {
            errors: vec![format!("run sudo helper: {e}")],
            ..Default::default()
        },
    }
}

// ── Root side: the privileged executor (runs inside `orca admin storage-apply`) ─

/// Execute a validated [`PrivilegedOp`] as root. Called only from the
/// `admin storage-apply` CLI path (via `sudo`). Validates every write path,
/// writes atomically (temp + rename), and restarts autofs.
pub async fn execute_privileged(op: PrivilegedOp) -> PrivilegedResult {
    match op {
        PrivilegedOp::Apply {
            writes,
            keep_secret_files,
            init,
        } => {
            let mut res = PrivilegedResult::default();
            for w in &writes {
                if !is_allowed_write(&w.path) {
                    res.errors
                        .push(format!("refused non-allowlisted path: {}", w.path));
                    continue;
                }
                match write_atomic(&w.path, &w.contents, w.mode).await {
                    Ok(()) => res.changed.push(w.path.clone()),
                    Err(e) => res.errors.push(format!("write {}: {e}", w.path)),
                }
            }
            // Teardown: prune secret-files not in the authoritative keep-set. When
            // a mount is deleted or its secret changes target, its stale secret-file
            // is removed so a resolved secret never lingers on disk. Scoped to
            // `SECRET_FILE_DIR`; foreign files there are left alone.
            reap_orphan_secret_files(&keep_secret_files, &mut res).await;
            if !res.changed.is_empty() {
                match restart_autofs(init).await {
                    Ok(()) => res.restarted = true,
                    Err(e) => res.errors.push(format!("restart autofs: {e}")),
                }
            }
            res
        }
        PrivilegedOp::Unmount { targets } => {
            let mut res = PrivilegedResult::default();
            for t in &targets {
                match force_unmount(t).await {
                    Ok(()) => res.changed.push(t.clone()),
                    Err(e) => res.errors.push(format!("release {t}: {e}")),
                }
            }
            res
        }
        PrivilegedOp::Mount {
            mounts,
            keep_secret_files,
        } => {
            let mut res = PrivilegedResult::default();
            for m in &mounts {
                match crate::mount_exec::run_mount(m).await {
                    Ok(()) => res.changed.push(m.target.clone()),
                    Err(e) => res.errors.push(format!("mount {}: {e}", m.target)),
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
    }
}

/// Atomic write: create the parent dir, write a sibling temp file, then rename
/// over the target so a reader never sees a half-written map. When `mode` is set
/// (a secret-file), the mode is applied to the **temp file before rename**
/// so the file is never visible at a laxer mode — there is no window in which the
/// secret is world-readable.
async fn write_atomic(path: &str, contents: &str, mode: Option<u32>) -> std::io::Result<()> {
    let p = Path::new(path);
    if let Some(dir) = p.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = format!("{path}.orca.tmp");
    tokio::fs::write(&tmp, contents).await?;
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m)).await?;
    }
    tokio::fs::rename(&tmp, path).await
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
    let mut errors = Vec::new();
    for t in targets {
        if let Err(e) = Command::new("stat").arg("--").arg(t).output().await {
            errors.push(format!("trigger {t}: {e}"));
        }
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
pub async fn probe(target: &str, health_timeout: Duration) -> Health {
    let target = target.to_string();
    tokio::task::spawn_blocking(move || probe_health(&target, health_timeout))
        .await
        .unwrap_or(Health::Error)
}

/// Probe every target and return those that need recovery — stale, hung
/// (`Timeout`), or not-mounted (`Missing`). Healthy and indeterminate (`Error`)
/// targets are omitted (never act on an ambiguous probe). This is the probe-only
/// half the self-heal loop calls each tick *without* acting.
pub async fn probe_stale(targets: &[String], health_timeout: Duration) -> Vec<String> {
    // Read the kernel mount table once so we can catch the false-positive
    // `probe_health` misses: a managed mountpoint that exists as a bare directory
    // with NOTHING mounted through it (no autofs trigger, no real fs). `stat`
    // succeeds on the empty dir, so `probe_health` returns `Health::Ok` and the
    // mount looks healthy — but autofs never loaded the direct-map entry (the
    // freyr / frigg "map written but not reloaded" case). The only reliable
    // signal is the mount table itself. See [[project-orca-nfs-automaster-defect]].
    let table = mount_table().unwrap_or_default();
    let mut stale = Vec::new();
    for target in targets {
        let needs_recovery = matches!(
            probe(target, health_timeout).await,
            Health::Stale | Health::Timeout | Health::Missing
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
/// error is treated as **busy** (fail safe: never disrupt on uncertainty).
pub(crate) async fn is_busy(target: &str) -> bool {
    match Command::new("fuser")
        .args(["-sm", "--", target])
        .output()
        .await
    {
        // `fuser -s` exits 0 when *something* holds the path, 1 when nothing does.
        Ok(out) => out.status.success(),
        Err(_) => true,
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

/// Clean remount of a not-busy target: lazy-detach the current mount so the next
/// access re-triggers autofs against the freshly-elected single-source map, then
/// re-access to bring it up now. Routes the unmount through the privileged seam.
async fn remount_to_elected(target: &str, _probe_timeout: Duration) -> Vec<String> {
    let mut errors = Vec::new();
    let r = run_privileged(&PrivilegedOp::Unmount {
        targets: vec![target.to_string()],
    })
    .await;
    errors.extend(r.errors);
    errors.extend(trigger(std::slice::from_ref(&target.to_string())).await);
    errors
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
/// Each rung re-probes and returns as soon as the target is `Health::Ok`.
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
    let missing_or_no_entry = matches!(probe(target, health_timeout).await, Health::Missing)
        || target_has_no_mount(target).await;
    if allow_reload && missing_or_no_entry {
        errors.extend(force_reload().await);
        errors.extend(trigger(&one(target)).await);
        if matches!(probe(target, health_timeout).await, Health::Ok)
            && !target_has_no_mount(target).await
        {
            return (true, errors);
        }
    }

    // Rung 2: lazy-unmount the (possibly wedged) handle + retrigger for failover.
    let r = run_privileged(&PrivilegedOp::Unmount {
        targets: one(target),
    })
    .await;
    errors.extend(r.errors);
    errors.extend(trigger(&one(target)).await);
    if matches!(probe(target, health_timeout).await, Health::Ok) {
        return (true, errors);
    }

    // Rung 3: last escalation — force a reload and retrigger once more. Still gated
    // on source reachability to avoid restarting autofs against a dead server.
    if allow_reload {
        errors.extend(force_reload().await);
        errors.extend(trigger(&one(target)).await);
    }
    let recovered = matches!(probe(target, health_timeout).await, Health::Ok);
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
        match probe(target, health_timeout).await {
            Health::Ok => out.healthy.push(target.clone()),
            Health::Error => out.errors.push(format!(
                "probe {target}: indeterminate error, left untouched"
            )),
            Health::Stale | Health::Timeout | Health::Missing => {
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

/// `umount -lf <target>` — lazy, forced detach of a wedged mount. Runs root-side
/// inside the helper. A non-zero exit (e.g. "not mounted") surfaces as an error
/// the caller collects but does not treat as fatal.
async fn force_unmount(target: &str) -> Result<(), String> {
    let out = Command::new("umount")
        .args(["-lf", "--", target])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn target_absent_when_no_mount_table_entry() {
        // The frigg /mnt/data regression: the mountpoint is a bare dir (stat OK,
        // probe_health => Ok) but nothing is mounted there — no autofs trigger and
        // no nfs. It must be classified absent so self-heal reloads the map.
        let table = vec![
            entry("/mnt/backups", "autofs", "/etc/auto.orca"),
            entry("/mnt/backups", "nfs4", "10.10.10.10:/mnt/user/backups"),
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
        let table = vec![entry("/mnt/data", "nfs4", "10.10.10.10:/mnt/user/data")];
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

    #[test]
    fn privileged_op_roundtrips_json() {
        let op = PrivilegedOp::Apply {
            writes: vec![FileWrite {
                path: MAP_FILE.into(),
                contents: "x".into(),
                mode: None,
            }],
            keep_secret_files: Vec::new(),
            init: Init::OpenRc,
        };
        let s = serde_json::to_string(&op).unwrap();
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
    }

    #[test]
    fn reload_op_roundtrips_json() {
        for init in [Init::Systemd, Init::OpenRc] {
            let op = PrivilegedOp::Reload { init };
            let s = serde_json::to_string(&op).unwrap();
            assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
        }
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

    // ── master_mountpoint ─────────────────────────────────────────────────

    #[test]
    fn master_mountpoint_extracts_first_field() {
        assert_eq!(master_mountpoint("/misc\t/etc/auto.misc"), Some("/misc"));
        assert_eq!(
            master_mountpoint("  /mnt/pool  /etc/auto.pool --ghost"),
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

    #[test]
    fn unmount_op_roundtrips_json() {
        let op = PrivilegedOp::Unmount {
            targets: vec!["/mnt/a".into(), "/mnt/b".into()],
        };
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("\"op\":\"unmount\""));
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
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
            } => {
                assert_eq!(writes.len(), 1);
                assert!(keep_secret_files.is_empty(), "defaulted to empty");
                assert_eq!(init, Init::Systemd);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    // ── PrivilegedOp::Mount serde roundtrip ───────────────────────────────

    #[test]
    fn mount_op_roundtrips_json_with_defaulted_keep_set() {
        let op = PrivilegedOp::Mount {
            mounts: vec![crate::mount_exec::MountReq {
                source: "primary:/srv/data".into(),
                target: "/mnt/data".into(),
                fstype: "nfs4".into(),
                options: "vers=4.2,hard".into(),
                secret_file: None,
            }],
            keep_secret_files: Vec::new(),
        };
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("\"op\":\"mount\""));
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
    }

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

    // ── PrivilegedResult serde with populated fields ──────────────────────

    #[test]
    fn privileged_result_roundtrips_with_errors_and_changes() {
        let r = PrivilegedResult {
            changed: vec!["/mnt/a".into(), "/mnt/b".into()],
            restarted: true,
            errors: vec!["mount /mnt/c: boom".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"restarted\":true"));
        assert!(s.contains("boom"));
        let back: PrivilegedResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.changed, r.changed);
        assert_eq!(back.errors, r.errors);
        assert!(back.restarted);
    }

    // ── PrivilegedOp::Mount roundtrip carrying a secret_file ──────────────

    #[test]
    fn mount_op_roundtrips_with_secret_file_and_keep_set() {
        let op = PrivilegedOp::Mount {
            mounts: vec![crate::mount_exec::MountReq {
                source: "//host/share".into(),
                target: "/mnt/media".into(),
                fstype: "cifs".into(),
                options: "rw,vers=3.0".into(),
                secret_file: Some(crate::mount_exec::SecretFile {
                    path: "/etc/orca/secret-files/mnt_media.secret".into(),
                    contents: "username=svc\npassword=p\n".into(),
                }),
            }],
            keep_secret_files: vec!["/etc/orca/secret-files/mnt_media.secret".into()],
        };
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("\"op\":\"mount\""));
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
    }

    // ── Apply op serde carries keep_secret_files when populated ───────────

    #[test]
    fn apply_op_serializes_keep_secret_files_when_present() {
        let op = PrivilegedOp::Apply {
            writes: Vec::new(),
            keep_secret_files: vec!["/etc/orca/secret-files/mnt_x.secret".into()],
            init: Init::Systemd,
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

    // ── execute_privileged: Unmount arm records a per-target failure ───────
    #[tokio::test]
    async fn execute_privileged_unmount_records_error_for_unmounted_target() {
        let op = PrivilegedOp::Unmount {
            targets: vec!["/orca/definitely/not/mounted/xyz".into()],
        };
        let res = execute_privileged(op).await;
        assert!(res.changed.is_empty(), "nothing was actually released");
        assert!(!res.restarted, "Unmount never restarts autofs");
        assert_eq!(res.errors.len(), 1, "one release error collected: {res:?}");
        assert!(res.errors[0].contains("release /orca/definitely/not/mounted/xyz"));
    }

    // ── execute_privileged: Mount arm rejects a non-allowlisted secret-file ─
    #[tokio::test]
    async fn execute_privileged_mount_records_secret_file_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("media").to_str().unwrap().to_string();
        let op = PrivilegedOp::Mount {
            mounts: vec![crate::mount_exec::MountReq {
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

    // ── probe: a bogus path yields a concrete Health (no panic across the seam) ─
    #[tokio::test]
    async fn probe_returns_health_for_bogus_path() {
        let h = probe("/orca/not/mounted/probe", Duration::from_millis(150)).await;
        assert_ne!(h, Health::Ok, "unmounted path must not probe healthy");
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
        // A real, existing directory stats clean → `probe_health` => Health::Ok,
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
}
