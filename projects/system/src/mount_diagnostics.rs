//! Core diagnostics provider: provisioned mounts set up to cause an outage.
//!
//! [`utils::mount_audit`] has classified these since the #563 investigation and
//! nothing in orca ever called it. The classifier was the easy half; the half
//! that was missing is gathering the facts it judges, which is why the risk it
//! describes has stayed invisible.
//!
//! The motivating case is frigg CT113: `/srv/jellyfin-transcode` bind-mounted to
//! `/transcode`, writable, no size cap, and backed by `pve-root`. Jellyfin had
//! throttling and segment deletion both off, so one stream wrote 514 segments
//! and 3.7 GB in about two minutes — into the hypervisor's own root filesystem.
//! Filling that does not degrade one container, it takes down every guest on the
//! node.
//!
//! **It measures zero bytes at rest.** Every level check, every liveness probe
//! and every capacity trend reads it as perfectly healthy, because at rest it is
//! — the risk is in the shape of the configuration, not in any current value.
//! That is the whole reason this is a separate provider rather than another
//! threshold.
//!
//! ## What it can and cannot see
//!
//! Gated on the `proxmox` capability: the facts come from `pct config`, so a
//! host with no `pct` registers nothing rather than reporting an empty audit
//! that would read as "no risky mounts here".
//!
//! `on_host_root_fs` is MEASURED, by comparing the source's device id to `/`'s.
//! A path-prefix guess would call every `/srv/...` source host-root even on a
//! node that mounts `/srv` separately — which is the one fact that separates
//! "a guest can fill its scratch" from "a guest can kill the hypervisor".
//!
//! `consumed` is always `None`. Nothing here can tell whether a mount is read
//! or written, and `mount_audit` treats `None` as "could not tell" precisely so
//! that unknown never renders as unused. Reporting an idle-looking mount as
//! disused is how a live mount gets deleted.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use contract::BoxFuture;
use contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome, Severity,
};
use utils::mount_audit::{self, MountFinding, MountKind, MountSpec, Risk};

/// Registry name. Stable — operators type it as `--provider`.
pub const PROVIDER_NAME: &str = "mounts";

/// One mount line from `pct config`, before the host-side facts are measured.
///
/// Split from [`MountSpec`] because parsing is pure and measuring is not: the
/// device-id comparison needs the filesystem, and keeping it out of the parser
/// is what makes every `pct config` shape testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMount {
    pub key: String,
    pub source: String,
    pub target: String,
    pub kind: MountKind,
    pub size_limit_bytes: Option<u64>,
    pub read_only: bool,
}

/// Parse a `size=` value as PVE writes it: `8G`, `512M`, `1024K`, or bare bytes.
///
/// An unrecognised unit yields `None` — "no cap we could read" — rather than a
/// number we are not sure about. A wrong cap here would silence a real finding,
/// which is worse than reporting a mount we could not size.
fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    let (digits, mult) = match v.chars().last()?.to_ascii_uppercase() {
        'K' => (&v[..v.len() - 1], 1024u64),
        'M' => (&v[..v.len() - 1], 1024 * 1024),
        'G' => (&v[..v.len() - 1], 1024 * 1024 * 1024),
        'T' => (&v[..v.len() - 1], 1024u64.pow(4)),
        c if c.is_ascii_digit() => (v, 1),
        _ => return None,
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(mult)
}

