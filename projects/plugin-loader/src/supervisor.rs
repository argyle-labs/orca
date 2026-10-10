//! Out-of-process plugin supervisor — the counterpart to `plugin_toolkit::serve`
//! on the plugin side.
//!
//! The supervisor **spawns the plugin as a child process** (crash-isolated,
//! libc/ABI-independent) and talks to it over a Unix-domain socket using the
//! [`plugin_proto`] wire protocol:
//!
//! 1. bind a per-plugin UDS, hand its path to the child via `ORCA_PLUGIN_SOCKET`,
//!    spawn the executable, and `accept()` its connection;
//! 2. read the child's [`Hello`](Frame::Hello) (identity + tool/backend/schema
//!    surface), reply [`Welcome`](Frame::Welcome) advertising the daemon's
//!    [capabilities](crate::capability::CAPABILITIES) — or refuse on a
//!    wire-protocol major mismatch;
//! 3. [`invoke`](PluginProcess::invoke) a tool as a synchronous round-trip:
//!    write [`Invoke`](Frame::Invoke), then pump the socket — servicing the
//!    plugin's [`Cap`](Frame::Cap) requests through
//!    [`capability::handle_cap`](crate::capability::handle_cap) and forwarding
//!    [`Log`](Frame::Log) lines — until the matching [`Result`](Frame::Result).
//!
//! ## Serial contract
//!
//! Exactly one `Invoke` is in flight per plugin at a time (a `Mutex` around the
//! stream enforces it), so the mid-invoke exchange is a simple synchronous loop:
//! the only frames expected while a tool runs are that tool's `Cap` requests and
//! `Log` lines. This matches the plugin-side contract in `plugin_proto::session`
//! — no per-plugin read multiplexing.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use plugin_proto::{
    Frame, PROTOCOL_VERSION, ToolDef, VerifiedCaller, protocol_compatible, read_frame, write_frame,
};
use serde_json::Value;

use crate::capability::{self, CAPABILITIES};

/// Env var naming the UDS path a plugin connects back on. Mirrors
/// `plugin_toolkit::serve::SOCKET_ENV`.
pub const SOCKET_ENV: &str = "ORCA_PLUGIN_SOCKET";

/// Env var naming the orca binary that launched the plugin. Lets toolkit helpers
/// invoke a privileged admin round-trip (`sudo -n <orca> admin …`) without the
/// plugin guessing the daemon's path. Mirrors `plugin_toolkit::process::ORCA_BIN_ENV`.
pub const ORCA_BIN_ENV: &str = "ORCA_BIN";

/// Daemon env vars a plugin inherits; everything else (API keys, tokens, OAuth
/// secrets) is withheld. Process basics, locale, proxy/CA trust, log filters,
/// and the per-plugin config knobs plugins read (`ORCA_OP_BIN` for onepassword,
/// `DOCKER_*` for docker, `NUT_UPSD_HOST` for nut, …). `LC_*` is matched by
/// prefix in [`inherited_env`].
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "LANG",
    "TZ",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "ORCA_LOG",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "ORCA_HOME",
    "ORCA_DB_PATH",
    "ORCA_OP_BIN",
    "ORCA_RCLONE_BIN",
    "ORCA_PBS_IMAGE",
    "ORCA_PBS_VOLUME_ROOT",
    "ORCA_DOCKER_BACKUP_ROOTS",
    "ORCA_DOCKER_MANAGED_ROOTS",
    "ORCA_GITEA_RUNNER_RELEASE_SOURCE",
    "ORCA_GITEA_RUNNER_RELEASE_HOSTS",
    "ORCA_GITEA_RUNNER_PLAINTEXT_ORIGINS",
    "ORCA_UNRAID_ICON_HOSTS",
    "DOCKER_CONFIG",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
    "NUT_UPSD_HOST",
];

/// Filter `vars` down to the [`INHERITED_ENV`] allowlist plus `LC_*`.
fn inherited_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.into_iter()
        .filter(|(k, _)| {
            k.to_str()
                .is_some_and(|k| INHERITED_ENV.contains(&k) || k.starts_with("LC_"))
        })
        .collect()
}

