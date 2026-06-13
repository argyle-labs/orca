//! LXC adapter for Proxmox VE hosts.
//!
//! Talks `pct list` for vmid + observed state, then reads
//! `/etc/pve/lxc/<vmid>.conf` directly for the per-CT config (cheaper and
//! richer than `pct config` — same data, no fork, no JSON serialization
//! round-trip). The conf format is one-key-per-line with comma-separated
//! sub-values, which we parse with a small line-oriented state machine
//! rather than dragging in a general INI/TOML dep.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::{
    AdapterError, Container, ContainerMount, ContainerState, ListFilter, LogTail, RestartPolicy,
    RuntimeAdapter, RuntimeKind, StartupOrdering, binary_on_path, local_hostname,
};

/// Default Proxmox LXC config directory. Pulled out as a constant so tests
/// can target a tempdir without touching the global filesystem.
const DEFAULT_PVE_LXC_DIR: &str = "/etc/pve/lxc";

/// Reason a single `mp*` line was dropped during conf parsing. Tracked so
/// the parser can surface "skipped, here's why" instead of silently
/// returning a thinner mount list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSkipReason {
    /// The line references a storage backend (`local-lvm:vm-200-disk-0`,
    /// `cephfs:subvol-...`, etc.) rather than a host bind path. These aren't
    /// dep-graph candidates for the §2.2 mounts reconciler.
    StorageRef { storage: String },
    /// The `mp=<target>` clause was missing. A `mp*` line without a
    /// container-side path is structurally broken; we record it instead of
    /// pretending it's a normal mount.
    MissingTarget,
}

/// One line from a vmid.conf — either a parsed mount or a typed skip reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountParseRow {
    Mount(ContainerMount),
    Skipped {
        key: String,
        reason: MountSkipReason,
    },
}

/// Structured view of one Proxmox `/etc/pve/lxc/<vmid>.conf` blob. Public
/// for unit tests; the adapter doesn't expose it on the trait surface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LxcConf {
    pub hostname: Option<String>,
    pub onboot: bool,
    pub startup: Option<StartupOrdering>,
    pub mounts: Vec<ContainerMount>,
    pub skipped_mounts: Vec<(String, MountSkipReason)>,
}

/// Parsed `pct list` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PctRow {
    pub vmid: u32,
    pub status: ContainerState,
    pub name: String,
}

/// LXC adapter. Holds the conf directory (overridable for tests) and the
/// `pct` binary path (overridable for tests / non-Proxmox LXC).
pub struct LxcProxmoxAdapter {
    conf_dir: PathBuf,
    pct_bin: String,
}

impl LxcProxmoxAdapter {
    pub fn new() -> Self {
        Self {
            conf_dir: PathBuf::from(DEFAULT_PVE_LXC_DIR),
            pct_bin: "pct".to_string(),
        }
    }

    /// Test-only constructor that targets a custom conf directory and pct
    /// binary. The body of `list()` reads both; pointing them at a tempdir
    /// + a stub shell script gives unit-test reach without a live cluster.
    #[doc(hidden)]
    pub fn with_paths(conf_dir: PathBuf, pct_bin: String) -> Self {
        Self { conf_dir, pct_bin }
    }
}

impl Default for LxcProxmoxAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RuntimeAdapter for LxcProxmoxAdapter {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Lxc
    }

    async fn list(&self, filter: &ListFilter) -> Result<Vec<Container>, AdapterError> {
        // Gate on PATH presence — without `pct` this host is plainly not
        // a Proxmox node and we surface that as Unavailable rather than
        // shelling and failing opaquely.
        if self.pct_bin == "pct"
            && !binary_on_path("pct").map_err(|e| AdapterError::Unavailable(e.to_string()))?
        {
            return Err(AdapterError::Unavailable("`pct` binary not on PATH".into()));
        }
        let rows = pct_list(&self.pct_bin).await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let conf_path = self.conf_dir.join(format!("{}.conf", row.vmid));
            let conf = match tokio::fs::read_to_string(&conf_path).await {
                Ok(s) => parse_lxc_conf(&s),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // CT was destroyed between `pct list` and the conf
                    // read — skip silently, the row is gone.
                    tracing::debug!(
                        target: "containers::lxc",
                        vmid = row.vmid,
                        "lxc conf vanished between pct list and read"
                    );
                    continue;
                }
                Err(e) => return Err(AdapterError::Transport(e.to_string())),
            };
            let name = conf.hostname.clone().unwrap_or_else(|| row.name.clone());
            let restart_policy = if conf.onboot {
                RestartPolicy::Always
            } else {
                RestartPolicy::No
            };
            let container = Container {
                id: row.vmid.to_string(),
                name,
                runtime: RuntimeKind::Lxc,
                host: local_hostname().to_string(),
                state: row.status,
                restart_policy,
                image: None,
                labels: Vec::new(),
                mounts: conf.mounts,
                ports: Vec::new(),
                started_at: None,
                finished_at: None,
                restart_count: 0,
                exit_code: None,
                startup: conf.startup,
            };
            if labels_match(&container.labels, &filter.labels) {
                out.push(container);
            }
        }
        Ok(out)
    }

    async fn inspect(&self, id: &str) -> Result<Container, AdapterError> {
        let vmid: u32 = id
            .parse()
            .map_err(|_| AdapterError::NotFound(format!("vmid `{id}` is not a u32")))?;
        let all = self.list(&ListFilter::default()).await?;
        all.into_iter()
            .find(|c| c.id == vmid.to_string())
            .ok_or_else(|| AdapterError::NotFound(format!("lxc vmid `{vmid}`")))
    }

    async fn start(&self, _id: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Refused(
            "LxcProxmoxAdapter::start lands in C3".into(),
        ))
    }

    async fn stop(&self, _id: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Refused(
            "LxcProxmoxAdapter::stop lands in C3".into(),
        ))
    }

    async fn restart(&self, _id: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Refused(
            "LxcProxmoxAdapter::restart lands in C3".into(),
        ))
    }

    async fn logs(&self, _id: &str, _tail: LogTail) -> Result<String, AdapterError> {
        Err(AdapterError::Refused(
            "LxcProxmoxAdapter::logs lands in C3".into(),
        ))
    }
}

