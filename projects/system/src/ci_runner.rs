//! Health of the Gitea Actions runner this host runs, if it runs one.
//!
//! The half of #631 that [`crate::supervisor`] does not cover. A runner can be
//! *up and still broken*: the original incident was a missing
//! `$HOME/.cache/actcache` directory, which made act_runner's cache server 500
//! on every request and cancelled every image publish, while the daemon itself
//! reported perfectly healthy. Liveness alone would have said "fine".
//!
//! Everything here reads files the runner already maintains. Nothing is
//! inferred from process state, and **the registration token is never read into
//! a report** — `.runner` holds a live credential next to the fields worth
//! surfacing, so the parse takes only what it needs.

use std::path::{Path, PathBuf};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where act_runner keeps its registration and config.
pub const DEFAULT_CONFIG_DIR: &str = "/etc/act_runner";

/// act_runner's built-in cache directory when `cache.dir` is unset — the exact
/// path whose absence caused the outage.
pub const DEFAULT_CACHE_SUBPATH: &str = ".cache/actcache";

/// What a runner says about itself. Deliberately a subset of `.runner`:
/// `token` and `uuid` are omitted because a health report is read by more eyes
/// than a credential should be.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub id: i64,
    pub name: String,
    /// Bare labels, as `runs-on` matches them.
    pub labels: Vec<String>,
    pub address: String,
}

/// Registration as it sits on disk. Private so the token cannot escape: the
/// field is not even declared, so nothing can read it off this struct.
#[derive(Deserialize)]
struct RunnerFile {
    id: i64,
    name: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    address: String,
}

/// Parse `.runner`.
///
/// Labels are stored as `<label>:<runner-image>` (e.g.
/// `ubuntu-latest:docker://gitea/runner-images:ubuntu-latest`) but a workflow's
/// `runs-on` matches only the part before the FIRST colon. Reporting the raw
/// string would mean an operator comparing a blocked job's `runs-on` against
/// this list sees no match and concludes the runner cannot serve it.
pub fn parse_registration(raw: &str) -> Result<Registration> {
    #[allow(clippy::disallowed_types)]
    let f: RunnerFile = serde_json::from_str(raw)?;
    Ok(Registration {
        id: f.id,
        name: f.name,
        labels: f
            .labels
            .iter()
            .map(|l| l.split(':').next().unwrap_or(l).to_string())
            .collect(),
        address: f.address,
    })
}

#[derive(Deserialize, Default)]
struct CacheSection {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    cache: CacheSection,
}

/// Where this runner's cache lives, or `None` when caching is off.
///
/// `cache.enabled` defaults to TRUE when the key is absent — matching
/// act_runner — so an empty config still yields a directory to check. Assuming
/// the opposite would silently skip the check on exactly the default config the
/// broken host had.
pub fn cache_dir(config_yaml: &str, runner_home: Option<&Path>) -> Result<Option<PathBuf>> {
    let cfg: ConfigFile = if config_yaml.trim().is_empty() {
        ConfigFile::default()
    } else {
        utils::yaml::from_str(config_yaml)?
    };
    if !cfg.cache.enabled.unwrap_or(true) {
        return Ok(None);
    }
    Ok(match cfg.cache.dir.as_deref() {
        Some(d) if !d.trim().is_empty() => Some(PathBuf::from(d.trim())),
        // The default is relative to the RUNNER user's home. Unknown home =>
        // unknown path; guessing one would invent a finding.
        _ => runner_home.map(|h| h.join(DEFAULT_CACHE_SUBPATH)),
    })
}

/// State of the cache directory — the signal that was missing.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheHealth {
    /// Caching is switched off; there is nothing to break.
    Disabled,
    /// Present and writable.
    Ok,
    /// Configured but absent — act_runner will 500 on every cache request and
    /// cancel builds that use it, while reporting itself healthy.
    Missing,
    /// Present but this process cannot write it.
    NotWritable,
    /// Could not be examined — typically the runner runs as another user whose
    /// home orca cannot traverse. Distinct from [`CacheHealth::Missing`] on
    /// purpose: "I cannot look" and "it is not there" are different claims, and
    /// reporting the second for the first is a false alarm on a healthy runner.
    Unknown(String),
}

/// Probe the cache directory by WRITING to it.
///
/// Existence is not the property that matters — act_runner has to open
/// `bolt.db` there. A stat says the path is present; only a write says the
/// runner can actually use it.
pub fn probe_cache(dir: Option<&Path>) -> CacheHealth {
    let Some(dir) = dir else {
        return CacheHealth::Disabled;
    };
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return CacheHealth::Missing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CacheHealth::Missing,
        // Anything else — permission, most often — is not knowledge of absence.
        Err(e) => return CacheHealth::Unknown(e.to_string()),
    }
    let probe = dir.join(".orca-cache-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            _ = std::fs::remove_file(&probe);
            CacheHealth::Ok
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => CacheHealth::NotWritable,
        Err(e) => CacheHealth::Unknown(e.to_string()),
    }
}

