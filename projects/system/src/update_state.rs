//! Update channel state + version pin: path helpers, readers, writers,
//! and the `is_newer_full` semver comparator used to decide pin vetoes
//! and GitHub release ordering.
//!
//! Moved from `server::commands::update` (slices A1 + B2a).

use anyhow::{Context, Result};
use std::path::Path;
use std::path::PathBuf;

// ── Version pin ───────────────────────────────────────────────────────────────

/// Path to the version pin file (`$ORCA_HOME/version-pin`, default `~/.orca/version-pin`).
/// Returns None only if both `ORCA_HOME` and `HOME` are unset (CI sandboxes).
pub fn pin_path() -> Option<PathBuf> {
    Some(files::ops::orca_home()?.join("version-pin"))
}

/// Read the version pin from `$ORCA_HOME/version-pin`. Returns None if absent.
pub fn read_version_pin() -> Option<String> {
    let path = pin_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Write a version pin. The version is stored as-is (caller may include `v` prefix).
pub fn write_version_pin(version: &str) -> Result<()> {
    let path = pin_path().context("no ORCA_HOME or HOME set")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    std::fs::write(&path, format!("{version}\n"))
        .with_context(|| format!("write {}", path.display()))
}

/// Remove the version pin. No-op if not set.
pub fn clear_version_pin() -> Result<()> {
    let path = pin_path().context("no ORCA_HOME or HOME set")?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// Returns `Some(pinned_version)` if `available_version` is newer than the pin
/// and therefore should be blocked. Returns None if there is no pin or the
/// available version is within the pin.
pub fn resolve_pin_veto(available_version: &str) -> Option<String> {
    let pin = read_version_pin()?;
    if is_newer_full(available_version, &pin) {
        Some(pin)
    } else {
        None
    }
}

// ── Channel ───────────────────────────────────────────────────────────────────

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Released tags with no pre-release suffix (e.g. `v0.0.4`).
    Stable,
    /// Stable + `-rc.N` tags.
    Rc,
    /// Local git HEAD; not a GitHub release. No version list available.
    Dev,
}

impl Channel {
    pub fn parse(s: &str) -> Self {
        match s {
            // "prerelease" was the original install.sh value before the
            // vocabulary was harmonized with the enum (2026-05-11). Keep
            // accepting it so existing installations don't silently
            // downgrade to stable on next `orca update`.
            "rc" | "prerelease" => Self::Rc,
            "dev" => Self::Dev,
            _ => Self::Stable,
        }
    }

    pub fn as_marker(&self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Rc => "rc",
            Self::Dev => "dev",
        }
    }

    /// Infer the channel implied by a version string. `0.0.6-rc.9` → Rc,
    /// `0.0.6-dev.0` → Dev, `0.0.6` → Stable. Used to choose the effective
    /// channel when the stored pref disagrees with the running binary —
    /// e.g. a host installed via the rc.9 release artifact but with no
    /// channel marker written would otherwise report itself as stable and
    /// trigger a phantom "downgrade available" badge.
    pub fn from_version(v: &str) -> Self {
        let s = v.trim_start_matches('v');
        if s.contains("-dev") {
            Self::Dev
        } else if s.contains("-rc") {
            Self::Rc
        } else {
            Self::Stable
        }
    }

    pub fn accepts(&self, tag: &str) -> bool {
        match self {
            // stable: only tags with no pre-release suffix
            Self::Stable => !tag.contains('-'),
            // rc: stable + rc tags
            Self::Rc => !tag.contains('-') || tag.contains("-rc."),
            // dev: no released tags — git HEAD only
            Self::Dev => false,
        }
    }
}

/// Path to the channel marker file (`$ORCA_HOME/channel`, default `~/.orca/channel`).
/// Returns None only if both `ORCA_HOME` and `HOME` are unset (CI sandboxes).
pub fn channel_marker_path() -> Option<PathBuf> {
    Some(files::ops::orca_home()?.join("channel"))
}

/// Read the channel marker written by `install.sh` (or a prior `orca update`).
/// Returns None if the file doesn't exist or can't be read; callers fall back to Stable.
pub fn read_channel_marker() -> Option<Channel> {
    let path = channel_marker_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(Channel::parse(trimmed))
}