/// Parse the `mpN:` / `rootfs:` lines out of `pct config` output.
///
/// The shape is `mp0: <source>,mp=<target>,size=8G,ro=1`. A source containing
/// a `/` is a host path (a bind); anything else is `storage:volume` form, which
/// has its own size and fills itself rather than the node.
///
/// `rootfs` is skipped: it is the container's own disk, already sized by the
/// hypervisor, and flagging every container's root as unbounded scratch would
/// bury the finding that matters under one per guest.
pub fn parse_pct_config(vmid: &str, text: &str) -> Vec<ParsedMount> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if !(key.starts_with("mp") && key[2..].chars().all(|c| c.is_ascii_digit()) && key.len() > 2)
        {
            continue;
        }
        let mut parts = rest.trim().split(',');
        let Some(source) = parts.next().map(str::trim) else {
            continue;
        };
        if source.is_empty() {
            continue;
        }
        let mut target = String::new();
        let mut size_limit_bytes = None;
        let mut read_only = false;
        for opt in parts {
            let Some((k, v)) = opt.split_once('=') else {
                continue;
            };
            match k.trim() {
                "mp" => target = v.trim().to_string(),
                "size" => size_limit_bytes = parse_size(v),
                // PVE spells it both ways depending on version.
                "ro" | "readonly" => read_only = matches!(v.trim(), "1" | "true" | "yes"),
                _ => {}
            }
        }
        out.push(ParsedMount {
            key: key.to_string(),
            source: source.to_string(),
            target,
            // A bind is identified by the source being a path, not by a flag.
            kind: if source.starts_with('/') {
                MountKind::Bind
            } else {
                MountKind::Volume
            },
            size_limit_bytes,
            read_only,
            // Carried into the id so a finding names the guest it belongs to.
        });
        if let Some(last) = out.last_mut() {
            last.key = format!("{vmid}:{}", last.key);
        }
    }
    out
}

/// Does `path` sit on the same filesystem as `/`?
///
/// Measured by device id, never by prefix. `/srv/x` is host-root on a node with
/// one volume and is not on a node that mounts `/srv` separately, and that
/// difference is the whole severity of the finding. An unreadable path yields
/// `false`: claiming host-root on a stat failure would manufacture a Crit from
/// a missing directory.
#[cfg(unix)]
fn on_host_root_fs(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(here) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(root) = std::fs::metadata("/") else {
        return false;
    };
    here.dev() == root.dev()
}

#[cfg(not(unix))]
fn on_host_root_fs(_path: &str) -> bool {
    false
}

/// Attach the measured host-side facts to a parsed mount.
fn to_spec(p: &ParsedMount) -> MountSpec {
    MountSpec {
        id: p.key.clone(),
        source: p.source.clone(),
        target: p.target.clone(),
        kind: p.kind,
        size_limit_bytes: p.size_limit_bytes,
        read_only: p.read_only,
        on_host_root_fs: p.kind == MountKind::Bind && on_host_root_fs(&p.source),
        // Never `Some(false)`: see the module docs.
        consumed: None,
    }
}

/// Project an audit finding into the diagnostics surface.
fn finding_for(f: &MountFinding) -> Finding {
    let (id, severity, title) = match f.risk {
        Risk::UnboundedOnHostRootFs => (
            "mount-unbounded-on-host-root",
            Severity::Crit,
            format!("{}: uncapped bind mount on the host root filesystem", f.id),
        ),
        Risk::UnboundedScratch => (
            "mount-unbounded-scratch",
            Severity::Warn,
            format!("{}: uncapped writable bind mount", f.id),
        ),
        Risk::UnusedProvisioned => (
            "mount-unused-provisioned",
            Severity::Info,
            format!("{}: provisioned mount with no known consumer", f.id),
        ),
    };
    Finding {
        id: id.to_string(),
        provider: PROVIDER_NAME.to_string(),
        severity,
        title,
        detail: f.detail.clone(),
        // No repair. Capping a mount means moving its source to a dedicated
        // volume or changing the writer's config — both change where a running
        // service puts its data, which orca must not do unattended.
        repair: None,
    }
}