/// Real uid from a `/proc/<pid>/status` block (`Uid:\treal\teff\tsaved\tfs`).
pub fn parse_proc_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Home directory for `uid` from `/etc/passwd` content.
pub fn home_for_uid(passwd: &str, uid: u32) -> Option<PathBuf> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        // name:passwd:uid:gid:gecos:home:shell
        (f.len() >= 6 && f[2].parse::<u32>().ok()? == uid).then(|| PathBuf::from(f[5]))
    })
}

/// Home directory of the user actually running act_runner on this host.
///
/// Not the orca daemon's `$HOME`, which is a different user: on the fleet the
/// runner is root (`/root`) while orca runs as `orca` (`/var/lib/orca`). Using
/// orca's home would resolve the default cache path to a directory that was
/// never supposed to exist and report a healthy runner as broken.
///
/// `/proc/<pid>/status` and `/etc/passwd` are both world-readable, so this
/// works unprivileged even though the cache directory itself may not be.
#[cfg(target_os = "linux")]
pub fn runner_home() -> Option<PathBuf> {
    let procs = std::fs::read_dir("/proc").ok()?;
    for entry in procs.flatten() {
        let pid_dir = entry.path();
        let cmdline = std::fs::read(pid_dir.join("cmdline")).unwrap_or_default();
        // argv is NUL-separated; the daemon's argv0 ends in `act_runner`.
        let argv0 = cmdline.split(|b| *b == 0).next().unwrap_or_default();
        if !String::from_utf8_lossy(argv0).ends_with("act_runner") {
            continue;
        }
        let status = std::fs::read_to_string(pid_dir.join("status")).ok()?;
        let uid = parse_proc_uid(&status)?;
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        return home_for_uid(&passwd, uid);
    }
    None
}

/// No `/proc` to read: report unknown rather than guessing a home.
#[cfg(not(target_os = "linux"))]
pub fn runner_home() -> Option<PathBuf> {
    None
}

/// Everything orca can say about this host's runner.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct RunnerHealth {
    pub registration: Registration,
    pub cache: CacheHealth,
    /// Resolved cache directory, when caching is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<String>,
}

/// Read this host's runner health, or `None` when no runner is installed.
///
/// A host with no `.runner` is not a defect — most of the fleet runs no runner
/// — so absence returns `None` rather than an error a caller must special-case.
pub fn local_runner(config_dir: &Path, runner_home: Option<&Path>) -> Result<Option<RunnerHealth>> {
    let reg_path = config_dir.join(".runner");
    if !reg_path.is_file() {
        return Ok(None);
    }
    let registration = parse_registration(&std::fs::read_to_string(&reg_path)?)?;
    let raw = std::fs::read_to_string(config_dir.join("config.yaml")).unwrap_or_default();
    let dir = cache_dir(&raw, runner_home)?;
    // An unresolvable home is only a problem when the path depends on it: an
    // explicit `cache.dir` is absolute and needs no home at all.
    let cache = match (&dir, runner_home) {
        (Some(_), _) => probe_cache(dir.as_deref()),
        (None, _) if is_cache_disabled(&raw) => CacheHealth::Disabled,
        (None, _) => CacheHealth::Unknown(
            "cannot determine which user runs act_runner, so its default cache path is unknown"
                .to_string(),
        ),
    };
    Ok(Some(RunnerHealth {
        cache,
        cache_dir: dir.map(|d| d.display().to_string()),
        registration,
    }))
}

