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
use plugin_toolkit::storage::{Health, probe_health};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

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
    let master_path = detect_master_file();
    let map = render_map(mounts);
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
    match plan(mounts).await {
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
}
