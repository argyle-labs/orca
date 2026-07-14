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
//! `hard` mount — is handled by [`recover`] (the `storage.recover` tool) and the
//! per-host loop in [`crate::storage_selfheal`]; its `umount -lf` also needs
//! root, so it routes through the same seam ([`PrivilegedOp::Unmount`]).
//!
//! The pure builders ([`render_map`], [`master_line`], [`merge_master`],
//! [`map_line`], [`autofs_options`]) unit-test without touching the host.

use crate::managed_mounts::{ManagedMount, ordered_sources};
use crate::source_election::{Election, RemountAggression, Transition, elect, transition};
use plugin_toolkit::storage::{Health, mount_table_of, probe_health, probe_source};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// Network filesystem types orca elects/reads sources for. Backend-agnostic:
/// NFS today, SMB (`cifs`/`smbfs`) next — the same election path serves both.
const NET_FSTYPES: &[&str] = &["nfs4", "nfs", "cifs", "smbfs"];

/// The direct map file. One line per managed mount, keyed by absolute target.
pub const MAP_FILE: &str = "/etc/auto.orca";
/// Idle-unmount timeout (seconds) autofs applies to our mounts. Short enough
/// that an idle share unmounts and re-probes (auto-failover on next access),
/// long enough not to churn actively-used mounts.
const TIMEOUT_SECS: u32 = 60;

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
}

/// The privileged operation handed to `orca admin storage-apply` over stdin.
/// A closed, validated vocabulary — the helper does exactly these and nothing
/// else, so the `sudo` grant is a narrow, auditable surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PrivilegedOp {
    /// Write the given config files, then restart autofs via `init`. The daemon
    /// only emits this when at least one file actually differs from disk.
    Apply { writes: Vec<FileWrite>, init: Init },
    /// Force-release wedged mounts (`umount -lf`) so autofs can remount + fail
    /// over. Used by the self-heal path.
    Unmount { targets: Vec<String> },
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
    let opts = autofs_options(&m.fstype, m.options.as_deref());
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
    let opts = autofs_options(&m.fstype, m.options.as_deref());
    format!("{}  {}  {}", m.target, opts, source)
}

/// Build the autofs `-fstype=…,opt,opt` option string. fstab/systemd-only
/// options (`_netdev`, `nofail`, `x-systemd.*`, `auto`/`noauto`) are dropped —
/// meaningless to autofs and would make the map entry invalid.
fn autofs_options(fstype: &str, options: Option<&str>) -> String {
    let mut parts = vec![format!("fstype={fstype}")];
    if let Some(opts) = options {
        parts.extend(
            opts.split(',')
                .map(str::trim)
                .filter(|o| !o.is_empty() && !is_fstab_only(o))
                .map(str::to_string),
        );
    }
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
    path == MAP_FILE || path == "/etc/auto.master" || path == "/etc/autofs/auto.master"
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
            });
        }
    }

    PrivilegedOp::Apply {
        writes,
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
        PrivilegedOp::Apply { writes, .. } if writes.is_empty() => ApplyOutcome::default(),
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
        PrivilegedOp::Apply { writes, init } => {
            let mut res = PrivilegedResult::default();
            for w in &writes {
                if !is_allowed_write(&w.path) {
                    res.errors
                        .push(format!("refused non-allowlisted path: {}", w.path));
                    continue;
                }
                match write_atomic(&w.path, &w.contents).await {
                    Ok(()) => res.changed.push(w.path.clone()),
                    Err(e) => res.errors.push(format!("write {}: {e}", w.path)),
                }
            }
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
                if let Err(e) = force_unmount(t).await {
                    res.errors.push(format!("release {t}: {e}"));
                }
            }
            res
        }
    }
}

/// Atomic write: create the parent dir, write a sibling temp file, then rename
/// over the target so a reader never sees a half-written map.
async fn write_atomic(path: &str, contents: &str) -> std::io::Result<()> {
    let p = Path::new(path);
    if let Some(dir) = p.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = format!("{path}.orca.tmp");
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::rename(&tmp, path).await
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
    let mut stale = Vec::new();
    for target in targets {
        if matches!(
            probe(target, health_timeout).await,
            Health::Stale | Health::Timeout | Health::Missing
        ) {
            stale.push(target.clone());
        }
    }
    stale
}

// ── Source-liveness election + failback (the autofs-can't-do-it core) ──────────