/// The plugin surface learned from the handshake `Hello`.
#[derive(Debug, Clone)]
pub struct Handshake {
    pub software: String,
    pub semver: String,
    pub manifest: Vec<ToolDef>,
    pub backends: Vec<Value>,
    /// Declared SQL schema, verbatim (applied by the caller that owns the db).
    pub schema: Value,
}

/// Read the child's `Hello`, reply `Welcome` (advertising `capabilities`), and
/// return the declared surface. Refuses on a wire-protocol major mismatch.
pub fn handshake<S: Read + Write>(stream: &mut S, capabilities: &[&str]) -> Result<Handshake> {
    let hello = read_frame(stream)
        .context("reading plugin Hello")?
        .ok_or_else(|| anyhow!("plugin closed the socket before Hello"))?;
    let Frame::Hello {
        protocol,
        plugin,
        version,
        manifest,
        backends,
        schema,
    } = hello
    else {
        bail!("first plugin frame was not Hello");
    };
    if !protocol_compatible(&protocol, PROTOCOL_VERSION) {
        bail!(
            "plugin '{plugin}' speaks protocol {protocol}, daemon speaks {PROTOCOL_VERSION} (incompatible major)"
        );
    }
    write_frame(
        stream,
        &Frame::Welcome {
            protocol: PROTOCOL_VERSION.to_string(),
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
        },
    )
    .context("sending Welcome")?;
    Ok(Handshake {
        software: plugin,
        semver: version,
        manifest,
        backends,
        schema,
    })
}

/// Drive one tool invocation to completion over `stream`. Writes `Invoke{id}`,
/// then services `Cap`/`Log` frames until the `Result` with the matching `id`.
/// A `Cap` is executed via [`capability::handle_cap`] and answered with a
/// `CapResult`; a tool error becomes an `Err`.
pub fn invoke_on<S: Read + Write>(
    stream: &mut S,
    id: u64,
    tool: &str,
    args: Value,
    caller: Option<VerifiedCaller>,
    principal: &str,
) -> Result<Value> {
    write_frame(
        stream,
        &Frame::Invoke {
            id,
            tool: tool.to_string(),
            args,
            caller,
        },
    )
    .with_context(|| format!("sending Invoke for '{tool}'"))?;

    loop {
        let frame = read_frame(stream)
            .with_context(|| format!("awaiting Result for '{tool}'"))?
            .ok_or_else(|| anyhow!("plugin closed the socket during '{tool}'"))?;
        match frame {
            Frame::Result {
                id: rid,
                ok,
                value,
                error,
            } if rid == id => {
                return if ok {
                    Ok(value)
                } else {
                    Err(anyhow!(
                        "plugin tool '{tool}' failed: {}",
                        error.unwrap_or_else(|| "unknown error".into())
                    ))
                };
            }
            Frame::Cap {
                id: cap_id,
                cap,
                args,
            } if capability::is_streaming_cap(&cap) => {
                // Streaming capability: relay each chunk as a CapStreamChunk as it
                // is produced, then terminate with a CapStreamEnd. The chunk sink
                // writes directly to the socket, so a write failure aborts the
                // stream (propagated out of handle_cap_stream as an Err).
                let mut sink_err: Option<anyhow::Error> = None;
                let result = {
                    let sink_err = &mut sink_err;
                    capability::handle_cap_stream(&cap, args, &mut |seq, data| {
                        write_frame(
                            stream,
                            &Frame::CapStreamChunk {
                                id: cap_id,
                                seq,
                                data,
                            },
                        )
                        .map_err(|e| {
                            let e = anyhow!("streaming capability '{cap}': write chunk: {e}");
                            *sink_err = Some(anyhow!("{e}"));
                            e
                        })
                    })
                };
                // A socket-write failure in the sink means we cannot signal the
                // end either — surface it as the invoke error.
                if let Some(e) = sink_err {
                    return Err(e).with_context(|| format!("streaming capability '{cap}'"));
                }
                let end = match result {
                    Ok(()) => Frame::CapStreamEnd {
                        id: cap_id,
                        ok: true,
                        error: None,
                    },
                    Err(e) => Frame::CapStreamEnd {
                        id: cap_id,
                        ok: false,
                        error: Some(e.to_string()),
                    },
                };
                write_frame(stream, &end)
                    .with_context(|| format!("ending streaming capability '{cap}'"))?;
            }
            Frame::Cap {
                id: cap_id,
                cap,
                args,
            } => {
                let reply = match capability::handle_cap(&cap, args, principal) {
                    Ok(value) => Frame::CapResult {
                        id: cap_id,
                        ok: true,
                        value,
                        error: None,
                    },
                    Err(e) => Frame::CapResult {
                        id: cap_id,
                        ok: false,
                        value: Value::Null,
                        error: Some(e.to_string()),
                    },
                };
                write_frame(stream, &reply)
                    .with_context(|| format!("answering capability '{cap}'"))?;
            }
            Frame::Log { level, msg, .. } => match level.as_str() {
                "error" => tracing::error!(target: "plugin", "{msg}"),
                "warn" => tracing::warn!(target: "plugin", "{msg}"),
                "debug" | "trace" => tracing::debug!(target: "plugin", "{msg}"),
                _ => tracing::info!(target: "plugin", "{msg}"),
            },
            // Serial contract: a stray Result for another id, or any other frame,
            // shouldn't arrive mid-invoke. Ignore defensively rather than wedge.
            _ => {}
        }
    }
}

