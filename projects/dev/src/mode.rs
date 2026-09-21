//! Dev mode supervisor — clones the orca git repo on demand and runs
//! `cargo watch -x 'run -- daemon'` so a developer's edits hot-reload the
//! local daemon. Driven by `orca dev enable / disable / sync` CLI verbs.
//!
//! Relocated 2026-06-01 from `system::dev`. The fleet-facing URL-fetch
//! path (peer fetches binary from a configured URL) stays in system —
//! that's a system primitive, not a dev concern.

use anyhow::{Context, Result};
use files::ops::chmod_dir_owner_only;
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEV_REPO_SUBDIR: &str = "dev/orca";

/// Isolated dev instance lives here — a full, independent `ORCA_HOME` nested
/// under the prod home so it's discoverable and easy to purge, yet carries its
/// OWN state.json / orca.db / pki / vault. NEVER the prod DB or PKI.
const DEV_INSTANCE_SUBDIR: &str = "dev-instance";

/// The overlay-state file, written into the PROD home so `disable`/`status`/
/// `exec` can find the running dev instance regardless of `$ORCA_HOME` churn.
const DEV_OVERLAY_FILE: &str = "dev-overlay.json";

fn dev_repo_path() -> Option<PathBuf> {
    Some(files::ops::orca_home()?.join(DEV_REPO_SUBDIR))
}

fn dev_pid_path() -> Option<PathBuf> {
    Some(files::ops::orca_home()?.join("dev.pid"))
}

/// Find `cargo` for `dev_enable` — daemon-inherited PATH typically lacks
/// `~/.cargo/bin` because rustup's env hook only runs in interactive shells.
fn resolve_cargo_bin() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("CARGO") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        let p = PathBuf::from(home).join("bin").join("cargo");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".cargo/bin/cargo");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("cargo");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for candidate in [
        "/var/lib/orca/.cargo/bin/cargo",
        "/home/orca/.cargo/bin/cargo",
        "/root/.cargo/bin/cargo",
    ] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn read_dev_pid() -> Option<u32> {
    std::fs::read_to_string(dev_pid_path()?)
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_dev_pid(pid: u32) -> Result<()> {
    let path = dev_pid_path().context("no ORCA_HOME or HOME set")?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, format!("{pid}\n"))?;
    Ok(())
}

fn clear_dev_pid() {
    if let Some(p) = dev_pid_path() {
        _ = std::fs::remove_file(p);
    }
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct DevEnableResult {
    pub repo_path: String,
    pub cloned: bool,
    pub daemon_parked: bool,
}

pub fn cmd_dev_enable(github_token: &str) -> Result<DevEnableResult> {
    use contract::config::APP_REPO_URL;

    let repo = dev_repo_path().context("no ORCA_HOME or HOME")?;

    if let Ok(Some(s)) = utils::state::read()
        && matches!(s.mode, utils::state::DaemonMode::Dev)
        && pid_alive(s.daemon_pid)
    {
        return Ok(DevEnableResult {
            repo_path: repo.to_string_lossy().into(),
            cloned: false,
            daemon_parked: false,
        });
    }

    if let Some(pid) = read_dev_pid()
        && pid_alive(pid)
    {
        let daemon_state = utils::state::read()?;
        let daemon_parked = daemon_state
            .as_ref()
            .map(|s| {
                s.mode == utils::state::DaemonMode::Parked
                    || s.mode == utils::state::DaemonMode::Dev
            })
            .unwrap_or(false);
        return Ok(DevEnableResult {
            repo_path: repo.to_string_lossy().into(),
            cloned: false,
            daemon_parked,
        });
    }

    let cloned = if !repo.exists() {
        if let Some(parent) = repo.parent() {
            std::fs::create_dir_all(parent)?;
            chmod_dir_owner_only(parent)
                .with_context(|| format!("chmod 0700 on dev dir {}", parent.display()))?;
        }
        let clone_url = if github_token.is_empty() {
            APP_REPO_URL.to_string()
        } else if let Some(rest) = APP_REPO_URL.strip_prefix("https://") {
            format!("https://x-access-token:{github_token}@{rest}")
        } else {
            APP_REPO_URL.to_string()
        };
        let status = Command::new("git")
            .args([
                "clone",
                "--depth=1",
                &clone_url,
                repo.to_str().unwrap_or("."),
            ])
            .status()?;
        anyhow::ensure!(
            status.success(),
            "git clone failed (private repo — ensure `github_token` secret is set on this host)"
        );
        true
    } else {
        false
    };

    let daemon_parked = match utils::state::read()? {
        Some(s) if s.mode == utils::state::DaemonMode::Daemon => {
            Command::new("kill")
                .args(["-USR1", &s.daemon_pid.to_string()])
                .status()?;
            wait_for_park(s.daemon_pid)?;
            true
        }
        _ => false,
    };

    let cargo_bin = resolve_cargo_bin()
        .context("locate cargo binary (install rustup and ensure ~/.cargo/bin is reachable)")?;
    let cargo_dir = cargo_bin.parent().unwrap_or(std::path::Path::new("/"));
    let augmented_path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut paths = vec![cargo_dir.to_path_buf()];
            paths.extend(std::env::split_paths(&p));
            std::env::join_paths(paths).context("join PATH")?
        }
        None => cargo_dir.as_os_str().to_owned(),
    };
    // Child is intentionally dropped without wait/kill: std::process::Child does
    // NOT kill on drop, so the cargo-watch process outlives this call by design.
    // Lifecycle is owned via the PID file written below; teardown happens via
    // explicit `kill` in the dev disable path, not Drop.
    let child = Command::new(&cargo_bin)
        .args(["watch", "-x", "run -- daemon"])
        .current_dir(&repo)
        .env("PATH", &augmented_path)
        .env("ORCA_DEV_PARENT_PID", "0")
        // Dev is a STATE, signalled by this env var (see `update::is_dev`).
        // The hot-reloaded daemon inherits it, so it reports mode=dev and the
        // updater won't pull a GitHub release over the local build.
        .env("ORCA_DEV", "1")
        .spawn()?;

    let watch_pid = child.id();
    write_dev_pid(watch_pid)?;

    if daemon_parked && let Ok(Some(mut s)) = utils::state::read() {
        s.active_pid = watch_pid;
        _ = utils::state::write(&s);
    }

    Ok(DevEnableResult {
        repo_path: repo.to_string_lossy().into(),
        cloned,
        daemon_parked,
    })
}

