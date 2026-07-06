//! autofs renderer — turns the declarative `managed_mounts` store into an
//! autofs *direct map* so autofs owns the mount mechanics we would otherwise
//! hand-build: on-demand mounting, idle unmount, and — the reason this exists —
//! **replicated-server failover** across a mount's ordered sources
//! (primary → secondary). A map entry with multiple locations lets autofs probe
//! and pick a live server itself.
//!
//! orca's job is deliberately small: render the map from the store, write it
//! idempotently (which doubles as drift detection), and reload autofs. The one
//! failure mode autofs does *not* self-heal — an actively-held stale `hard`
//! mount that never idles out — is covered by the storage `recover_stale` loop,
//! not here.
//!
//! Everything above [`apply`] is pure string rendering so it unit-tests without
//! touching the host; [`apply`]/[`trigger`] are the only parts that do I/O.

use crate::managed_mounts::{ManagedMount, ordered_sources};
use tokio::process::Command;

/// autofs master drop-in that registers our direct map. A direct map is keyed
/// by absolute mountpoint, which is what `managed_mounts.target` gives us.
pub const MASTER_DROPIN: &str = "/etc/auto.master.d/orca.autofs";
/// The direct map file the drop-in points at. One line per managed mount.
pub const MAP_FILE: &str = "/etc/auto.orca";
/// Idle-unmount timeout (seconds) autofs applies to our mounts. Short enough
/// that an idle share unmounts and re-probes (auto-failover on next access),
/// long enough not to churn actively-used mounts.
const TIMEOUT_SECS: u32 = 60;

const HEADER: &str =
    "# managed by orca — do not edit; source of truth is the managed_mounts store\n";

/// Rendered autofs configuration: the two files that together declare every
/// managed network-share mount to autofs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutofsConfig {
    /// Contents of [`MASTER_DROPIN`].
    pub master: String,
    /// Contents of [`MAP_FILE`].
    pub map: String,
}

/// Render the autofs config for every enabled network-share mount. Non-network
/// mounts (disk/object) are ignored — autofs only manages network shares. Rows
/// are emitted sorted by target so the output is stable (byte-identical across
/// runs when the store hasn't changed), which is what makes the write in
/// [`apply`] a reliable drift check.
pub fn render(mounts: &[ManagedMount]) -> AutofsConfig {
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

    let master = format!("{HEADER}/-  {MAP_FILE} --timeout={TIMEOUT_SECS}\n");
    AutofsConfig { master, map }
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

/// Build the autofs `-fstype=…,opt,opt` option string from a mount's fstype and
/// its comma-joined mount options. fstab/systemd-only options (`_netdev`,
/// `nofail`, `x-systemd.*`, `auto`/`noauto`) are dropped — they are meaningless
/// to autofs and would make the map entry invalid.
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

/// Outcome of applying a rendered [`AutofsConfig`] to the host.
#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    /// Files whose contents actually changed (the drift set). Empty means the
    /// host already matched the store — a clean no-op, no reload needed.
    pub changed: Vec<String>,
    /// Whether `systemctl reload autofs` was run (only when something changed).
    pub reloaded: bool,
    /// Non-fatal errors (a write or reload failure) — collected, not thrown, so
    /// one bad file doesn't abort the whole apply.
    pub errors: Vec<String>,
}

/// Write the rendered config to the host and reload autofs if anything changed.
/// Idempotent: unchanged files are left untouched and autofs is not reloaded, so
/// this is safe to run on every self-heal tick.
pub async fn apply(cfg: &AutofsConfig) -> ApplyOutcome {
    let mut out = ApplyOutcome::default();

    // Ensure the master drop-in directory exists before writing into it.
    if let Some(dir) = std::path::Path::new(MASTER_DROPIN).parent()
        && let Err(e) = tokio::fs::create_dir_all(dir).await
    {
        out.errors.push(format!("create {}: {e}", dir.display()));
    }

    for (path, contents) in [(MASTER_DROPIN, &cfg.master), (MAP_FILE, &cfg.map)] {
        match write_if_changed(path, contents).await {
            Ok(true) => out.changed.push(path.to_string()),
            Ok(false) => {}
            Err(e) => out.errors.push(format!("write {path}: {e}")),
        }
    }

    if !out.changed.is_empty() {
        match reload().await {
            Ok(()) => out.reloaded = true,
            Err(e) => out.errors.push(format!("reload autofs: {e}")),
        }
    }
    out
}