/// Resolve the authoritative session principal from the install id (if known)
/// and the plugin's self-declared handshake id. `Some(id)` validates equality
/// (a mismatch is a hard error — id mismatch or tampering); `None` is
/// trust-on-first-use and accepts the declared id. Either way an empty or
/// reserved ([`crate::is_reserved_plugin_id`]) principal is refused, since a
/// principal owns every secret named `<id>` / `<id>.…`.
fn resolve_principal(expected_id: Option<&str>, declared: &str) -> Result<String> {
    let principal = match expected_id {
        Some(id) if declared != id => bail!(
            "plugin declared id '{declared}' but orca is loading it as '{id}' \
             — refusing (id mismatch or tampering)"
        ),
        Some(id) => id,
        None => declared,
    };
    if !crate::is_valid_plugin_id(principal) {
        bail!("refusing to load plugin as '{principal}': id must match ^[a-z][a-z0-9_-]{{0,63}}$");
    }
    if crate::is_reserved_plugin_id(principal) {
        bail!("refusing to load plugin as '{principal}': id is reserved for orca core");
    }
    Ok(principal.to_string())
}

/// A spawned plugin subprocess and its session socket.
///
/// Drops send `Shutdown` (best-effort) and reap the child; a plugin crash is
/// isolated to this process — the daemon logs it and can respawn, never dies
/// with the plugin.
pub struct PluginProcess {
    pub software: String,
    pub semver: String,
    pub manifest: Vec<ToolDef>,
    pub backends: Vec<Value>,
    pub schema: Value,
    child: Child,
    stream: Mutex<std::os::unix::net::UnixStream>,
    next_id: AtomicU64,
}

