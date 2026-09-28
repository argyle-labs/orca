//! What this host's init system says is BROKEN — reported, not guessed at.
//!
//! A service that crashed and was never restarted is invisible: the init system
//! records it and nothing asks. That is how `act_runner` sat dead on freyr for
//! three days while CI quietly queued, and how baldur's `dnsmasq` and loki's
//! `orca-smb-mounts.service` were found only because someone went looking by
//! hand (2026-09-27).
//!
//! No registry of "services that matter" is needed, and deliberately so: a
//! crashed service is always a defect, whoever installed it. Asking the init
//! system directly means orca reports the same truth the host already knows,
//! rather than a list someone has to remember to maintain.

use std::process::Command;

use anyhow::Result;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Service manager this host actually runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Openrc,
    Launchd,
    /// Slackware-derived Unraid: rc scripts with no queryable failed-unit state.
    Unraid,
    Unknown,
}

/// One service the init system considers broken.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct BrokenService {
    /// Service name as the init system spells it — what a repair must pass back.
    pub name: String,
    /// What the init system said, verbatim enough to act on.
    pub detail: String,
    /// The unit no longer exists and this is only a retained failure record.
    /// systemd keeps a `failed` result after the unit file is deleted, so the
    /// entry outlives the thing it describes — loki still lists a unit that
    /// failed in August and was removed. Reporting that as a live defect would
    /// cry wolf on every run, forever.
    #[serde(default)]
    pub stale: bool,
}

/// Detect the service manager. Ordered most-specific first: Unraid is
/// Slackware+rc and would otherwise read as `Unknown`, and a systemd host can
/// carry a stray `/sbin/openrc` without using it.
pub fn detect_init() -> Init {
    if cfg!(target_os = "macos") {
        return Init::Launchd;
    }
    if !cfg!(target_os = "linux") {
        return Init::Unknown;
    }
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    if os_release.contains("ID=\"unraid-os\"") || os_release.contains("ID=unraid-os") {
        return Init::Unraid;
    }
    if std::path::Path::new("/run/systemd/system").exists() {
        return Init::Systemd;
    }
    if std::path::Path::new("/run/openrc").exists() || std::path::Path::new("/sbin/openrc").exists()
    {
        return Init::Openrc;
    }
    Init::Unknown
}

/// `rc-status --crashed` prints one bare service name per line.
pub fn parse_openrc_crashed(out: &str) -> Vec<BrokenService> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // OpenRC prefixes advisory chatter with `*`; a service name never has one.
        .filter(|l| !l.starts_with('*'))
        .map(|name| BrokenService {
            name: name.to_string(),
            detail: "crashed (started, then the process died)".to_string(),
            stale: false,
        })
        .collect()
}

/// `systemctl list-units --state=failed --no-legend --no-pager` rows look like
/// `● unit.service not-found failed failed Description`. The leading bullet is
/// decoration; the unit name is the first real column.
pub fn parse_systemd_failed(out: &str) -> Vec<BrokenService> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let line = line.trim_start_matches(['●', '*', '×']).trim();
            let mut cols = line.split_whitespace();
            let name = cols.next()?;
            // `0 loaded units listed.` and similar summaries are not units.
            if !name.contains('.') {
                return None;
            }
            let rest: Vec<&str> = cols.collect();
            // Columns are UNIT LOAD ACTIVE SUB DESCRIPTION; LOAD of `not-found`
            // means the unit file is gone and only the failure record remains.
            let stale = rest.first().is_some_and(|load| *load == "not-found");
            Some(BrokenService {
                name: name.to_string(),
                detail: if rest.is_empty() {
                    "failed".to_string()
                } else {
                    rest.join(" ")
                },
                stale,
            })
        })
        .collect()
}

/// `launchctl list` rows are `PID<TAB>Status<TAB>Label`.
///
/// Two things make launchd different, and both are honoured here. `Status` is
/// the LAST exit status, so a row with a live PID is running now whatever it
/// exited with before — only a PID-less row is actually down. And the list is
/// mostly on-demand Apple agents whose non-zero exits are normal, so this
/// reports only labels the caller vouches for; a blanket scan would be noise
/// presented as findings.
pub fn parse_launchd_failed(out: &str, label_prefixes: &[&str]) -> Vec<BrokenService> {
    out.lines()
        .skip_while(|l| l.starts_with("PID"))
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let (pid, status, label) = (cols.next()?, cols.next()?, cols.next()?);
            if !label_prefixes.iter().any(|p| label.starts_with(p)) {
                return None;
            }
            // Running now → not broken, regardless of a past non-zero exit.
            if pid != "-" {
                return None;
            }
            if status == "0" {
                return None;
            }
            Some(BrokenService {
                name: label.to_string(),
                detail: format!("not running; last exit status {status}"),
                stale: false,
            })
        })
        .collect()
}