/// Write `contents` to `path` only if it differs from what's already there.
/// Returns `true` when a write happened (the file was missing or drifted).
async fn write_if_changed(path: &str, contents: &str) -> std::io::Result<bool> {
    if let Ok(existing) = tokio::fs::read_to_string(path).await
        && existing == contents
    {
        return Ok(false);
    }
    tokio::fs::write(path, contents).await?;
    Ok(true)
}

/// `systemctl reload autofs` — reloads maps in place without disturbing live
/// mounts. Surfaces a non-zero exit as an error carrying stderr.
async fn reload() -> Result<(), String> {
    let out = Command::new("systemctl")
        .args(["reload", "autofs"])
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

/// Force an immediate mount of each target by accessing it (a direct-map
/// mountpoint mounts on access). Best-effort — used after an apply so declared
/// mounts come up now rather than on first consumer access. Returns per-target
/// errors.
pub async fn trigger(targets: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    for t in targets {
        // `stat` is enough to trip the automount; we don't care about the
        // result, only that the access happened.
        if let Err(e) = Command::new("stat").arg("--").arg(t).output().await {
            errors.push(format!("trigger {t}: {e}"));
        }
    }
    errors
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
        let line = map_line(&m);
        assert_eq!(
            line,
            "/mnt/data  -fstype=nfs4,vers=4.2,hard,nconnect=4  \
             primary:/srv/pool/data secondary:/srv/pool/data"
        );
    }

    #[test]
    fn render_emits_master_and_sorted_enabled_network_shares() {
        let mounts = vec![
            mount("zeta", "primary:/z", None),
            mount("alpha", "primary:/a", Some("secondary:/a")),
        ];
        let cfg = render(&mounts);
        assert!(cfg.master.contains("/-  /etc/auto.orca --timeout=60"));
        let body: Vec<&str> = cfg.map.lines().filter(|l| !l.starts_with('#')).collect();
        // Sorted by target: /mnt/alpha before /mnt/zeta.
        assert_eq!(body.len(), 2);
        assert!(body[0].starts_with("/mnt/alpha"));
        assert!(body[1].starts_with("/mnt/zeta"));
    }

    #[test]
    fn targets_are_arbitrary_per_mount_direct_map_keys() {
        // The mountpoint is whatever the user set on the row — a direct map
        // keys each entry by its absolute target, so varied paths coexist.
        let mut a = mount("a", "primary:/exports/a", None);
        a.target = "/mnt/data".into();
        let mut b = mount("b", "primary:/exports/b", None);
        b.target = "/mnt/pool/data".into();
        let mut c = mount("c", "primary:/exports/c", None);
        c.target = "/nfs/mnt/data".into();

        let cfg = render(&[a, b, c]);
        let keys: Vec<&str> = cfg
            .map
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split("  ").next().unwrap())
            .collect();
        assert_eq!(keys, ["/mnt/data", "/mnt/pool/data", "/nfs/mnt/data"]);
    }

    #[test]
    fn render_skips_disabled_and_non_network_mounts() {
        let mut disabled = mount("off", "primary:/o", None);
        disabled.enabled = false;
        let mut disk = mount("disk", "primary:/d", None);
        disk.kind = "disk_storage".into();
        let cfg = render(&[disabled, disk]);
        let body: Vec<&str> = cfg.map.lines().filter(|l| !l.starts_with('#')).collect();
        assert!(body.is_empty());
    }
}