impl PluginProcess {
    /// Spawn `exe`, connect its session socket, complete the handshake, and
    /// return a live process advertising the daemon's capabilities.
    ///
    /// `expected_id` is the authoritative plugin id orca is loading this binary
    /// as (the install-dir filename). When `Some`, the self-declared `Hello` id
    /// is validated against it (mismatch is refused) and it becomes the session
    /// **principal** for capability namespace scoping — so a plugin cannot widen
    /// its reach by lying in `Hello`. When `None` (trust-on-first-use, e.g. a
    /// sideload from an arbitrary path before the id is recorded), the declared
    /// `Hello` id is accepted and used as the principal.
    pub fn spawn(exe: &Path, expected_id: Option<&str>) -> Result<Self> {
        // Bind BEFORE spawn so the child's connect() can't race an unbound path.
        let (rendezvous, listener) = bind_rendezvous(private_socket_dir()?, "s.sock")?;
        let sock_path = rendezvous.sock.clone();

        // Trust model: the env is scrubbed so daemon secrets don't leak by
        // default, but the plugin runs as the daemon's uid — it can still read
        // anything the daemon can (e.g. `~/.orca/.db_key`, mode 0600). That is
        // not an isolation boundary; installed plugins are trusted code.
        let mut cmd = Command::new(exe);
        cmd.env_clear();
        cmd.envs(inherited_env(std::env::vars_os()));
        cmd.env(SOCKET_ENV, &sock_path);
        // Tell the plugin which orca binary launched it, so toolkit helpers that
        // need a privileged round-trip (e.g. `sudo -n <orca> admin lxc-exec`)
        // can reach it. The daemon's own path is authoritative — a plugin never
        // guesses it. Best-effort: absent ORCA_BIN, the toolkit falls back to a
        // direct (non-orca) path where one exists.
        if let Ok(orca_bin) = std::env::current_exe() {
            cmd.env(ORCA_BIN_ENV, orca_bin);
        }
        // Opt-in instrumentation: when the daemon has enabled profiling for this
        // plugin, inject MALLOC_CONF + ORCA_PLUGIN_INSTRUMENT so the respawned
        // process activates jemalloc heap profiling and its auto-diagnostics
        // provider. Empty (no-op) for every plugin that is not enabled.
        if let Some(id) = expected_id {
            for (k, v) in contract::plugin_instrument::env_for(id) {
                cmd.env(k, v);
            }
        }
        let spawned = cmd
            .spawn()
            .with_context(|| format!("spawning plugin executable {exe:?}"));
        let accepted = spawned.and_then(|mut child| {
            // The child connects back; accept its single session connection.
            let stream = listener
                .accept()
                .map(|(stream, _addr)| stream)
                .with_context(|| format!("accepting connection from plugin {exe:?}"))
                .and_then(|stream| {
                    verify_peer(&stream, child.id())
                        .with_context(|| format!("plugin {exe:?} session peer"))?;
                    Ok(stream)
                });
            match stream {
                Ok(stream) => Ok((child, stream)),
                Err(e) => {
                    _ = child.kill();
                    _ = child.wait();
                    Err(e)
                }
            }
        });
        // The dir is only needed for the connect rendezvous; remove it now so a
        // crash can't leave a stale socket blocking a respawn.
        drop(rendezvous);
        let (mut child, mut stream) = accepted?;

        // Authoritative principal: the install id when known (validate the
        // self-declared handshake id against it), else trust-on-first-use.
        let bound = handshake(&mut stream, CAPABILITIES).and_then(|hs| {
            let principal = resolve_principal(expected_id, &hs.software)
                .with_context(|| format!("plugin binary {exe:?} handshake identity"))?;
            Ok((hs, principal))
        });
        let (hs, principal) = match bound {
            Ok(b) => b,
            Err(e) => {
                _ = child.kill();
                _ = child.wait();
                return Err(e);
            }
        };
        tracing::info!(
            plugin = %principal,
            version = %hs.semver,
            tools = hs.manifest.len(),
            backends = hs.backends.len(),
            "spawned out-of-process plugin"
        );

        Ok(Self {
            // Authoritative identity is the install id (validated equal above),
            // or the declared id on trust-on-first-use.
            software: principal,
            semver: hs.semver,
            manifest: hs.manifest,
            backends: hs.backends,
            schema: hs.schema,
            child,
            stream: Mutex::new(stream),
            next_id: AtomicU64::new(1),
        })
    }