fn wait_for_park(daemon_pid: u32) -> Result<()> {
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(Some(s)) = utils::state::read() {
            if s.daemon_pid == daemon_pid && s.mode == utils::state::DaemonMode::Parked {
                return Ok(());
            }
            if s.daemon_pid != daemon_pid {
                return Ok(());
            }
        }
        if !pid_alive(daemon_pid) {
            return Ok(());
        }
    }
    anyhow::bail!("daemon did not park within 5 s")
}

pub struct DevDisableResult {
    pub dev_process_stopped: bool,
    pub daemon_reclaimed: bool,
}

pub fn cmd_dev_disable() -> Result<DevDisableResult> {
    let dev_process_stopped = if let Some(pid) = read_dev_pid()
        && pid_alive(pid)
    {
        _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        clear_dev_pid();
        true
    } else {
        clear_dev_pid();
        false
    };

    let daemon_reclaimed = match utils::state::read()? {
        Some(s) if s.mode != utils::state::DaemonMode::Daemon => Command::new("kill")
            .args(["-USR2", &s.daemon_pid.to_string()])
            .status()
            .map(|st| st.success())
            .unwrap_or(false),
        _ => false,
    };

    Ok(DevDisableResult {
        dev_process_stopped,
        daemon_reclaimed,
    })
}

pub struct DevSyncResult {
    pub commits_pulled: u32,
    pub already_up_to_date: bool,
    pub detail: String,
}

pub fn cmd_dev_sync() -> Result<DevSyncResult> {
    let repo = dev_repo_path().context("no ORCA_HOME or HOME")?;
    anyhow::ensure!(
        repo.exists(),
        "dev repo not found at {} — run `orca dev enable` first",
        repo.display()
    );

    let out = Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&repo)
        .output()?;

    let detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let combined = if stderr.is_empty() {
        detail.clone()
    } else {
        format!("{detail}\n{stderr}")
    };

    anyhow::ensure!(out.status.success(), "git pull failed: {combined}");

    let already_up_to_date = detail.contains("Already up to date");
    let commits_pulled = if already_up_to_date {
        0
    } else {
        detail.lines().filter(|l| l.starts_with("   ")).count() as u32
    };

    Ok(DevSyncResult {
        commits_pulled,
        already_up_to_date,
        detail: combined,
    })
}

// ── isolated working-checkout overlay (Slice 1) ───────────────────────────────
//
// A LOCAL build from the developer's WORKING CHECKOUT (uncommitted changes and
// all) that supersedes the installed release for live verification — WITHOUT
// endangering prod. It runs under an isolated `ORCA_HOME` (`dev-instance`) on
// its own port block, so the prod daemon keeps running untouched: no park, no
// reclaim, no shared state. This is distinct from `cmd_dev_enable` above, which
// clones HEAD and parks prod for a fleet-style hot-reload.

/// Persisted description of the running dev overlay. Round-trips through
/// `<prod ORCA_HOME>/dev-overlay.json` so lifecycle verbs can find the child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevOverlay {
    /// Working-checkout repo root the dev daemon builds from.
    pub checkout: PathBuf,
    /// Isolated `ORCA_HOME` for the dev instance.
    pub dev_home: PathBuf,
    pub http_port: u16,
    pub https_port: u16,
    pub mesh_port: u16,
    /// PID of the spawned supervisor (cargo-watch or cargo run).
    pub pid: u32,
    /// Process-group id of the supervisor. The supervisor is spawned as its own
    /// group leader (`process_group(0)`), so `pgid == pid` and the whole tree
    /// (cargo-watch → cargo run → dev daemon) shares it — killing `-pgid` reaps
    /// the grandchild daemon even during the initial build/boot window, before it
    /// has registered a pid in `dev_home/state.json`.
    #[serde(default)]
    pub pgid: u32,
    pub started_at: utils::time::Timestamp,
}

fn overlay_path() -> Option<PathBuf> {
    Some(contract::config::orca_home()?.join(DEV_OVERLAY_FILE))
}

fn read_overlay() -> Result<Option<DevOverlay>> {
    let Some(path) = overlay_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)?;
    // A torn/corrupt overlay must not wedge enable/disable/status: warn, drop
    // the bad file, and self-heal by treating it as no overlay.
    match serde_json::from_str(&raw) {
        Ok(overlay) => Ok(Some(overlay)),
        Err(e) => {
            eprintln!(
                "warning: ignoring corrupt dev overlay at {}: {e}",
                path.display()
            );
            clear_overlay();
            Ok(None)
        }
    }
}

fn write_overlay(overlay: &DevOverlay) -> Result<()> {
    let path = overlay_path().context("no ORCA_HOME or HOME set")?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    // Write to a temp sibling then atomically rename, so a crash mid-write can't
    // leave a torn overlay that later reads would choke on.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(overlay)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn clear_overlay() {
    if let Some(p) = overlay_path() {
        _ = std::fs::remove_file(p);
    }
}

/// Default isolated dev home: `<prod ORCA_HOME>/dev-instance`.
fn default_dev_home() -> Option<PathBuf> {
    Some(contract::config::orca_home()?.join(DEV_INSTANCE_SUBDIR))
}

/// The prod REST port this host would resolve, used as the base for dev ports.
/// Reads the port the prod daemon published at bind time, else the const.
fn prod_http_base() -> u16 {
    if let Some(home) = contract::config::orca_home()
        && let Ok(raw) = std::fs::read_to_string(home.join("http.port"))
        && let Ok(p) = raw.trim().parse::<u16>()
    {
        return p;
    }
    contract::config::APP_REST_HTTP_PORT
}

fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