/// Write the channel marker. Best-effort: errors are returned but callers
/// typically log-and-continue (marker drift is recoverable on next install).
pub fn write_channel_marker(ch: &Channel) -> Result<()> {
    let path = channel_marker_path().context("no ORCA_HOME or HOME set")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let content = format!("{}\n", ch.as_marker());
    if Path::new(&path).exists()
        && std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str())
    {
        return Ok(()); // already up to date — no-op
    }
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Resolve the channel to use for an `orca update` invocation:
/// 1. Non-empty explicit input → parse that.
/// 2. Empty input → read the channel marker.
/// 3. No marker → Stable.
pub fn resolve_channel(explicit: &str) -> Channel {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        return Channel::parse(explicit);
    }
    read_channel_marker().unwrap_or(Channel::Stable)
}

// ── Semver comparator ─────────────────────────────────────────────────────────

/// Full semver comparison that handles pre-release suffixes (rc).
/// Returns true if `a` is strictly newer than `b`.
/// Pre-release ordering within same core: any unknown < rc < stable.
pub fn is_newer_full(a: &str, b: &str) -> bool {
    let a = a.trim_start_matches('v');
    let b = b.trim_start_matches('v');

    fn split_pre(s: &str) -> (&str, &str) {
        match s.find('-') {
            Some(idx) => (&s[..idx], &s[idx + 1..]),
            None => (s, ""),
        }
    }

    let (a_core, a_pre) = split_pre(a);
    let (b_core, b_pre) = split_pre(b);

    let parse_core = |s: &str| -> (u64, u64, u64) {
        let mut p = s.split('.').map(|x| x.parse::<u64>().unwrap_or(0));
        (
            p.next().unwrap_or(0),
            p.next().unwrap_or(0),
            p.next().unwrap_or(0),
        )
    };

    let (ac, bc) = (parse_core(a_core), parse_core(b_core));
    if ac != bc {
        return ac > bc;
    }

    // Only two pre-release kinds exist in this project: stable (empty suffix)
    // and rc. No alpha/beta — anything unrecognized is treated as older than
    // rc so it can never out-rank a real release.
    let pre_kind = |s: &str| -> u64 {
        if s.is_empty() {
            2
        } else if s.starts_with("rc") {
            1
        } else {
            0
        }
    };
    let pre_num = |s: &str| -> u64 {
        s.split('.')
            .next_back()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0)
    };

    let (ak, an) = (pre_kind(a_pre), pre_num(a_pre));
    let (bk, bn) = (pre_kind(b_pre), pre_num(b_pre));
    (ak, an) > (bk, bn)
}