    /// Invoke a tool. Serialized by the stream `Mutex` — one `Invoke` in flight
    /// per plugin, per the serial contract.
    pub fn invoke(&self, tool: &str, args: Value, caller: Option<VerifiedCaller>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| anyhow!("plugin '{}' session mutex poisoned", self.software))?;
        invoke_on(&mut *stream, id, tool, args, caller, &self.software)
    }

    /// Best-effort graceful shutdown: send `Shutdown`, then terminate + reap.
    pub fn shutdown(&mut self) {
        if let Ok(mut stream) = self.stream.lock() {
            _ = write_frame(&mut *stream, &Frame::Shutdown);
        }
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Create a fresh 0700 directory under the temp dir to hold one plugin's
/// rendezvous socket, so no other uid can connect to (or pre-plant) it.
///
/// `mkdir` with the mode is atomic (umask only narrows it). The name is
/// predictable, so an existing entry is adopted only if it is a real directory
/// (not a symlink) owned by us with mode 0700 — e.g. left by a crashed spawn.
///
/// Names stay short: macOS `$TMPDIR` is long and `sun_path` caps at 104 bytes.
fn private_socket_dir() -> Result<std::path::PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "orca-plugin-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating plugin socket dir {dir:?}")),
    }
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("inspecting plugin socket dir {dir:?}"))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if !meta.file_type().is_dir()
        || meta.uid() != euid
        || meta.permissions().mode() & 0o777 != 0o700
    {
        bail!("plugin socket dir {dir:?} is not a 0700 directory owned by uid {euid} — refusing");
    }
    Ok(())
}

/// A plugin's private socket dir and socket path, removed on drop so every
/// exit path from [`PluginProcess::spawn`] cleans up.
struct Rendezvous {
    dir: std::path::PathBuf,
    sock: std::path::PathBuf,
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        _ = std::fs::remove_file(&self.sock);
        _ = std::fs::remove_dir(&self.dir);
    }
}

/// Bind `name` inside `dir`, which the returned guard (or a failed bind)
/// removes.
fn bind_rendezvous(
    dir: std::path::PathBuf,
    name: &str,
) -> Result<(Rendezvous, std::os::unix::net::UnixListener)> {
    let rendezvous = Rendezvous {
        sock: dir.join(name),
        dir,
    };
    _ = std::fs::remove_file(&rendezvous.sock);
    let listener = std::os::unix::net::UnixListener::bind(&rendezvous.sock)
        .with_context(|| format!("binding plugin socket {:?}", rendezvous.sock))?;
    Ok((rendezvous, listener))
}

/// Refuse a session connection that did not come from the spawned child.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn verify_peer(stream: &std::os::unix::net::UnixStream, child_pid: u32) -> Result<()> {
    use std::os::fd::AsRawFd;

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred`/`len` are valid for the ucred-sized write SO_PEERCRED does.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("SO_PEERCRED");
    }
    check_peer_pid(cred.pid as u32, child_pid)
}

/// Refuse a session connection that did not come from the spawned child.
#[cfg(target_vendor = "apple")]
fn verify_peer(stream: &std::os::unix::net::UnixStream, child_pid: u32) -> Result<()> {
    use std::os::fd::AsRawFd;

    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid`/`len` are valid for the pid_t-sized write LOCAL_PEERPID does.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("LOCAL_PEERPID");
    }
    check_peer_pid(pid as u32, child_pid)
}

/// Platforms without a peer-pid query rely on the 0700 socket dir alone.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn verify_peer(_stream: &std::os::unix::net::UnixStream, _child_pid: u32) -> Result<()> {
    Ok(())
}