/// Allocate a contiguous, fully-free dev port triple `(http, https, mesh)` one
/// 1000-block above prod. Scans blocks of 10 upward so a second dev instance —
/// or a socket still in TIME_WAIT — can't collide. Setting all three env ports
/// on the child keeps its mesh/https off prod's, not just its REST port.
fn allocate_dev_ports(base: u16) -> Result<(u16, u16, u16)> {
    let start = base.saturating_add(1000);
    let mut http = start;
    for _ in 0..64 {
        // Guard the triple against a high base port wrapping/overflowing u16.
        let (Some(https), Some(mesh)) = (http.checked_add(1), http.checked_add(2)) else {
            anyhow::bail!("dev port base {http} too high for a contiguous triple");
        };
        if port_is_free(http) && port_is_free(https) && port_is_free(mesh) {
            return Ok((http, https, mesh));
        }
        http = http
            .checked_add(10)
            .context("ran out of u16 port space scanning for a free dev port triple")?;
    }
    anyhow::bail!("no free dev port triple found near {start}")
}

/// Resolve the working-checkout repo root. `--path` if given, else the git
/// toplevel of the cwd. Errors clearly if it's not a git repo or not an orca
/// checkout.
fn resolve_checkout_root(path: Option<&Path>) -> Result<PathBuf> {
    let start = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("resolve current dir")?,
    };
    let start_str = start.to_str().context("checkout path is not valid UTF-8")?;
    let out = Command::new("git")
        .args(["-C", start_str, "rev-parse", "--show-toplevel"])
        // Strip any inherited git context (hooks/tooling export these) so we
        // resolve the CWD's repo, not the outer one that invoked us.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .context("run git rev-parse")?;
    anyhow::ensure!(
        out.status.success(),
        "{} is not inside a git repository",
        start.display()
    );
    let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    anyhow::ensure!(
        root.join("Cargo.toml").exists() && root.join("projects/server/Cargo.toml").exists(),
        "{} is not an orca checkout (missing projects/server)",
        root.display()
    );
    Ok(root)
}