/// The source currently mounted at `target`, read from the **live kernel mount
/// table** filtered to network filesystem types — not a `stat` on the autofs
/// trigger dir, which is a false positive (the trigger dir exists whether or not
/// anything is mounted through it). `None` means nothing network-shaped is
/// mounted there right now. Runtime-agnostic; offloaded to the blocking pool.
pub async fn current_source_for_target(target: &str) -> Option<String> {
    let target = target.to_string();
    tokio::task::spawn_blocking(move || {
        mount_table_of(NET_FSTYPES)
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
async fn is_busy(target: &str) -> bool {
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

/// Recover one confirmed-stale target: force-release the wedged handle
/// (privileged `umount -lf` via the helper) then re-access so autofs remounts
/// and fails over to the next ordered source. Returns `(recovered, errors)`.
pub async fn force_and_retrigger(target: &str, health_timeout: Duration) -> (bool, Vec<String>) {
    let mut errors = Vec::new();
    let r = run_privileged(&PrivilegedOp::Unmount {
        targets: vec![target.to_string()],
    })
    .await;
    errors.extend(r.errors);
    errors.extend(trigger(std::slice::from_ref(&target.to_string())).await);
    let recovered = matches!(probe(target, health_timeout).await, Health::Ok);
    (recovered, errors)
}

/// Self-heal the one failure mode autofs can't recover on its own: an
/// actively-held **stale** `hard` mount that never idles out. Probes each target
/// and immediately recovers any that are stale/hung/not-mounted. This is the
/// *manual* / on-demand path (the `storage.recover` tool) — it acts on the first
/// stale probe. The automated per-host loop instead confirms across several
/// ticks before acting (see [`crate::storage_selfheal`]).
pub async fn recover(targets: &[String], health_timeout: Duration) -> RecoverOutcome {
    let mut out = RecoverOutcome::default();

    for target in targets {
        match probe(target, health_timeout).await {
            Health::Ok => out.healthy.push(target.clone()),
            Health::Error => out.errors.push(format!(
                "probe {target}: indeterminate error, left untouched"
            )),
            Health::Stale | Health::Timeout | Health::Missing => {
                let (recovered, errs) = force_and_retrigger(target, health_timeout).await;
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
            addresses: Vec::new(),
            enabled: true,
        }
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
            }],
            init: Init::OpenRc,
        };
        let s = serde_json::to_string(&op).unwrap();
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
    }

    // ── autofs_options ────────────────────────────────────────────────────

    #[test]
    fn autofs_options_bare_fstype_when_no_options() {
        assert_eq!(autofs_options("nfs4", None), "-fstype=nfs4");
    }

    #[test]
    fn autofs_options_empty_string_options_yields_bare_fstype() {
        assert_eq!(autofs_options("nfs4", Some("")), "-fstype=nfs4");
    }

    #[test]
    fn autofs_options_keeps_real_opts_and_drops_fstab_only() {
        assert_eq!(
            autofs_options(
                "nfs4",
                Some("_netdev,nofail,x-systemd.automount,vers=4.2,hard,nconnect=4,noauto,auto")
            ),
            "-fstype=nfs4,vers=4.2,hard,nconnect=4"
        );
    }

    #[test]
    fn autofs_options_trims_whitespace_around_opts() {
        assert_eq!(
            autofs_options("cifs", Some(" ro , vers=3.0 ")),
            "-fstype=cifs,ro,vers=3.0"
        );
    }

    #[test]
    fn autofs_options_drops_empty_segments_from_double_commas() {
        assert_eq!(autofs_options("nfs", Some("ro,,rw")), "-fstype=nfs,ro,rw");
    }

    #[test]
    fn autofs_options_drops_comment_option() {
        assert_eq!(
            autofs_options("nfs", Some("comment=x-gvfs-show,ro")),
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
        write_atomic(&path, "body\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "body\n");
        assert!(!std::path::Path::new(&format!("{path}.orca.tmp")).exists());
    }

    #[tokio::test]
    async fn write_atomic_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("f");
        std::fs::write(&target, "old").unwrap();
        write_atomic(target.to_str().unwrap(), "new").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    // ── execute_privileged: allowlist refusal (no root/restart needed) ────

    #[tokio::test]
    async fn execute_privileged_refuses_non_allowlisted_path() {
        let op = PrivilegedOp::Apply {
            writes: vec![FileWrite {
                path: "/etc/passwd".into(),
                contents: "x".into(),
            }],
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
}