/// Run `pct list` and parse its three-column output:
///
/// ```text
/// VMID       Status     Lock         Name
/// 116        running                  tyr
/// 200        stopped                  test
/// ```
async fn pct_list(pct_bin: &str) -> Result<Vec<PctRow>, AdapterError> {
    let out = Command::new(pct_bin)
        .arg("list")
        .output()
        .await
        .map_err(|e| AdapterError::Unavailable(format!("`{pct_bin} list` failed: {e}")))?;
    if !out.status.success() {
        return Err(AdapterError::Transport(format!(
            "`{pct_bin} list` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let stdout = String::from_utf8(out.stdout)
        .map_err(|e| AdapterError::Malformed(format!("pct list utf8: {e}")))?;
    parse_pct_list(&stdout)
}

/// Pure parser for `pct list` stdout. Splits on whitespace; the first row
/// is a header and gets skipped.
pub(crate) fn parse_pct_list(stdout: &str) -> Result<Vec<PctRow>, AdapterError> {
    let mut out = Vec::new();
    for (idx, line) in stdout.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if idx == 0 && trimmed.starts_with("VMID") {
            continue;
        }
        let mut cols = trimmed.split_whitespace();
        let Some(vmid_str) = cols.next() else {
            continue;
        };
        let Ok(vmid) = vmid_str.parse::<u32>() else {
            // Not a data row — could be a banner / footer; skip.
            continue;
        };
        let status_str = cols.next().unwrap_or("");
        let status = map_pct_status(status_str);
        // The 3rd column is Lock (often empty). The Name is whatever's
        // left. `pct` pads with spaces so split_whitespace already
        // collapsed empty Lock; we recover by treating the remainder as
        // the name.
        let rest: Vec<&str> = cols.collect();
        let name = rest.join(" ");
        out.push(PctRow { vmid, status, name });
    }
    Ok(out)
}

fn map_pct_status(s: &str) -> ContainerState {
    match s {
        "running" => ContainerState::Running,
        "stopped" => ContainerState::Exited,
        "paused" => ContainerState::Paused,
        // pct surfaces "mounted" when a CT's rootfs is mounted but the CT
        // itself isn't started — closest to a Created/idle state.
        "mounted" => ContainerState::Created,
        _ => ContainerState::Unknown,
    }
}

/// Parse a `/etc/pve/lxc/<vmid>.conf` blob.
///
/// Recognised keys (others ignored):
///
/// - `hostname: <name>` — display name.
/// - `onboot: 0|1` — boot-time auto-start; mapped to `RestartPolicy::Always`
///   when `1`.
/// - `startup: order=<N>,up=<sec>,down=<sec>` — boot ordering; mapped into
///   [`StartupOrdering`].
/// - `mp0:`..`mp255:` — bind/dir mount lines.
pub(crate) fn parse_lxc_conf(blob: &str) -> LxcConf {
    let mut out = LxcConf::default();
    for raw_line in blob.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "hostname" => out.hostname = Some(value.to_string()),
            "onboot" => out.onboot = value == "1",
            "startup" => out.startup = Some(parse_startup(value)),
            k if is_mp_key(k) => match parse_mp_line(value) {
                MountParseRow::Mount(m) => out.mounts.push(m),
                MountParseRow::Skipped { key: _, reason } => {
                    out.skipped_mounts.push((k.to_string(), reason));
                }
            },
            _ => continue,
        }
    }
    out
}

fn is_mp_key(k: &str) -> bool {
    if let Some(n) = k.strip_prefix("mp")
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    false
}

/// Parse a `startup` value such as `order=3,up=30,down=60`. Missing fields
/// leave the corresponding `Option` at `None`.
pub(crate) fn parse_startup(value: &str) -> StartupOrdering {
    let mut out = StartupOrdering::default();
    for part in value.split(',') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        let parsed = v.parse::<u32>().ok();
        match k {
            "order" => out.order = parsed,
            "up" => out.up_delay_secs = parsed,
            "down" => out.down_delay_secs = parsed,
            _ => continue,
        }
    }
    out
}