/// Does `cargo watch` resolve? Falls back to a one-shot `cargo run` if not.
fn has_cargo_watch(cargo_bin: &Path) -> bool {
    Command::new(cargo_bin)
        .args(["watch", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Bootstrap the isolated instance's own vault dirs + PKI + CLI cert under
/// `dev_home`. The DB itself is created by the dev daemon on first boot
/// (migrations run against `dev_home/orca.db`). Idempotent; returns whether a
/// fresh CA had to be minted. NEVER touches prod PKI — `dev_home` is a distinct
/// state root.
fn bootstrap_dev_home(dev_home: &Path) -> Result<bool> {
    std::fs::create_dir_all(dev_home)?;
    chmod_dir_owner_only(dev_home)
        .with_context(|| format!("chmod 0700 on dev home {}", dev_home.display()))?;
    for sub in ["memory", "logs/sessions"] {
        std::fs::create_dir_all(dev_home.join(sub))?;
    }
    let pki_dir = dev_home.join(contract::config::APP_PKI_DIR);
    let fresh = !utils::pki::ca_cert_path(&pki_dir).exists();
    utils::pki::init(&pki_dir).context("init dev PKI")?;
    if !utils::pki::cli_client_cert_path(&pki_dir).exists() {
        utils::pki::issue_cli_client_cert(&pki_dir, "dev").context("issue dev CLI cert")?;
    }
    Ok(fresh)
}

pub struct DevOverlayResult {
    pub checkout: String,
    pub dev_home: String,
    pub http_port: u16,
    pub pid: u32,
    pub used_cargo_watch: bool,
    pub bootstrapped: bool,
}

/// Bring up the isolated dev daemon from the working checkout. Prod is left
/// running and untouched — a different port block + home means no park.
pub fn cmd_dev_overlay_enable(path: Option<&Path>) -> Result<DevOverlayResult> {
    if let Some(existing) = read_overlay()?
        && pid_alive(existing.pid)
    {
        anyhow::bail!(
            "dev overlay already running (pid {}, port {}) — run `orca dev disable` first",
            existing.pid,
            existing.http_port
        );
    }

    let checkout = resolve_checkout_root(path)?;
    let dev_home = default_dev_home().context("no ORCA_HOME or HOME to place dev-instance")?;
    // TOCTOU: `allocate_dev_ports` scans for a free triple, but the child binds
    // them a moment later — a racing process could grab one in between. Acceptable
    // on a single-user dev box; a lost race surfaces as a child bind failure that
    // crashes the dev daemon (not prod), which `disable` then cleans up.
    let (http, https, mesh) = allocate_dev_ports(prod_http_base())?;
    let bootstrapped = bootstrap_dev_home(&dev_home)?;

    let cargo_bin = resolve_cargo_bin()
        .context("locate cargo binary (install rustup and ensure ~/.cargo/bin is reachable)")?;
    let cargo_dir = cargo_bin.parent().unwrap_or(std::path::Path::new("/"));
    let augmented_path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut paths = vec![cargo_dir.to_path_buf()];
            paths.extend(std::env::split_paths(&p));
            std::env::join_paths(paths).context("join PATH")?
        }
        None => cargo_dir.as_os_str().to_owned(),
    };

    let used_cargo_watch = has_cargo_watch(&cargo_bin);
    // Child is intentionally dropped without wait/kill (std::process::Child does
    // NOT kill on drop): the supervisor outlives this call by design and is torn
    // down via the overlay pgid in `cmd_dev_overlay_disable`, not Drop.
    let mut cmd = Command::new(&cargo_bin);
    if used_cargo_watch {
        cmd.args(["watch", "-x", "run -- daemon"]);
    } else {
        cmd.args(["run", "--", "daemon"]);
    }
    let child = cmd
        .current_dir(&checkout)
        // Own process group (leader pgid == child pid): lets `disable` signal the
        // whole tree (cargo-watch → cargo run → dev daemon) at once, reaping the
        // grandchild daemon even before it registers in dev_home/state.json.
        .process_group(0)
        .env("PATH", &augmented_path)
        // Isolated home + own port block ⇒ the dev daemon reads/writes its OWN
        // state.json, orca.db and http.port and binds only its own ports.
        .env("ORCA_HOME", &dev_home)
        .env("ORCA_HTTP_PORT", http.to_string())
        .env("ORCA_HTTPS_PORT", https.to_string())
        .env("ORCA_MESH_PORT", mesh.to_string())
        // Dev is a STATE (see `update::is_dev`): the updater won't pull a GitHub
        // release over the local build.
        .env("ORCA_DEV", "1")
        .spawn()
        .context("spawn dev daemon supervisor")?;
    let pid = child.id();
    // Leader's pgid equals its pid under `process_group(0)`.
    let pgid = pid;

    // Persist the overlay AFTER spawn (pid is only known now). If that write
    // fails, never leave an untracked running supervisor: reap the whole group
    // before returning the error.
    if let Err(e) = write_overlay(&DevOverlay {
        checkout: checkout.clone(),
        dev_home: dev_home.clone(),
        http_port: http,
        https_port: https,
        mesh_port: mesh,
        pid,
        pgid,
        started_at: utils::time::now(),
    }) {
        terminate_group(pgid);
        terminate(pid);
        return Err(e).context("record dev overlay (supervisor reaped)");
    }

    Ok(DevOverlayResult {
        checkout: checkout.to_string_lossy().into(),
        dev_home: dev_home.to_string_lossy().into(),
        http_port: http,
        pid,
        used_cargo_watch,
        bootstrapped,
    })
}

/// SIGTERM then (after a ~2.5s grace) SIGKILL a whole process group by pgid.
/// `kill -<sig> -<pgid>` targets every process in the group, so this reaps the
/// supervisor and every descendant it spawned — including a dev daemon still in
/// its initial build/boot window. Best-effort: a dead/empty group just no-ops.
fn terminate_group(pgid: u32) {
    if pgid == 0 {
        return;
    }
    _ = Command::new("kill")
        .args(["-TERM", &format!("-{pgid}")])
        .status();
    for _ in 0..25 {
        // ESRCH once the group is empty → nothing left to signal.
        let alive = Command::new("kill")
            .args(["-0", &format!("-{pgid}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    _ = Command::new("kill")
        .args(["-KILL", &format!("-{pgid}")])
        .status();
}

/// SIGTERM a pid, then SIGKILL after a ~2.5s grace if it's still alive.
fn terminate(pid: u32) {
    _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    for _ in 0..25 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

pub struct DevOverlayDisableResult {
    pub stopped: bool,
    pub purged: bool,
    pub dev_home: Option<String>,
}

/// Stop the dev daemon. Prod is never parked, so there's nothing to reclaim —
/// we just kill the supervisor (and the daemon it spawned, tracked in the dev
/// home's own state.json) and clear the overlay file. State is preserved unless
/// `purge` removes the whole isolated home.
pub fn cmd_dev_overlay_disable(purge: bool) -> Result<DevOverlayDisableResult> {
    let Some(overlay) = read_overlay()? else {
        return Ok(DevOverlayDisableResult {
            stopped: false,
            purged: false,
            dev_home: None,
        });
    };

    let stopped = pid_alive(overlay.pid);
    // Kill the whole process group first: reaps cargo-watch, cargo run, and the
    // grandchild daemon in one shot — even mid-build, before it writes state.json.
    // Fall back to the leader pid for overlays written before pgid was tracked.
    if overlay.pgid != 0 {
        terminate_group(overlay.pgid);
    } else if stopped {
        terminate(overlay.pid);
    }
    // Belt-and-suspenders: if cargo-watch orphaned the daemon out of the group,
    // kill the real daemon via the dev home's own state.json so nothing survives
    // holding the dev port.
    if let Ok(Some(s)) = utils::state::read_from(&overlay.dev_home.join("state.json")) {
        terminate(s.daemon_pid);
        if s.active_pid != s.daemon_pid {
            terminate(s.active_pid);
        }
    }
    clear_overlay();

    let purged = purge && std::fs::remove_dir_all(&overlay.dev_home).is_ok();

    Ok(DevOverlayDisableResult {
        stopped,
        purged,
        dev_home: Some(overlay.dev_home.to_string_lossy().into()),
    })
}

#[derive(Default)]
pub struct DevOverlayStatus {
    pub running: bool,
    pub checkout: Option<String>,
    pub dev_home: Option<String>,
    pub http_port: Option<u16>,
    pub pid: Option<u32>,
    pub git_rev: Option<String>,
    pub dirty: Option<bool>,
}

/// Short git rev + dirty flag for a checkout, or `(None, None)` if git fails.
fn git_rev_dirty(checkout: &Path) -> (Option<String>, Option<bool>) {
    let dir = match checkout.to_str() {
        Some(d) => d,
        None => return (None, None),
    };
    let rev = Command::new("git")
        .args(["-C", dir, "rev-parse", "--short", "HEAD"])
        // Strip inherited git context so we describe the checkout, not the caller.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let dirty = Command::new("git")
        .args(["-C", dir, "status", "--porcelain"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty());
    (rev, dirty)
}

pub fn cmd_dev_overlay_status() -> Result<DevOverlayStatus> {
    let Some(overlay) = read_overlay()? else {
        return Ok(DevOverlayStatus::default());
    };
    let (git_rev, dirty) = git_rev_dirty(&overlay.checkout);
    Ok(DevOverlayStatus {
        running: pid_alive(overlay.pid),
        checkout: Some(overlay.checkout.to_string_lossy().into()),
        dev_home: Some(overlay.dev_home.to_string_lossy().into()),
        http_port: Some(overlay.http_port),
        pid: Some(overlay.pid),
        git_rev,
        dirty,
    })
}

/// Env pairs that point a command at the dev instance (`orca dev exec`).
pub fn dev_overlay_exec_env() -> Result<Vec<(String, String)>> {
    let overlay =
        read_overlay()?.context("no dev overlay running — run `orca dev enable` first")?;
    Ok(vec![
        (
            "ORCA_HOME".into(),
            overlay.dev_home.to_string_lossy().into(),
        ),
        ("ORCA_HTTP_PORT".into(), overlay.http_port.to_string()),
        ("ORCA_HTTPS_PORT".into(), overlay.https_port.to_string()),
        ("ORCA_MESH_PORT".into(), overlay.mesh_port.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes every test that mutates process-global env vars. All the
    /// path/pid helpers read `ORCA_HOME`/`HOME`/`CARGO*`/`PATH`, which are
    /// shared across the test binary's threads, so they must not run
    /// concurrently. Held for the whole body of each env-mutating test.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Snapshot + restore of the env vars these helpers depend on. Restoring on
    /// drop keeps tests hermetic even if one panics mid-body.
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        const VARS: [&'static str; 5] = ["ORCA_HOME", "HOME", "CARGO", "CARGO_HOME", "PATH"];

        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = Self::VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            let g = Self { _lock: lock, saved };
            for k in Self::VARS {
                g.clear(k);
            }
            g
        }

        fn set(&self, key: &str, val: impl AsRef<std::ffi::OsStr>) {
            // Safety: single-threaded within the ENV_LOCK critical section.
            unsafe { std::env::set_var(key, val) };
        }

        fn clear(&self, key: &str) {
            // Safety: single-threaded within the ENV_LOCK critical section.
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    #[test]
    fn dev_repo_parent_is_chmoded_to_0700() {
        let dir = tempfile::tempdir().unwrap();
        let dev_dir = dir.path().join("dev");
        std::fs::create_dir_all(&dev_dir).unwrap();
        chmod_dir_owner_only(&dev_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&dev_dir).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o700, "dev dir should be 0700, got {mode:o}");
        }
    }

    #[test]
    fn dev_repo_path_uses_orca_home_and_subdir() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());

        let repo = dev_repo_path().expect("repo path with ORCA_HOME set");
        assert_eq!(repo, home.path().join("dev").join("orca"));
        assert!(repo.ends_with("dev/orca"));
    }

    #[test]
    fn dev_repo_path_falls_back_to_home_dot_orca() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());

        let repo = dev_repo_path().expect("repo path with HOME set");
        assert_eq!(repo, home.path().join(".orca").join("dev").join("orca"));
    }

    #[test]
    fn dev_pid_path_is_dev_pid_under_orca_home() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());

        let pid = dev_pid_path().expect("pid path with ORCA_HOME set");
        assert_eq!(pid, home.path().join("dev.pid"));
    }

    #[test]
    fn path_helpers_return_none_without_home() {
        let _env = EnvGuard::new();
        // Both ORCA_HOME and HOME cleared by the guard.
        assert!(dev_repo_path().is_none());
        assert!(dev_pid_path().is_none());
    }

    #[test]
    fn write_then_read_dev_pid_roundtrips() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());

        write_dev_pid(4242).unwrap();
        // Parent dir must exist and file must carry a trailing newline.
        let raw = std::fs::read_to_string(home.path().join("dev.pid")).unwrap();
        assert_eq!(raw, "4242\n");
        assert_eq!(read_dev_pid(), Some(4242));
    }

    #[test]
    fn write_dev_pid_creates_missing_parent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        // Point ORCA_HOME at a not-yet-created nested dir.
        let nested = home.path().join("a").join("b");
        env.set("ORCA_HOME", &nested);

        write_dev_pid(7).unwrap();
        assert!(nested.join("dev.pid").is_file());
        assert_eq!(read_dev_pid(), Some(7));
    }

    #[test]
    fn write_dev_pid_errors_without_home() {
        let _env = EnvGuard::new();
        let err = write_dev_pid(1).unwrap_err();
        assert!(err.to_string().contains("ORCA_HOME") || err.to_string().contains("HOME"));
    }

    #[test]
    fn read_dev_pid_none_when_file_absent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        assert_eq!(read_dev_pid(), None);
    }

    #[test]
    fn read_dev_pid_none_on_garbage_contents() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        std::fs::write(home.path().join("dev.pid"), "not-a-pid\n").unwrap();
        assert_eq!(read_dev_pid(), None);
    }

    #[test]
    fn read_dev_pid_trims_whitespace() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        std::fs::write(home.path().join("dev.pid"), "  915  \n").unwrap();
        assert_eq!(read_dev_pid(), Some(915));
    }

    #[test]
    fn clear_dev_pid_removes_file_and_is_idempotent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let pid_file = home.path().join("dev.pid");
        std::fs::write(&pid_file, "5\n").unwrap();
        assert!(pid_file.exists());

        clear_dev_pid();
        assert!(!pid_file.exists());
        // Second call on an already-absent file must not panic or error.
        clear_dev_pid();
        assert!(!pid_file.exists());
    }

    #[test]
    fn resolve_cargo_bin_prefers_valid_cargo_env() {
        let env = EnvGuard::new();
        // A real regular file standing in for the cargo binary.
        let dir = tempfile::tempdir().unwrap();
        let fake_cargo = dir.path().join("cargo");
        std::fs::write(&fake_cargo, b"#!/bin/sh\n").unwrap();
        env.set("CARGO", &fake_cargo);

        assert_eq!(resolve_cargo_bin(), Some(fake_cargo));
    }

    #[test]
    fn resolve_cargo_bin_ignores_nonfile_cargo_and_uses_cargo_home() {
        let env = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        // CARGO points at a directory (not a file) -> skipped.
        env.set("CARGO", dir.path());
        // CARGO_HOME/bin/cargo is a real file -> chosen.
        let cargo_home = tempfile::tempdir().unwrap();
        let bin = cargo_home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cargo = bin.join("cargo");
        std::fs::write(&cargo, b"x").unwrap();
        env.set("CARGO_HOME", cargo_home.path());

        assert_eq!(resolve_cargo_bin(), Some(cargo));
    }

    #[test]
    fn resolve_cargo_bin_finds_cargo_on_path() {
        let env = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("cargo");
        std::fs::write(&cargo, b"x").unwrap();
        env.set("PATH", dir.path());

        assert_eq!(resolve_cargo_bin(), Some(cargo));
    }

    #[test]
    fn pid_alive_true_for_current_process() {
        // pid_alive shells out to `kill` resolved via PATH; hold ENV_LOCK so a
        // concurrent EnvGuard (which clears PATH) can't run and make `kill`
        // unresolvable mid-test.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_false_for_unused_pid() {
        // Also resolves `kill` via PATH — serialize under ENV_LOCK so a
        // concurrent EnvGuard clearing PATH can't turn the "not found" IO error
        // into a misleading pass (it already returns false, but keep it honest).
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Very high PID is not in use on any realistic system.
        assert!(!pid_alive(4_294_967_294));
    }

    #[test]
    fn cmd_dev_sync_errors_when_repo_missing() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        // ORCA_HOME set but no dev/orca repo cloned under it.
        env.set("ORCA_HOME", home.path());
        // DevSyncResult has no Debug impl, so `.err()` rather than `unwrap_err()`.
        let err = cmd_dev_sync().err().expect("expected repo-missing error");
        assert!(
            err.to_string().contains("dev repo not found"),
            "unexpected error: {err}"
        );
    }

    /// Snapshot + clear the `GIT_*` vars git subprocesses inherit, restoring on
    /// drop. Under a pre-commit hook `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`
    /// point at the outer repo and would corrupt the temp repo these tests drive.
    struct GitEnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl GitEnvGuard {
        const VARS: [&'static str; 3] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"];
        fn new() -> Self {
            let saved = Self::VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            for k in Self::VARS {
                // Safety: caller holds ENV_LOCK via EnvGuard.
                unsafe { std::env::remove_var(k) };
            }
            Self { saved }
        }
    }
    impl Drop for GitEnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    /// Runs `git args...` in `dir` with deterministic identity, asserting success.
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git spawns");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// Clone an origin repo with one initial commit into `ORCA_HOME/dev/orca`.
    /// Returns (origin_dir_tempdir_kept_alive_by_caller, origin_path).
    fn setup_synced_repo(home: &std::path::Path) -> (tempfile::TempDir, PathBuf) {
        let origin_td = tempfile::tempdir().unwrap();
        let origin = origin_td.path().to_path_buf();
        git_in(&origin, &["init", "-q"]);
        git_in(&origin, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let dev_orca = home.join("dev").join("orca");
        std::fs::create_dir_all(dev_orca.parent().unwrap()).unwrap();
        git_in(
            home,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                dev_orca.to_str().unwrap(),
            ],
        );
        (origin_td, origin)
    }

    #[test]
    fn cmd_dev_sync_already_up_to_date_pulls_zero_commits() {
        let env = EnvGuard::new();
        // Restore a PATH so `git` resolves; the guard cleared it.
        env.set("PATH", "/usr/bin:/bin:/usr/local/bin");
        let _git = GitEnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let _origin = setup_synced_repo(home.path());

        let r = cmd_dev_sync().err();
        // Should be Ok, not an error.
        assert!(r.is_none(), "sync should succeed, got {r:?}");
        let r = cmd_dev_sync().expect("second sync Ok");
        assert!(r.already_up_to_date, "no new commits → already up to date");
        assert_eq!(r.commits_pulled, 0);
        assert!(
            r.detail.contains("up to date") || r.detail.contains("up-to-date"),
            "detail should note up-to-date, got {:?}",
            r.detail
        );
    }

    #[test]
    fn cmd_dev_sync_fast_forwards_new_commit() {
        let env = EnvGuard::new();
        env.set("PATH", "/usr/bin:/bin:/usr/local/bin");
        let _git = GitEnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let (_origin_td, origin) = setup_synced_repo(home.path());

        // Add a new commit on origin so the clone can fast-forward.
        std::fs::write(origin.join("f.txt"), "hello\n").unwrap();
        git_in(&origin, &["add", "f.txt"]);
        git_in(&origin, &["commit", "-q", "-m", "add f"]);

        let r = cmd_dev_sync().expect("fast-forward sync Ok");
        assert!(!r.already_up_to_date, "a new commit was pulled");
        assert!(
            r.detail.contains("Fast-forward") || r.detail.contains("Updating"),
            "detail should reflect a fast-forward, got {:?}",
            r.detail
        );
    }

    #[test]
    fn cmd_dev_sync_errors_without_home() {
        let _env = EnvGuard::new();
        // Neither ORCA_HOME nor HOME → dev_repo_path is None → context error.
        // DevSyncResult has no Debug impl, so `.err()` rather than `unwrap_err()`.
        let err = cmd_dev_sync().err().expect("expected no-home error");
        assert!(
            err.to_string().contains("ORCA_HOME") || err.to_string().contains("HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn cmd_dev_disable_with_no_pid_and_no_state_is_clean_noop() {
        // Holds ENV_LOCK via EnvGuard: cmd_dev_disable may shell out to `kill`
        // only when a live pid/state exists — here there is neither, so no
        // process is signalled. state::read honors ORCA_HOME → Ok(None).
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        // No dev.pid file and no daemon state file under this ORCA_HOME.
        let r = cmd_dev_disable().expect("disable with nothing running is Ok");
        assert!(!r.dev_process_stopped, "no pid file → nothing to stop");
        assert!(!r.daemon_reclaimed, "no daemon state → nothing to reclaim");
    }

    #[test]
    fn cmd_dev_disable_clears_stale_pid_file_for_dead_pid() {
        // A pid file pointing at a dead pid: dev_process_stopped stays false
        // (pid not alive) but the stale file is cleared as a side effect.
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        std::fs::write(home.path().join("dev.pid"), "4294967294\n").unwrap();
        let r = cmd_dev_disable().expect("disable Ok");
        assert!(!r.dev_process_stopped, "dead pid → not stopped");
        assert!(
            !home.path().join("dev.pid").exists(),
            "stale pid file must be cleared"
        );
    }

    #[test]
    fn resolve_cargo_bin_uses_home_dot_cargo_when_env_absent() {
        let env = EnvGuard::new();
        // CARGO / CARGO_HOME / PATH are all cleared by the guard, so resolution
        // falls through to the HOME/.cargo/bin/cargo branch.
        let home = tempfile::tempdir().unwrap();
        let cargo = home.path().join(".cargo").join("bin").join("cargo");
        std::fs::create_dir_all(cargo.parent().unwrap()).unwrap();
        std::fs::write(&cargo, b"x").unwrap();
        env.set("HOME", home.path());

        assert_eq!(resolve_cargo_bin(), Some(cargo));
    }

    #[test]
    fn resolve_cargo_bin_returns_none_when_nothing_resolves() {
        let env = EnvGuard::new();
        // All of CARGO/CARGO_HOME/HOME/PATH cleared by the guard, and the
        // hardcoded system fallbacks (/var/lib/orca, /root/.cargo, …) don't
        // exist in the test environment → no cargo can be located.
        let home = tempfile::tempdir().unwrap();
        // HOME points at an empty dir with no .cargo/bin/cargo.
        env.set("HOME", home.path());
        assert_eq!(resolve_cargo_bin(), None);
    }

    /// Build a DaemonState with the given mode and pids for exercising the
    /// early-return branches of `cmd_dev_enable` without spawning cargo-watch.
    fn state_with(mode: utils::state::DaemonMode, daemon_pid: u32) -> utils::state::DaemonState {
        utils::state::DaemonState {
            daemon_pid,
            active_pid: daemon_pid,
            port: 12000,
            mode,
            binary: "/usr/local/bin/orca".to_string(),
            version: "0.1.0".to_string(),
            started_at: utils::time::now(),
        }
    }

    #[test]
    fn cmd_dev_enable_returns_early_when_dev_daemon_already_alive() {
        // State says mode=Dev with a live daemon_pid (this process): the first
        // early-return fires, so nothing is cloned or parked and no cargo-watch
        // is spawned.
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let me = std::process::id();
        utils::state::write(&state_with(utils::state::DaemonMode::Dev, me)).unwrap();

        let r = cmd_dev_enable("").expect("enable is Ok on already-dev state");
        assert!(!r.cloned, "existing repo/state → not cloned");
        assert!(!r.daemon_parked, "already dev → nothing parked");
        assert!(
            r.repo_path.ends_with("dev/orca"),
            "repo_path should point at the dev repo, got {}",
            r.repo_path
        );
    }

    #[test]
    fn cmd_dev_enable_returns_early_on_live_dev_pid_with_parked_daemon() {
        // No live Dev daemon in state (mode=Parked), but a live dev.pid file
        // exists → second early-return fires and reports the daemon as parked.
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let me = std::process::id();
        // Daemon state is Parked (not Dev), so the first branch is skipped.
        utils::state::write(&state_with(utils::state::DaemonMode::Parked, me)).unwrap();
        // A live dev-process pid triggers the second early return.
        std::fs::write(home.path().join("dev.pid"), format!("{me}\n")).unwrap();

        let r = cmd_dev_enable("").expect("enable Ok on live dev pid");
        assert!(!r.cloned);
        assert!(r.daemon_parked, "parked state → daemon_parked true");
    }

    #[test]
    fn cmd_dev_enable_live_dev_pid_reports_unparked_for_plain_daemon() {
        // Live dev.pid but state mode=Daemon → second branch reports
        // daemon_parked=false (daemon still owns the port).
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let me = std::process::id();
        utils::state::write(&state_with(utils::state::DaemonMode::Daemon, me)).unwrap();
        std::fs::write(home.path().join("dev.pid"), format!("{me}\n")).unwrap();

        let r = cmd_dev_enable("").expect("enable Ok");
        assert!(!r.cloned);
        assert!(!r.daemon_parked, "plain daemon → not parked");
    }

    #[test]
    fn dev_enable_result_fields_are_addressable() {
        // Guards the public result struct shape used by the CLI layer.
        let r = DevEnableResult {
            repo_path: "/tmp/x".into(),
            cloned: true,
            daemon_parked: false,
        };
        assert_eq!(r.repo_path, "/tmp/x");
        assert!(r.cloned);
        assert!(!r.daemon_parked);

        let d = DevDisableResult {
            dev_process_stopped: true,
            daemon_reclaimed: false,
        };
        assert!(d.dev_process_stopped);
        assert!(!d.daemon_reclaimed);

        let s = DevSyncResult {
            commits_pulled: 3,
            already_up_to_date: false,
            detail: "pulled".into(),
        };
        assert_eq!(s.commits_pulled, 3);
        assert!(!s.already_up_to_date);
        assert_eq!(s.detail, "pulled");
    }

    // ── isolated overlay (Slice 1) ────────────────────────────────────────────

    #[test]
    fn overlay_path_is_under_orca_home() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        assert_eq!(
            overlay_path().unwrap(),
            home.path().join("dev-overlay.json")
        );
    }

    #[test]
    fn default_dev_home_is_dev_instance_under_orca_home() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        assert_eq!(
            default_dev_home().unwrap(),
            home.path().join("dev-instance")
        );
    }

    #[test]
    fn overlay_roundtrips_through_disk() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());

        let overlay = DevOverlay {
            checkout: PathBuf::from("/src/orca"),
            dev_home: home.path().join("dev-instance"),
            http_port: 13000,
            https_port: 13001,
            mesh_port: 13002,
            pid: 4242,
            pgid: 4242,
            started_at: utils::time::now(),
        };
        write_overlay(&overlay).unwrap();
        let read = read_overlay().unwrap().expect("overlay present");
        assert_eq!(read.checkout, overlay.checkout);
        assert_eq!(read.http_port, 13000);
        assert_eq!(read.https_port, 13001);
        assert_eq!(read.mesh_port, 13002);
        assert_eq!(read.pid, 4242);
        assert_eq!(read.pgid, 4242, "process-group id must round-trip");
    }

    #[test]
    fn read_overlay_self_heals_on_corrupt_file() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let path = home.path().join("dev-overlay.json");
        // Garbage that is not valid DevOverlay JSON.
        std::fs::write(&path, "{ not json").unwrap();
        // Must not error — treated as no overlay and the bad file removed.
        assert!(read_overlay().unwrap().is_none());
        assert!(!path.exists(), "corrupt overlay should be cleared");
    }

    #[test]
    fn write_overlay_leaves_no_temp_file() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        write_overlay(&DevOverlay {
            checkout: PathBuf::from("/src/orca"),
            dev_home: home.path().join("dev-instance"),
            http_port: 13000,
            https_port: 13001,
            mesh_port: 13002,
            pid: 7,
            pgid: 7,
            started_at: utils::time::now(),
        })
        .unwrap();
        // Atomic rename must not leave the temp sibling behind.
        assert!(home.path().join("dev-overlay.json").exists());
        assert!(!home.path().join("dev-overlay.json.tmp").exists());
        assert!(read_overlay().unwrap().is_some());
    }

    #[test]
    fn allocate_dev_ports_errors_on_overflowing_base() {
        // Base near u16::MAX: base+1000 saturates to u16::MAX, so no contiguous
        // triple fits and the checked arithmetic must bail rather than wrap.
        let err = allocate_dev_ports(u16::MAX).unwrap_err();
        assert!(
            err.to_string().contains("too high"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_overlay_none_when_absent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        assert!(read_overlay().unwrap().is_none());
    }

    #[test]
    fn clear_overlay_removes_file_and_is_idempotent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let path = home.path().join("dev-overlay.json");
        std::fs::write(&path, "{}").unwrap();
        clear_overlay();
        assert!(!path.exists());
        clear_overlay(); // second call must not panic
    }

    #[test]
    fn prod_http_base_reads_published_port_else_const() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        // No http.port file → compile-time const.
        assert_eq!(prod_http_base(), contract::config::APP_REST_HTTP_PORT);
        // With a published port → that wins.
        std::fs::write(home.path().join("http.port"), "12345\n").unwrap();
        assert_eq!(prod_http_base(), 12345);
    }

    #[test]
    fn allocate_dev_ports_returns_free_contiguous_triple() {
        // Base+1000 offset, contiguous (http, http+1, http+2), all bindable.
        let (http, https, mesh) = allocate_dev_ports(12000).unwrap();
        assert!(http >= 13000, "dev port is one 1000-block above prod");
        assert_eq!(https, http + 1);
        assert_eq!(mesh, http + 2);
        assert!(port_is_free(http) && port_is_free(https) && port_is_free(mesh));
    }

    #[test]
    fn allocate_dev_ports_skips_occupied_triple() {
        // Occupy the primary http slot so allocation advances to a later block.
        let (http, _, _) = allocate_dev_ports(12000).unwrap();
        let _held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, http)).unwrap();
        let (http2, _, _) = allocate_dev_ports(12000).unwrap();
        assert_ne!(http2, http, "must not reuse the bound port");
    }

    #[test]
    fn resolve_checkout_root_errors_outside_git_repo() {
        // Clear inherited GIT_* (the pre-push hook exports them) so `git -C
        // <tmp> rev-parse` reports the tmp dir's real status, not the outer repo.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _git = GitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_checkout_root(Some(dir.path())).unwrap_err();
        assert!(
            err.to_string().contains("not inside a git repository"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_checkout_root_errors_when_not_orca_checkout() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _git = GitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // A git repo, but with no projects/server → not an orca checkout.
        let ok = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let err = resolve_checkout_root(Some(&root)).unwrap_err();
        assert!(
            err.to_string().contains("not an orca checkout"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_checkout_root_accepts_orca_shaped_repo() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _git = GitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        Command::new("git")
            .args(["-C", root.to_str().unwrap(), "init", "-q"])
            .status()
            .unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(root.join("projects/server")).unwrap();
        std::fs::write(root.join("projects/server/Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(resolve_checkout_root(Some(&root)).unwrap(), root);
    }

    #[test]
    fn cmd_dev_overlay_disable_noop_without_overlay() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let r = cmd_dev_overlay_disable(false).expect("disable Ok with no overlay");
        assert!(!r.stopped);
        assert!(!r.purged);
        assert!(r.dev_home.is_none());
    }

    #[test]
    fn cmd_dev_overlay_status_reports_not_running_without_overlay() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let s = cmd_dev_overlay_status().expect("status Ok");
        assert!(!s.running);
        assert!(s.pid.is_none());
        assert!(s.checkout.is_none());
    }

    #[test]
    fn dev_overlay_exec_env_errors_without_overlay() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let err = dev_overlay_exec_env().unwrap_err();
        assert!(err.to_string().contains("no dev overlay running"));
    }

    #[test]
    fn dev_overlay_exec_env_projects_home_and_ports() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        write_overlay(&DevOverlay {
            checkout: PathBuf::from("/src/orca"),
            dev_home: PathBuf::from("/dev/home"),
            http_port: 13000,
            https_port: 13001,
            mesh_port: 13002,
            pid: 1,
            pgid: 1,
            started_at: utils::time::now(),
        })
        .unwrap();
        let pairs = dev_overlay_exec_env().unwrap();
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map["ORCA_HOME"], "/dev/home");
        assert_eq!(map["ORCA_HTTP_PORT"], "13000");
        assert_eq!(map["ORCA_MESH_PORT"], "13002");
    }

    #[test]
    fn bootstrap_dev_home_creates_isolated_vault_and_pki() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let dev_home = dir.path().join("dev-instance");
        let fresh = bootstrap_dev_home(&dev_home).expect("bootstrap Ok");
        assert!(fresh, "first bootstrap mints a fresh CA");
        assert!(dev_home.join("memory").is_dir());
        assert!(dev_home.join("logs/sessions").is_dir());
        assert!(utils::pki::ca_cert_path(&dev_home.join(contract::config::APP_PKI_DIR)).exists());
        // Idempotent: second run must not re-mint.
        assert!(!bootstrap_dev_home(&dev_home).expect("second bootstrap Ok"));
    }
}