/// Commands that restart `name`, in order.
///
/// OpenRC is the reason this is a plan and not one command: a `crashed` service
/// refuses a plain `start` with "has already been started" and stays crashed,
/// because OpenRC's recorded state outlived the process. It must be `zap`ped to
/// stopped first. That cost a real debugging session; encoding it here means it
/// costs nobody another one.
pub fn restart_plan(init: Init, name: &str) -> Vec<Vec<String>> {
    let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match init {
        Init::Openrc => vec![
            argv(&["rc-service", name, "zap"]),
            argv(&["rc-service", name, "start"]),
        ],
        Init::Systemd => vec![argv(&["systemctl", "restart", name])],
        Init::Launchd => vec![argv(&["launchctl", "kickstart", "-k", name])],
        Init::Unraid | Init::Unknown => Vec::new(),
    }
}

/// Ask the init system what is broken on this host.
///
/// A manager that cannot answer returns an empty list rather than an error:
/// "nothing reported" must never read as "nothing wrong", so callers get the
/// [`Init`] back and say which it was.
pub fn broken_services(init: Init, launchd_prefixes: &[&str]) -> Result<Vec<BrokenService>> {
    let out = match init {
        Init::Openrc => run(&["rc-status", "--crashed"]),
        Init::Systemd => run(&[
            "systemctl",
            "list-units",
            "--state=failed",
            "--no-legend",
            "--no-pager",
        ]),
        Init::Launchd => run(&["launchctl", "list"]),
        Init::Unraid | Init::Unknown => return Ok(Vec::new()),
    };
    let Some(out) = out else {
        return Ok(Vec::new());
    };
    Ok(match init {
        Init::Openrc => parse_openrc_crashed(&out),
        Init::Systemd => parse_systemd_failed(&out),
        Init::Launchd => parse_launchd_failed(&out, launchd_prefixes),
        Init::Unraid | Init::Unknown => Vec::new(),
    })
}