/// Parse one `mp*` value such as `/mnt/pool/data,mp=/data,ro=1`.
///
/// The first comma-separated token is the source. If it contains a `:` and
/// doesn't start with `/`, it's a Proxmox storage reference (`local-lvm:...`)
/// and we surface it as [`MountSkipReason::StorageRef`] rather than pretending
/// it's a host bind.
pub(crate) fn parse_mp_line(value: &str) -> MountParseRow {
    let mut parts = value.split(',');
    let Some(first) = parts.next() else {
        return MountParseRow::Skipped {
            key: String::new(),
            reason: MountSkipReason::MissingTarget,
        };
    };
    let source_raw = first.trim();
    // Storage refs look like `<storage>:<volname>`. Distinguished from host
    // paths by (a) the leading non-`/` char and (b) the colon-separated
    // shape. A bind path of the form `/mnt/foo:bar` would still start with
    // `/`, so this discriminator is unambiguous.
    if !source_raw.starts_with('/')
        && let Some((storage, _)) = source_raw.split_once(':')
    {
        return MountParseRow::Skipped {
            key: String::new(),
            reason: MountSkipReason::StorageRef {
                storage: storage.to_string(),
            },
        };
    }

    let mut options: HashMap<&str, &str> = HashMap::new();
    for kv in parts {
        let kv = kv.trim();
        if let Some((k, v)) = kv.split_once('=') {
            options.insert(k.trim(), v.trim());
        }
    }
    let Some(target) = options.get("mp") else {
        return MountParseRow::Skipped {
            key: String::new(),
            reason: MountSkipReason::MissingTarget,
        };
    };
    let read_only = options.get("ro").is_some_and(|v| *v == "1");
    MountParseRow::Mount(ContainerMount {
        source: PathBuf::from(source_raw),
        target: PathBuf::from(*target),
        read_only,
    })
}

fn labels_match(have: &[(String, String)], wanted: &[(String, String)]) -> bool {
    wanted
        .iter()
        .all(|w| have.iter().any(|h| h.0 == w.0 && h.1 == w.1))
}

/// Hook so the LXC unit tests can target a fixture conf file directly
/// rather than going through `list()` (which expects a full cluster).
#[doc(hidden)]
pub fn parse_lxc_conf_for_test(blob: &str) -> LxcConf {
    parse_lxc_conf(blob)
}