#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple")),
    allow(dead_code)
)]
fn check_peer_pid(peer_pid: u32, child_pid: u32) -> Result<()> {
    if peer_pid != child_pid {
        bail!("connection came from pid {peer_pid}, not the spawned plugin pid {child_pid}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_proto::session::serve;
    use serde_json::json;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn fake_hello() -> Frame {
        Frame::Hello {
            protocol: PROTOCOL_VERSION.into(),
            plugin: "fake".into(),
            version: "0.1.0".into(),
            manifest: vec![],
            backends: vec![],
            schema: Value::Null,
        }
    }

    #[test]
    fn private_socket_dir_is_owner_only() {
        // Skip inside the spawned plugin child: it is killed mid-run and
        // would leak the dir.
        if std::env::var_os(SOCKET_ENV).is_some() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = private_socket_dir().unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn bind_failure_removes_socket_dir() {
        // Skip inside the spawned plugin child: it is killed mid-run and
        // would leak the dir.
        if std::env::var_os(SOCKET_ENV).is_some() {
            return;
        }
        let dir = private_socket_dir().unwrap();
        // Longer than any platform's sun_path, so bind() must fail.
        let res = bind_rendezvous(dir.clone(), &"s".repeat(200));
        assert!(res.is_err());
        assert!(!dir.exists(), "socket dir left behind: {dir:?}");
    }

    #[test]
    fn rendezvous_drop_removes_socket_and_dir() {
        // Skip inside the spawned plugin child: it is killed mid-run and
        // would leak the dir.
        if std::env::var_os(SOCKET_ENV).is_some() {
            return;
        }
        let dir = private_socket_dir().unwrap();
        let (rendezvous, _listener) = bind_rendezvous(dir.clone(), "s.sock").unwrap();
        assert!(rendezvous.sock.exists());
        drop(rendezvous);
        assert!(!dir.exists(), "socket dir left behind: {dir:?}");
    }

    #[test]
    fn ensure_private_dir_refuses_loose_or_symlinked_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let loose = tmp.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_private_dir(&loose).is_err());

        let target = tmp.path().join("target");
        ensure_private_dir(&target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());

        // A leftover dir that is already ours and 0700 is adopted.
        ensure_private_dir(&target).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn verify_peer_matches_connecting_pid_only() {
        let (a, _b) = UnixStream::pair().unwrap();
        verify_peer(&a, std::process::id()).unwrap();
        assert!(verify_peer(&a, std::process::id() + 1).is_err());
    }

    #[test]
    fn inherited_env_withholds_secrets() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("LC_ALL", "C.UTF-8"),
            ("https_proxy", "http://p"),
            ("ORCA_OP_BIN", "/opt/op"),
            ("ANTHROPIC_API_KEY", "sk"),
            ("GITHUB_TOKEN", "gh"),
            ("ORCA_TOKEN", "t"),
            ("ORCA_MCP_TOKEN", "t"),
            ("ORCA_OAUTH_CLIENT_SECRET", "s"),
        ]
        .map(|(k, v)| (k.into(), v.into()));
        let mut kept: Vec<String> = inherited_env(vars)
            .into_iter()
            .map(|(k, _)| k.into_string().unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, ["LC_ALL", "ORCA_OP_BIN", "PATH", "https_proxy"]);
    }

    #[test]
    fn handshake_reads_surface_and_sends_welcome() {
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        // Plugin side: just send Hello, then read the Welcome back.
        let plugin = thread::spawn(move || {
            let mut s = plugin_end;
            write_frame(&mut s, &fake_hello()).unwrap();
            read_frame(&mut s).unwrap().unwrap()
        });

        let hs = handshake(&mut orca_end, CAPABILITIES).unwrap();
        assert_eq!(hs.software, "fake");
        assert_eq!(hs.semver, "0.1.0");

        match plugin.join().unwrap() {
            Frame::Welcome {
                protocol,
                capabilities,
            } => {
                assert_eq!(protocol, PROTOCOL_VERSION);
                assert!(capabilities.contains(&"db.op".to_string()));
            }
            f => panic!("expected Welcome, got {f:?}"),
        }
    }

    #[test]
    fn handshake_rejects_incompatible_protocol() {
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let mut s = plugin_end;
            write_frame(
                &mut s,
                &Frame::Hello {
                    protocol: "2.0".into(),
                    plugin: "fake".into(),
                    version: "0.1.0".into(),
                    manifest: vec![],
                    backends: vec![],
                    schema: Value::Null,
                },
            )
            .unwrap();
        });
        let err = handshake(&mut orca_end, CAPABILITIES)
            .unwrap_err()
            .to_string();
        assert!(err.contains("incompatible major"), "got: {err}");
    }

    #[test]
    fn invoke_echoes_tool_result() {
        // A fake plugin running the real plugin-side serve loop; the daemon side
        // drives it through handshake + invoke_on. `db.op`/`secret.op` caps are
        // not exercised here (they'd need a live db) — routing is covered in the
        // `capability` module's tests.
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || {
            serve(plugin_end, fake_hello(), |tool, args, _caps| {
                if tool == "echo" {
                    Ok(args)
                } else {
                    Err(format!("no such tool: {tool}"))
                }
            })
        });

        let hs = handshake(&mut orca_end, CAPABILITIES).unwrap();
        assert_eq!(hs.software, "fake");

        let out = invoke_on(&mut orca_end, 1, "echo", json!({"n": 7}), None, "fake").unwrap();
        assert_eq!(out, json!({"n": 7}));

        let err = invoke_on(&mut orca_end, 2, "missing", Value::Null, None, "fake")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such tool"), "got: {err}");

        write_frame(&mut orca_end, &Frame::Shutdown).unwrap();
        plugin.join().unwrap().unwrap();
    }

    #[test]
    fn invoke_services_log_and_cap_before_result() {
        // Drive invoke_on against a hand-rolled "plugin" that, mid-invoke, emits
        // a Log line and an (unknown) Cap request before the Result. This
        // exercises the Log branch and the non-streaming Cap branch — the Cap is
        // answered with a failing CapResult (unknown capability), which the fake
        // plugin verifies, and the invoke still completes with the tool value.
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || {
            let mut s = plugin_end;
            // Read the Invoke the supervisor writes.
            let inv = read_frame(&mut s).unwrap().unwrap();
            let inv_id = match inv {
                Frame::Invoke { id, tool, .. } => {
                    assert_eq!(tool, "work");
                    id
                }
                f => panic!("expected Invoke, got {f:?}"),
            };
            // Log lines at each level exercise all match arms.
            for level in ["error", "warn", "debug", "trace", "info", "other"] {
                write_frame(
                    &mut s,
                    &Frame::Log {
                        level: level.into(),
                        msg: format!("hello from {level}"),
                        fields: Value::Null,
                    },
                )
                .unwrap();
            }
            // A capability request; handle_cap rejects the unknown cap, so the
            // supervisor answers with a failing CapResult.
            write_frame(
                &mut s,
                &Frame::Cap {
                    id: 99,
                    cap: "does.not.exist".into(),
                    args: Value::Null,
                },
            )
            .unwrap();
            let reply = read_frame(&mut s).unwrap().unwrap();
            match reply {
                Frame::CapResult { id, ok, error, .. } => {
                    assert_eq!(id, 99);
                    assert!(!ok);
                    assert!(error.unwrap().contains("unknown capability"));
                }
                f => panic!("expected CapResult, got {f:?}"),
            }
            // Finally the tool Result.
            write_frame(
                &mut s,
                &Frame::Result {
                    id: inv_id,
                    ok: true,
                    value: json!({"done": true}),
                    error: None,
                },
            )
            .unwrap();
        });

        let out = invoke_on(&mut orca_end, 42, "work", Value::Null, None, "p").unwrap();
        assert_eq!(out, json!({"done": true}));
        plugin.join().unwrap();
    }

    #[test]
    fn invoke_carries_the_verified_caller_on_the_wire() {
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || {
            let mut s = plugin_end;
            let (id, caller) = match read_frame(&mut s).unwrap().unwrap() {
                Frame::Invoke { id, caller, .. } => (id, caller),
                f => panic!("expected Invoke, got {f:?}"),
            };
            write_frame(
                &mut s,
                &Frame::Result {
                    id,
                    ok: true,
                    value: serde_json::to_value(&caller).unwrap(),
                    error: None,
                },
            )
            .unwrap();
        });
        let alice = VerifiedCaller {
            user_id: "u1".into(),
            username: "alice".into(),
            role: "admin".into(),
            can_mutate: false,
        };
        let out = invoke_on(&mut orca_end, 1, "who", Value::Null, Some(alice), "p").unwrap();
        assert_eq!(out["username"], "alice");
        plugin.join().unwrap();
    }

    #[test]
    fn invoke_services_streaming_cap_then_result() {
        // A streaming Cap ("http.stream") with a bad payload: handle_cap_stream
        // fails to parse, so the supervisor writes CapStreamEnd{ok:false} and
        // then continues until the Result.
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || {
            let mut s = plugin_end;
            let inv_id = match read_frame(&mut s).unwrap().unwrap() {
                Frame::Invoke { id, .. } => id,
                f => panic!("expected Invoke, got {f:?}"),
            };
            write_frame(
                &mut s,
                &Frame::Cap {
                    id: 7,
                    cap: "http.stream".into(),
                    // Not a valid HttpStreamRequest -> parse error.
                    args: json!("not an object"),
                },
            )
            .unwrap();
            match read_frame(&mut s).unwrap().unwrap() {
                Frame::CapStreamEnd { id, ok, error } => {
                    assert_eq!(id, 7);
                    assert!(!ok);
                    assert!(error.unwrap().contains("http.stream"));
                }
                f => panic!("expected CapStreamEnd, got {f:?}"),
            }
            write_frame(
                &mut s,
                &Frame::Result {
                    id: inv_id,
                    ok: false,
                    value: Value::Null,
                    error: Some("boom".into()),
                },
            )
            .unwrap();
        });

        let err = invoke_on(&mut orca_end, 1, "streamy", Value::Null, None, "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "got: {err}");
        plugin.join().unwrap();
    }

    #[test]
    fn invoke_errors_when_plugin_closes_before_result() {
        // Plugin reads the Invoke, then drops the socket without a Result.
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || {
            let mut s = plugin_end;
            let _ = read_frame(&mut s).unwrap();
            // drop `s` -> EOF on the orca side.
        });
        let err = invoke_on(&mut orca_end, 5, "work", Value::Null, None, "p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed the socket"), "got: {err}");
        plugin.join().unwrap();
    }

    #[test]
    fn handshake_errors_when_closed_before_hello() {
        // Empty stream: no Hello ever arrives.
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        drop(plugin_end);
        let err = handshake(&mut orca_end, CAPABILITIES)
            .unwrap_err()
            .to_string();
        assert!(err.contains("before Hello"), "got: {err}");
    }

    #[test]
    fn handshake_rejects_non_hello_first_frame() {
        let (plugin_end, mut orca_end) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let mut s = plugin_end;
            write_frame(&mut s, &Frame::Shutdown).unwrap();
        });
        let err = handshake(&mut orca_end, CAPABILITIES)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not Hello"), "got: {err}");
    }

    #[test]
    fn private_socket_dir_names_by_pid() {
        // Skip inside the spawned plugin child: it is killed mid-run and
        // would leak the dir.
        if std::env::var_os(SOCKET_ENV).is_some() {
            return;
        }
        let p = private_socket_dir().unwrap();
        std::fs::remove_dir(&p).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap();
        let prefix = format!("orca-plugin-{}-", std::process::id());
        assert!(name.starts_with(&prefix), "got: {name}");
    }

    #[test]
    fn resolve_principal_validates_and_tofu() {
        // Trust-on-first-use: declared id accepted as principal.
        assert_eq!(resolve_principal(None, "jellyfin").unwrap(), "jellyfin");
        // Known id matching the declaration: accepted.
        assert_eq!(
            resolve_principal(Some("jellyfin"), "jellyfin").unwrap(),
            "jellyfin"
        );
        // Known id disagreeing with the declaration: refused.
        let err = resolve_principal(Some("jellyfin"), "evil")
            .unwrap_err()
            .to_string();
        assert!(err.contains("id mismatch or tampering"), "got: {err}");
        assert!(resolve_principal(None, "").is_err());
    }

    #[test]
    fn resolve_principal_refuses_reserved_ids() {
        for id in ["orca", "github_token", "secrets", "mesh"] {
            let err = resolve_principal(Some(id), id).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{id}: {err}");
            let err = resolve_principal(None, id).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{id}: {err}");
        }
    }

    #[test]
    fn resolve_principal_refuses_malformed_ids() {
        for id in [
            "model.0190f2c4-7b1e-7c3a-9a8e-1d2c3b4a5f6e",
            "../x",
            "Jellyfin",
        ] {
            let err = resolve_principal(Some(id), id).unwrap_err().to_string();
            assert!(err.contains("must match"), "{id}: {err}");
            let err = resolve_principal(None, id).unwrap_err().to_string();
            assert!(err.contains("must match"), "{id}: {err}");
        }
        assert_eq!(
            resolve_principal(Some("calibre-web"), "calibre-web").unwrap(),
            "calibre-web"
        );
        assert_eq!(
            resolve_principal(None, "calibre-web").unwrap(),
            "calibre-web"
        );
    }
}