/// Run a query command, returning its stdout. `None` when the binary is absent
/// or it failed — a query that did not run has no answer to report.
fn run(argv: &[&str]) -> Option<String> {
    let out = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run one command of a restart plan. Returns the failure text, if any.
fn run_step(argv: &[String]) -> Option<String> {
    match Command::new(&argv[0]).args(&argv[1..]).output() {
        Ok(out) if out.status.success() => None,
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            Some(if err.is_empty() {
                format!("`{}` exited {}", argv.join(" "), out.status)
            } else {
                format!("`{}`: {err}", argv.join(" "))
            })
        }
        Err(e) => Some(format!("`{}`: {e}", argv.join(" "))),
    }
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ServiceRestartArgs {
    /// Service name as the init system spells it (e.g. `act_runner`).
    #[arg(long)]
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ServiceRestartOutput {
    pub name: String,
    /// Service manager that ran the restart.
    pub init: String,
    /// The commands actually run, in order — so the answer says what it did.
    pub steps: Vec<String>,
    pub restarted: bool,
    /// Why not, when `restarted` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Restart a service this host's init system manages.
///
/// Exists because `system.detail` can now name a crashed service but nothing
/// could act on it without SSH, which our own operating rule treats as
/// break-glass (#631). The OpenRC `zap`-before-`start` dance is in
/// [`restart_plan`], so the caller never has to know it.
#[orca_tool(domain = "system", verb = "service.restart")]
async fn service_restart(
    args: ServiceRestartArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ServiceRestartOutput> {
    let name = args.name.trim();
    if name.is_empty() {
        anyhow::bail!("service name is required");
    }
    let init = detect_init();
    let plan = restart_plan(init, name);
    if plan.is_empty() {
        anyhow::bail!("cannot restart services under {init:?} — no supported command");
    }
    let mut steps = Vec::new();
    let last = plan.len() - 1;
    for (i, argv) in plan.iter().enumerate() {
        steps.push(argv.join(" "));
        // `zap` on an already-stopped service is a no-op that can still report
        // non-zero, so only the FINAL step decides success.
        if let Some(err) = run_step(argv)
            && i == last
        {
            return Ok(ServiceRestartOutput {
                name: name.to_string(),
                init: format!("{init:?}"),
                steps,
                restarted: false,
                error: Some(err),
            });
        }
    }
    Ok(ServiceRestartOutput {
        name: name.to_string(),
        init: format!("{init:?}"),
        steps,
        restarted: true,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrc_crashed_lists_bare_service_names() {
        // Real output shape from baldur, 2026-09-27.
        let got = parse_openrc_crashed("dnsmasq\nact_runner\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "dnsmasq");
        assert_eq!(got[1].name, "act_runner");
        assert!(got[0].detail.contains("crashed"));
    }

    #[test]
    fn openrc_ignores_blank_lines_and_advisory_chatter() {
        let got = parse_openrc_crashed("\n * Caching service dependencies ...\nact_runner\n\n");
        assert_eq!(got.len(), 1, "got: {got:?}");
        assert_eq!(got[0].name, "act_runner");
    }

    #[test]
    fn a_healthy_host_reports_nothing() {
        assert!(parse_openrc_crashed("").is_empty());
        assert!(parse_systemd_failed("").is_empty());
    }

    #[test]
    fn systemd_failed_strips_the_bullet_and_keeps_the_unit() {
        // Real output shape from loki, 2026-09-27.
        let got = parse_systemd_failed(
            "● orca-smb-mounts.service not-found failed failed orca-smb-mounts.service\n",
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "orca-smb-mounts.service");
        assert!(got[0].detail.contains("failed"));
        // The unit file is gone; this is a retained record, not a live defect.
        assert!(got[0].stale, "not-found must classify as stale: {got:?}");
    }

    #[test]
    fn systemd_summary_lines_are_not_units() {
        // `--no-legend` usually suppresses this, but a systemd that prints it
        // anyway must not yield a service literally named "0".
        assert!(parse_systemd_failed("0 loaded units listed.\n").is_empty());
    }

    #[test]
    fn a_unit_that_still_exists_is_a_live_defect_not_residue() {
        // Control for the stale case above: same parser, loaded unit, not stale.
        let got = parse_systemd_failed("● nginx.service loaded failed failed nginx\n");
        assert_eq!(got.len(), 1);
        assert!(!got[0].stale, "a loaded unit is a real failure: {got:?}");
    }

    #[test]
    fn launchd_reports_only_vouched_labels() {
        // Real shape from mint: Apple's own agents exit non-zero routinely and
        // are not this host's problem.
        let out = "PID\tStatus\tLabel\n-\t-9\tcom.apple.knowledgeconstructiond\n-\t1\tcom.argyle.gitea-act-runner\n";
        let got = parse_launchd_failed(out, &["com.argyle.", "actions.runner."]);
        assert_eq!(got.len(), 1, "got: {got:?}");
        assert_eq!(got[0].name, "com.argyle.gitea-act-runner");
    }

    #[test]
    fn launchd_running_is_not_broken_whatever_it_last_exited_with() {
        // A live PID with a non-zero last status was killed once and came back.
        // Reporting that as broken would be reporting history as current state.
        let out = "PID\tStatus\tLabel\n60886\t-9\tcom.argyle.gitea-act-runner\n";
        assert!(parse_launchd_failed(out, &["com.argyle."]).is_empty());
    }

    #[test]
    fn an_openrc_restart_zaps_before_starting() {
        // The whole point: a crashed OpenRC service refuses `start` until its
        // stale state is cleared.
        let plan = restart_plan(Init::Openrc, "act_runner");
        assert_eq!(plan.len(), 2, "got: {plan:?}");
        assert_eq!(plan[0], vec!["rc-service", "act_runner", "zap"]);
        assert_eq!(plan[1], vec!["rc-service", "act_runner", "start"]);
    }

    #[test]
    fn other_managers_restart_in_one_step() {
        assert_eq!(
            restart_plan(Init::Systemd, "orca.service"),
            vec![vec!["systemctl", "restart", "orca.service"]]
        );
        assert_eq!(restart_plan(Init::Launchd, "com.x").len(), 1);
    }

    #[test]
    fn a_manager_we_cannot_drive_offers_no_plan() {
        // Better an empty plan than a command that silently does nothing.
        assert!(restart_plan(Init::Unraid, "x").is_empty());
        assert!(restart_plan(Init::Unknown, "x").is_empty());
    }
}