/// Re-export for tests that want to construct an adapter against a
/// fixture directory.
#[doc(hidden)]
pub fn fixture_dir_default() -> &'static Path {
    Path::new(DEFAULT_PVE_LXC_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── pct list parsing ───────────────────────────────────────────────────

    #[test]
    fn pct_list_parses_typical_header_plus_rows() {
        let stdout = "\
VMID       Status     Lock         Name
116        running                 tyr
200        stopped                 test-ct
201        paused                  paused-ct
";
        let rows = parse_pct_list(stdout).expect("parses");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].vmid, 116);
        assert_eq!(rows[0].status, ContainerState::Running);
        assert_eq!(rows[0].name, "tyr");
        assert_eq!(rows[1].status, ContainerState::Exited);
        assert_eq!(rows[2].status, ContainerState::Paused);
    }

    #[test]
    fn pct_list_skips_blank_and_non_numeric_lines() {
        let stdout = "VMID       Status     Lock         Name\n\n116 running tyr\nbogus line\n";
        let rows = parse_pct_list(stdout).expect("parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].vmid, 116);
    }

    // ── conf parsing ───────────────────────────────────────────────────────

    #[test]
    fn conf_parses_hostname_onboot_startup() {
        let blob = include_str!("../../tests/fixtures/lxc/116_basic.conf");
        let parsed = parse_lxc_conf(blob);
        assert_eq!(parsed.hostname.as_deref(), Some("tyr"));
        assert!(parsed.onboot);
        let s = parsed.startup.expect("startup parsed");
        assert_eq!(s.order, Some(3));
        assert_eq!(s.up_delay_secs, Some(30));
        assert_eq!(s.down_delay_secs, Some(60));
    }

    #[test]
    fn conf_parses_multiple_mp_lines_with_ro_flag() {
        let blob = include_str!("../../tests/fixtures/lxc/200_mounts.conf");
        let parsed = parse_lxc_conf(blob);
        assert_eq!(parsed.mounts.len(), 2);
        assert_eq!(parsed.mounts[0].source, PathBuf::from("/mnt/pool/data"));
        assert_eq!(parsed.mounts[0].target, PathBuf::from("/data"));
        assert!(!parsed.mounts[0].read_only);
        assert_eq!(parsed.mounts[1].source, PathBuf::from("/mnt/pool/configs"));
        assert_eq!(parsed.mounts[1].target, PathBuf::from("/config"));
        assert!(parsed.mounts[1].read_only);
    }

    #[test]
    fn conf_drops_storage_refs_with_typed_reason() {
        let blob = include_str!("../../tests/fixtures/lxc/300_storage_mp.conf");
        let parsed = parse_lxc_conf(blob);
        assert_eq!(parsed.mounts.len(), 1);
        assert_eq!(parsed.mounts[0].target, PathBuf::from("/srv"));
        // Two `mp*` lines were storage refs and got skipped.
        assert_eq!(parsed.skipped_mounts.len(), 2);
        let lvm = parsed
            .skipped_mounts
            .iter()
            .find(|(k, _)| k == "mp1")
            .expect("mp1 skip recorded");
        assert!(
            matches!(lvm.1, MountSkipReason::StorageRef { ref storage } if storage == "local-lvm")
        );
        let cephfs = parsed
            .skipped_mounts
            .iter()
            .find(|(k, _)| k == "mp2")
            .expect("mp2 skip recorded");
        assert!(matches!(
            cephfs.1,
            MountSkipReason::StorageRef { ref storage } if storage == "cephfs"
        ));
    }

    #[test]
    fn conf_missing_hostname_yields_none() {
        let blob = include_str!("../../tests/fixtures/lxc/400_no_hostname.conf");
        let parsed = parse_lxc_conf(blob);
        assert!(parsed.hostname.is_none());
        // Still parses other keys.
        assert!(parsed.onboot);
    }

    #[test]
    fn conf_ignores_comments_and_blank_lines() {
        let blob = "# header comment\n\nhostname: alpha\n\n# trailing\n";
        let parsed = parse_lxc_conf(blob);
        assert_eq!(parsed.hostname.as_deref(), Some("alpha"));
    }

    // ── startup ───────────────────────────────────────────────────────────

    #[test]
    fn startup_parses_full_triple() {
        let s = parse_startup("order=7,up=15,down=45");
        assert_eq!(s.order, Some(7));
        assert_eq!(s.up_delay_secs, Some(15));
        assert_eq!(s.down_delay_secs, Some(45));
    }

    #[test]
    fn startup_leaves_unmentioned_fields_none() {
        let s = parse_startup("order=2");
        assert_eq!(s.order, Some(2));
        assert!(s.up_delay_secs.is_none());
        assert!(s.down_delay_secs.is_none());
    }

    #[test]
    fn startup_skips_bad_numbers() {
        let s = parse_startup("order=notanumber,up=3");
        assert!(s.order.is_none());
        assert_eq!(s.up_delay_secs, Some(3));
    }

    // ── mp line ───────────────────────────────────────────────────────────

    #[test]
    fn mp_line_with_ro_marks_read_only() {
        let row = parse_mp_line("/mnt/pool/data,mp=/data,ro=1");
        let MountParseRow::Mount(m) = row else {
            panic!("expected Mount, got {row:?}");
        };
        assert!(m.read_only);
    }

    #[test]
    fn mp_line_without_target_skips_with_missing_target() {
        let row = parse_mp_line("/mnt/pool/data,size=8G");
        assert!(matches!(
            row,
            MountParseRow::Skipped {
                reason: MountSkipReason::MissingTarget,
                ..
            }
        ));
    }

    #[test]
    fn mp_line_with_storage_ref_is_skipped() {
        let row = parse_mp_line("local-lvm:vm-200-disk-0,mp=/srv");
        let MountParseRow::Skipped { reason, .. } = row else {
            panic!("expected Skipped");
        };
        assert!(
            matches!(reason, MountSkipReason::StorageRef { storage } if storage == "local-lvm")
        );
    }

    // ── mp key recogniser ─────────────────────────────────────────────────

    #[test]
    fn mp_key_recognises_mp0_through_mp255_only() {
        assert!(is_mp_key("mp0"));
        assert!(is_mp_key("mp42"));
        assert!(is_mp_key("mp255"));
        assert!(!is_mp_key("mp")); // bare prefix, no digit
        assert!(!is_mp_key("mpx")); // non-digit
        assert!(!is_mp_key("rootfs"));
        assert!(!is_mp_key("net0"));
    }
}