/// How long any one `pct` call may take before it is abandoned.
///
/// `pct` is normally instant. The bound exists so a wedged hypervisor turns
/// this provider into a missing report rather than a hung `diagnostics.diagnose`
/// — a diagnostic that can hang is one operators stop running.
const PCT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run `pct` and capture stdout, or fail. Non-zero exit is a failure: partial
/// output from a failed command is not a config.
async fn pct(args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        PCT_TIMEOUT,
        tokio::process::Command::new("pct")
            .args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow!("pct {} timed out after {:?}", args.join(" "), PCT_TIMEOUT))??;
    if !out.status.success() {
        return Err(anyhow!(
            "pct {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Container ids on this node, from `pct list`.
async fn vmids() -> Result<Vec<String>> {
    let out = pct(&["list"]).await?;
    Ok(out
        .lines()
        .skip(1) // header
        .filter_map(|l| l.split_whitespace().next())
        .filter(|id| id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty())
        .map(str::to_string)
        .collect())
}

/// Reports provisioned mounts that are shaped to cause an outage later.
pub struct MountDiagnostics;

impl DiagnosticsProvider for MountDiagnostics {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn diagnose(&self, _args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        Box::pin(async move {
            let mut specs = Vec::new();
            for vmid in vmids().await? {
                // One container's config failing to read must not hide the
                // others' risky mounts — the report is worth having partial.
                let Ok(text) = pct(&["config", &vmid]).await else {
                    continue;
                };
                specs.extend(parse_pct_config(&vmid, &text).iter().map(to_spec));
            }
            Ok(mount_audit::audit(&specs).iter().map(finding_for).collect())
        })
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        Box::pin(async move {
            Err(anyhow!(
                "{PROVIDER_NAME} has no automatic repair for {:?}: capping a mount means \
                 moving its source to a dedicated volume or changing the writer's config, \
                 and both relocate a running service's data",
                args.repair_id
            ))
        })
    }
}

/// Register this provider, but only where `pct` exists.
///
/// A host with no proxmox would otherwise report an empty audit, which reads as
/// "no risky mounts here" when the truth is "nothing was examined".
pub fn register() {
    if !crate::capability::is_available("proxmox") {
        return;
    }
    contract::diagnostics::register_provider(Arc::new(MountDiagnostics));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape that motivated this, as `pct config 113` renders it. The
    /// transcode mount is the one that matters; the others are here so the
    /// test proves it is *selected*, not just parsed.
    const CT113: &str = "\
arch: amd64
cores: 4
hostname: jellyfin
memory: 8192
mp0: /srv/jellyfin-transcode,mp=/transcode
mp1: local-lvm:vm-113-disk-1,mp=/var/lib/jellyfin,size=32G
mp2: /mnt/media,mp=/media,ro=1
net0: name=eth0,bridge=vmbr0
rootfs: local-lvm:vm-113-disk-0,size=16G
swap: 512
";

    #[test]
    fn the_transcode_mount_is_parsed_the_way_pct_writes_it() {
        let got = parse_pct_config("113", CT113);
        let mp0 = got.iter().find(|m| m.key == "113:mp0").expect("mp0");
        assert_eq!(mp0.source, "/srv/jellyfin-transcode");
        assert_eq!(mp0.target, "/transcode");
        assert_eq!(mp0.kind, MountKind::Bind);
        // The absence of `size=` is the finding. A default would erase it.
        assert_eq!(mp0.size_limit_bytes, None);
        assert!(!mp0.read_only);
    }

    #[test]
    fn rootfs_and_non_mount_lines_are_not_mounts() {
        let got = parse_pct_config("113", CT113);
        let keys: Vec<&str> = got.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["113:mp0", "113:mp1", "113:mp2"]);
        // `rootfs` is sized by the hypervisor already; flagging every
        // container's root would bury the one finding that matters.
        assert!(!keys.iter().any(|k| k.contains("rootfs")));
        // `net0:` and `memory:` have colons too and are not mounts.
        assert!(!keys.iter().any(|k| k.contains("net") || k.contains("mem")));
    }

    #[test]
    fn a_storage_volume_is_not_a_bind() {
        let got = parse_pct_config("113", CT113);
        let mp1 = got.iter().find(|m| m.key == "113:mp1").expect("mp1");
        // `storage:volume` form — it has its own size and fills itself rather
        // than the node, so it must not be classed as unbounded scratch.
        assert_eq!(mp1.kind, MountKind::Volume);
        assert_eq!(mp1.size_limit_bytes, Some(32 * 1024 * 1024 * 1024));
    }

    #[test]
    fn a_read_only_bind_is_recognised_as_read_only() {
        let got = parse_pct_config("113", CT113);
        let mp2 = got.iter().find(|m| m.key == "113:mp2").expect("mp2");
        assert_eq!(mp2.kind, MountKind::Bind);
        // Uncapped, but it cannot grow — so it is never unbounded scratch.
        assert!(mp2.read_only);
        assert!(
            mount_audit::audit(&[MountSpec {
                on_host_root_fs: true,
                ..to_spec(mp2)
            }])
            .is_empty()
        );
    }

    #[test]
    fn pve_size_units_parse_and_an_unknown_unit_does_not_invent_a_cap() {
        assert_eq!(parse_size("8G"), Some(8 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("1024K"), Some(1024 * 1024));
        assert_eq!(parse_size("2T"), Some(2 * 1024u64.pow(4)));
        assert_eq!(parse_size("4096"), Some(4096));
        // A cap we misread would SILENCE a real finding, so an unrecognised
        // unit must read as "no cap we could determine", not as a number.
        assert_eq!(parse_size("8Z"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("G"), None);
    }

    #[test]
    fn the_uncapped_host_root_bind_outranks_the_merely_uncapped_one() {
        // Both are uncapped writable binds. Only one can take the node down,
        // and it has to be the one reported first.
        let specs = vec![
            MountSpec {
                on_host_root_fs: false,
                ..to_spec(&parse_pct_config("100", "mp0: /mnt/other,mp=/scratch")[0])
            },
            MountSpec {
                on_host_root_fs: true,
                ..to_spec(&parse_pct_config("113", "mp0: /srv/jellyfin-transcode,mp=/transcode")[0])
            },
        ];
        let findings = mount_audit::audit(&specs);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].risk, Risk::UnboundedOnHostRootFs);
        assert_eq!(findings[0].id, "113:mp0");

        let f = finding_for(&findings[0]);
        assert_eq!(f.severity, Severity::Crit);
        assert_eq!(f.provider, PROVIDER_NAME);
        // Severity is the point: the second is bad, the first is an outage.
        assert_eq!(finding_for(&findings[1]).severity, Severity::Warn);
    }

    #[test]
    fn a_mount_is_never_reported_as_unused() {
        // Nothing here can observe a reader or a writer. `mount_audit` only
        // reports disuse on `Some(false)`, and this gatherer must never
        // produce that — an idle mount and an unused one look identical from
        // here, and the difference is whether deleting it is safe.
        let spec = to_spec(&parse_pct_config("113", "mp1: local-lvm:vol,mp=/x,size=8G")[0]);
        assert_eq!(spec.consumed, None);
        assert!(mount_audit::audit(&[spec]).is_empty());
    }

    #[test]
    fn host_root_membership_is_measured_not_guessed_from_the_path() {
        // `/` is trivially on its own filesystem; a path that cannot be
        // stat'ed must not manufacture a Crit.
        assert!(on_host_root_fs("/"));
        assert!(!on_host_root_fs("/definitely/not/a/real/path/orca-test"));
    }

    #[test]
    fn a_malformed_config_line_is_skipped_not_fatal() {
        let got = parse_pct_config("1", "mp0:\nmp1: ,mp=/x\nmp: /a,mp=/b\nmpX: /a,mp=/b\n");
        // Empty sources and non-numeric `mp` keys are not mounts.
        assert!(got.is_empty(), "{got:?}");
    }
}