/// Whether the config explicitly switches caching off.
fn is_cache_disabled(config_yaml: &str) -> bool {
    utils::yaml::from_str::<ConfigFile>(config_yaml)
        .map(|c| c.cache.enabled == Some(false))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape taken from a real `.runner`, with the credential and the real
    // address replaced — a fixture must not carry either.
    const RUNNER_JSON: &str = r#"{
      "WARNING": "generated; do not edit",
      "id": 7,
      "uuid": "cb9c5fd2-0000-0000-0000-000000000000",
      "name": "example-runner",
      "token": "SHOULD-NEVER-BE-REPORTED",
      "address": "http://10.0.0.20:3000",
      "labels": [
        "ubuntu-latest:docker://gitea/runner-images:ubuntu-latest",
        "ubuntu-22.04:docker://gitea/runner-images:ubuntu-22.04"
      ],
      "ephemeral": false
    }"#;

    #[test]
    fn labels_are_reported_the_way_runs_on_matches_them() {
        let r = parse_registration(RUNNER_JSON).unwrap();
        // NOT the raw `ubuntu-latest:docker://...` — an operator comparing a
        // blocked job's `runs-on` against that would see no match.
        assert_eq!(r.labels, vec!["ubuntu-latest", "ubuntu-22.04"]);
        assert_eq!(r.id, 7);
        assert_eq!(r.name, "example-runner");
    }

    #[test]
    fn the_registration_token_never_reaches_the_report() {
        let r = parse_registration(RUNNER_JSON).unwrap();
        #[allow(clippy::disallowed_types)]
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("SHOULD-NEVER-BE-REPORTED"),
            "a live credential leaked into the health report: {json}"
        );
        assert!(!json.contains("uuid"), "got: {json}");
    }

    #[test]
    fn caching_is_on_unless_it_says_otherwise() {
        // The broken host's config: `cache: enabled: true` and no `dir`. Treating
        // an absent `enabled` as off would skip the check on the default config.
        let home = Path::new("/home/x");
        let got = cache_dir("cache:\n  enabled: true\n", Some(home)).unwrap();
        assert_eq!(got, Some(home.join(".cache/actcache")));
        let absent = cache_dir("log:\n  level: info\n", Some(home)).unwrap();
        assert_eq!(absent, Some(home.join(".cache/actcache")));
    }

    #[test]
    fn an_explicit_dir_wins_and_disabled_means_nothing_to_check() {
        let home = Path::new("/home/x");
        assert_eq!(
            cache_dir(
                "cache:\n  enabled: true\n  dir: /var/cache/act\n",
                Some(home)
            )
            .unwrap(),
            Some(PathBuf::from("/var/cache/act"))
        );
        assert_eq!(
            cache_dir("cache:\n  enabled: false\n", Some(home)).unwrap(),
            None
        );
        // An absolute dir needs no home at all.
        assert_eq!(
            cache_dir("cache:\n  dir: /var/cache/act\n", None).unwrap(),
            Some(PathBuf::from("/var/cache/act"))
        );
    }

    #[test]
    fn a_missing_cache_dir_is_the_incident_and_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("not-created");
        assert_eq!(probe_cache(Some(&absent)), CacheHealth::Missing);
        // Control: the same probe on a real directory passes, so Missing is a
        // reading and not a constant.
        assert_eq!(probe_cache(Some(tmp.path())), CacheHealth::Ok);
        assert_eq!(probe_cache(None), CacheHealth::Disabled);
    }

    #[test]
    fn the_probe_writes_rather_than_stats() {
        // act_runner must open bolt.db in there; presence alone is not the
        // property that matters. Proven by leaving no probe file behind.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(probe_cache(Some(tmp.path())), CacheHealth::Ok);
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftovers.is_empty(), "probe left a file behind");
    }

    #[test]
    fn an_unknown_runner_user_yields_unknown_not_a_false_alarm() {
        // The bug this guards: orca runs as `orca` (/var/lib/orca) while
        // act_runner runs as root (/root). Resolving the default cache path
        // against the WRONG home names a directory that never existed, and
        // reporting that as Missing calls a healthy runner broken.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".runner"), RUNNER_JSON).unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "cache:\n  enabled: true\n").unwrap();

        let health = local_runner(tmp.path(), None).unwrap().unwrap();
        assert!(
            matches!(health.cache, CacheHealth::Unknown(_)),
            "got: {:?}",
            health.cache
        );
        assert!(health.cache_dir.is_none());
    }

    #[test]
    fn disabled_caching_is_known_good_even_without_a_home() {
        // Nothing to look for, so an unresolvable home does not make it unknown.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".runner"), RUNNER_JSON).unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "cache:\n  enabled: false\n").unwrap();
        let health = local_runner(tmp.path(), None).unwrap().unwrap();
        assert_eq!(health.cache, CacheHealth::Disabled);
    }

    #[test]
    fn the_runner_uid_and_home_come_from_the_files_the_os_already_publishes() {
        // Real shapes: /proc/<pid>/status and /etc/passwd, both world-readable,
        // which is what lets an unprivileged orca resolve root's home.
        assert_eq!(
            parse_proc_uid("Name:\tact_runner\nUid:\t0\t0\t0\t0\n"),
            Some(0)
        );
        assert_eq!(parse_proc_uid("Uid:\t1000\t1000\t1000\t1000"), Some(1000));
        assert_eq!(parse_proc_uid("Name:\tx\n"), None);

        let passwd =
            "root:x:0:0:root:/root:/bin/sh\norca:x:1001:1001::/var/lib/orca:/sbin/nologin\n";
        assert_eq!(home_for_uid(passwd, 0), Some(PathBuf::from("/root")));
        assert_eq!(
            home_for_uid(passwd, 1001),
            Some(PathBuf::from("/var/lib/orca"))
        );
        assert_eq!(home_for_uid(passwd, 4242), None);
    }

    #[test]
    fn a_host_with_no_runner_is_not_a_defect() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            local_runner(tmp.path(), Some(tmp.path()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_runner_with_no_config_still_gets_its_cache_checked() {
        // Missing config.yaml means act_runner's defaults, and those include
        // caching — skipping the check here would miss the default install.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".runner"), RUNNER_JSON).unwrap();
        let health = local_runner(tmp.path(), Some(tmp.path())).unwrap().unwrap();
        assert_eq!(health.cache, CacheHealth::Missing);
        assert!(health.cache_dir.unwrap().ends_with(".cache/actcache"));
    }
}