/// "Is `latest` strictly newer than `current` for update-available purposes?"
///
/// Wraps [`is_newer_full`] with one extra step: a dev-build suffix on
/// `current` (see `build.rs::resolve_version` — `-dev+g<sha>` plus optional
/// trailing `.dirty`) is stripped so a dirty/uncommitted build of `rc.14`
/// is not reported as "older than" the released `rc.14`. Without this,
/// list-view (`pod.list` row) and detail-view (`system.update`) drift —
/// one would show no update and the other would falsely show an update on
/// the same peer.
pub fn is_update_available(current: &str, latest: &str) -> bool {
    fn strip_dev(v: &str) -> &str {
        let v = v.strip_suffix(".dirty").unwrap_or(v);
        match v.find("-dev+g") {
            Some(idx) => &v[..idx],
            None => v,
        }
    }
    let cur = strip_dev(current.trim().trim_start_matches('v'));
    let lat = latest.trim().trim_start_matches('v');
    if cur.is_empty() || lat.is_empty() {
        return false;
    }
    is_newer_full(lat, cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn isolated_orca_home(scenario: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests touching ORCA_HOME are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", dir.path());
            std::env::set_var("ORCA_TEST_SCENARIO", scenario);
        }
        dir
    }

    // ── Channel::parse ────────────────────────────────────────────────────────

    #[test]
    fn channel_from_str_known() {
        assert_eq!(Channel::parse("stable"), Channel::Stable);
        assert_eq!(Channel::parse("rc"), Channel::Rc);
        assert_eq!(Channel::parse("dev"), Channel::Dev);
    }

    #[test]
    fn channel_from_str_unknown_defaults_to_stable() {
        assert_eq!(Channel::parse(""), Channel::Stable);
        assert_eq!(Channel::parse("nightly"), Channel::Stable);
        assert_eq!(Channel::parse("STABLE"), Channel::Stable);
    }

    #[test]
    fn channel_parses_legacy_prerelease_as_rc() {
        assert_eq!(Channel::parse("prerelease"), Channel::Rc);
    }

    #[test]
    fn channel_as_marker_round_trips() {
        for ch in [Channel::Stable, Channel::Rc, Channel::Dev] {
            assert_eq!(Channel::parse(ch.as_marker()), ch);
        }
    }

    // ── Channel::accepts ──────────────────────────────────────────────────────

    #[test]
    fn stable_accepts_only_clean_tags() {
        assert!(Channel::Stable.accepts("v1.0.0"));
        assert!(!Channel::Stable.accepts("v1.0.0-rc.1"));
    }

    #[test]
    fn rc_accepts_stable_and_rc() {
        assert!(Channel::Rc.accepts("v1.0.0"));
        assert!(Channel::Rc.accepts("v1.0.0-rc.1"));
        assert!(Channel::Rc.accepts("v1.0.0-rc.99"));
    }

    #[test]
    fn dev_accepts_no_released_tags() {
        // Dev channel = local git HEAD; no GitHub releases are part of it.
        assert!(!Channel::Dev.accepts("v1.0.0"));
        assert!(!Channel::Dev.accepts("v1.0.0-rc.1"));
    }

    // ── channel marker readers ────────────────────────────────────────────────

    #[test]
    #[serial(env)]
    fn read_channel_marker_returns_none_when_missing() {
        let _dir = isolated_orca_home("missing");
        assert!(read_channel_marker().is_none());
    }

    #[test]
    #[serial(env)]
    fn read_channel_marker_accepts_legacy_prerelease() {
        let dir = isolated_orca_home("legacy");
        std::fs::write(dir.path().join("channel"), "prerelease\n").unwrap();
        assert_eq!(read_channel_marker(), Some(Channel::Rc));
    }

    #[test]
    #[serial(env)]
    fn read_channel_marker_empty_file_returns_none() {
        let dir = isolated_orca_home("marker_empty");
        std::fs::write(dir.path().join("channel"), "\n").unwrap();
        assert!(read_channel_marker().is_none());
    }

    #[test]
    #[serial(env)]
    fn read_channel_marker_reads_written_value() {
        let dir = isolated_orca_home("marker_read");
        std::fs::write(dir.path().join("channel"), "rc\n").unwrap();
        assert_eq!(read_channel_marker(), Some(Channel::Rc));
    }

    // ── version pin reader ────────────────────────────────────────────────────

    #[test]
    #[serial(env)]
    fn read_version_pin_returns_none_when_absent() {
        let _dir = isolated_orca_home("pin_absent");
        assert!(read_version_pin().is_none());
    }

    #[test]
    #[serial(env)]
    fn read_version_pin_reads_trimmed_value() {
        let dir = isolated_orca_home("pin_read");
        std::fs::write(dir.path().join("version-pin"), "v0.0.4-rc.1\n").unwrap();
        assert_eq!(read_version_pin(), Some("v0.0.4-rc.1".to_string()));
    }

    #[test]
    #[serial(env)]
    fn read_version_pin_returns_none_for_empty_file() {
        let dir = isolated_orca_home("pin_empty");
        std::fs::write(dir.path().join("version-pin"), "   \n").unwrap();
        assert!(read_version_pin().is_none());
    }

    // ── path helpers ──────────────────────────────────────────────────────────

    #[test]
    #[serial(env)]
    fn pin_path_uses_orca_home() {
        let dir = isolated_orca_home("pin_path");
        let p = pin_path().expect("pin_path");
        assert_eq!(p, dir.path().join("version-pin"));
    }

    #[test]
    #[serial(env)]
    fn channel_marker_path_uses_orca_home() {
        let dir = isolated_orca_home("ch_path");
        let p = channel_marker_path().expect("channel_marker_path");
        assert_eq!(p, dir.path().join("channel"));
    }

    // ── is_newer_full ─────────────────────────────────────────────────────────

    #[test]
    fn is_newer_full_stable_vs_stable() {
        assert!(is_newer_full("1.0.1", "1.0.0"));
        assert!(!is_newer_full("1.0.0", "1.0.0"));
        assert!(!is_newer_full("1.0.0", "1.0.1"));
    }

    #[test]
    fn is_newer_full_stable_beats_rc() {
        assert!(is_newer_full("0.0.4", "0.0.4-rc.3"));
        assert!(!is_newer_full("0.0.4-rc.3", "0.0.4"));
    }

    #[test]
    fn is_newer_full_rc_ordering() {
        assert!(is_newer_full("0.0.4-rc.3", "0.0.4-rc.1"));
        assert!(is_newer_full("0.0.4-rc.2", "0.0.4-rc.1"));
        assert!(!is_newer_full("0.0.4-rc.1", "0.0.4-rc.1"));
    }

    #[test]
    fn is_update_available_equal_versions() {
        assert!(!is_update_available("0.0.14", "0.0.14"));
        assert!(!is_update_available("0.0.14", "v0.0.14"));
    }

    #[test]
    fn is_update_available_dirty_current_matches_clean_latest() {
        // build.rs format: <pkg>-dev+g<sha>[.dirty] — same release as v0.0.14
        assert!(!is_update_available("0.0.14-dev+g9409864.dirty", "v0.0.14"));
        assert!(!is_update_available("0.0.14-dev+g9409864", "0.0.14"));
    }

    #[test]
    fn is_update_available_latest_higher() {
        assert!(is_update_available("0.0.13", "v0.0.14"));
        assert!(is_update_available("0.0.13-dev+gabcdef1.dirty", "v0.0.14"));
    }

    #[test]
    fn is_update_available_latest_lower() {
        assert!(!is_update_available("0.0.14", "v0.0.13"));
    }

    #[test]
    fn is_update_available_rc_progression() {
        assert!(is_update_available("0.0.1-rc.13", "v0.0.1-rc.14"));
        assert!(!is_update_available("0.0.1-rc.14", "v0.0.1-rc.13"));
    }

    #[test]
    fn is_update_available_empty_inputs() {
        assert!(!is_update_available("", "v0.0.14"));
        assert!(!is_update_available("0.0.14", ""));
    }

    #[test]
    fn is_newer_full_v_prefix_stripped() {
        assert!(is_newer_full("v0.0.4-rc.3", "v0.0.4-rc.1"));
        assert!(!is_newer_full("v0.0.4-rc.1", "v0.0.4-rc.1"));
    }

    // ── writers + veto (native feature only — they use anyhow) ────────────────

    mod native_tests {
        use super::*;
        use serial_test::serial;

        #[test]
        #[serial(env)]
        fn write_then_read_channel_marker_round_trips() {
            let _dir = isolated_orca_home("write");
            write_channel_marker(&Channel::Rc).unwrap();
            assert_eq!(read_channel_marker(), Some(Channel::Rc));
            write_channel_marker(&Channel::Stable).unwrap();
            assert_eq!(read_channel_marker(), Some(Channel::Stable));
        }

        #[test]
        #[serial(env)]
        fn resolve_channel_explicit_wins_over_marker() {
            let _dir = isolated_orca_home("explicit");
            write_channel_marker(&Channel::Stable).unwrap();
            assert_eq!(resolve_channel("rc"), Channel::Rc);
        }

        #[test]
        #[serial(env)]
        fn resolve_channel_empty_reads_marker() {
            let _dir = isolated_orca_home("empty");
            write_channel_marker(&Channel::Rc).unwrap();
            assert_eq!(resolve_channel(""), Channel::Rc);
            assert_eq!(resolve_channel("  "), Channel::Rc);
        }

        #[test]
        #[serial(env)]
        fn resolve_channel_empty_falls_back_to_stable() {
            let _dir = isolated_orca_home("fallback");
            assert_eq!(resolve_channel(""), Channel::Stable);
        }

        #[test]
        #[serial(env)]
        fn write_channel_marker_noop_when_same() {
            let _dir = isolated_orca_home("marker_noop");
            write_channel_marker(&Channel::Rc).unwrap();
            write_channel_marker(&Channel::Rc).unwrap();
            assert_eq!(read_channel_marker(), Some(Channel::Rc));
        }

        #[test]
        #[serial(env)]
        fn write_then_read_version_pin_round_trips() {
            let _dir = isolated_orca_home("pin_write");
            write_version_pin("v0.0.4-rc.1").unwrap();
            assert_eq!(read_version_pin(), Some("v0.0.4-rc.1".to_string()));
        }

        #[test]
        #[serial(env)]
        fn clear_version_pin_removes_file() {
            let _dir = isolated_orca_home("pin_clear");
            write_version_pin("v0.0.4-rc.1").unwrap();
            clear_version_pin().unwrap();
            assert!(read_version_pin().is_none());
        }

        #[test]
        #[serial(env)]
        fn resolve_pin_veto_blocks_newer_version() {
            let _dir = isolated_orca_home("pin_veto");
            write_version_pin("v0.0.4-rc.1").unwrap();
            assert_eq!(
                resolve_pin_veto("0.0.4-rc.3"),
                Some("v0.0.4-rc.1".to_string())
            );
        }

        #[test]
        #[serial(env)]
        fn resolve_pin_veto_passes_within_pin() {
            let _dir = isolated_orca_home("pin_pass");
            write_version_pin("v0.0.4-rc.3").unwrap();
            assert!(resolve_pin_veto("0.0.4-rc.1").is_none());
        }
    }
}
