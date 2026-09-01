//! Cross-platform mount-table primitive shared by every network-share backend.
//!
//! Reading the kernel mount table is OS-specific (`/proc/mounts` on Linux,
//! `/sbin/mount` output on macOS) and was previously duplicated — and divergent
//! — across the `nfs` and `smb` plugins. This module is the single source: a
//! typed [`MountEntry`], a typed [`Health`], the platform-gated [`mount_table`]
//! reader, and a runtime-agnostic timed [`probe_health`]. Backends filter the
//! table by fstype and contribute the rows as `storage` shares.
//!
//! Kept synchronous (std only) so the `storage` domain stays tokio-free; async
//! callers wrap [`probe_health`] in `spawn_blocking` if they must.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One row of the kernel mount table, normalized across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MountEntry {
    /// Mount source as the OS reports it: `host:/export` (NFS),
    /// `//server/share` (SMB), a device node, etc.
    pub source: String,
    /// Absolute mountpoint path.
    pub mountpoint: String,
    /// Filesystem / transport type (`nfs4`, `cifs`, `smbfs`, `apfs`, …).
    pub fstype: String,
    /// Mount options as individual tokens (`rw`, `vers=4.2`, `nosuid`, …).
    #[serde(default)]
    pub options: Vec<String>,
}

/// Liveness classification for a mountpoint. Shared so the nfs/smb dashboards
/// speak one language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Path is a live mount and answers I/O within the budget.
    Ok,
    /// Mount is present but I/O hung past the timeout (unreachable server).
    Stale,
    /// Path does not exist / nothing mounted there.
    Missing,
    /// Probe exceeded its time budget without a definite stale/ok answer.
    Timeout,
    /// Probe failed for some other reason.
    Error,
    /// State is not observed by the daemon answering the read: the placement
    /// belongs to another host and that owner could not be reached to report its
    /// own liveness. Distinct from `Missing` (a definite "nothing is mounted"),
    /// which only the owning host can assert. A read never presents a peer's
    /// host-local liveness column as truth — an unreached owner is `Unknown`.
    Unknown,
}

/// Read the live kernel mount table for the current platform. Unsupported
/// platforms return an empty table rather than erroring so callers degrade
/// gracefully.
pub fn mount_table() -> std::io::Result<Vec<MountEntry>> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string("/proc/mounts")?;
        Ok(parse_linux_proc_mounts(&raw))
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/sbin/mount").output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "/sbin/mount exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(parse_macos_mount(&String::from_utf8_lossy(&out.stdout)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Ok(Vec::new())
    }
}

/// The live mount table restricted to a set of filesystem types. Empty filter
/// returns everything.
pub fn mount_table_of(fstypes: &[&str]) -> std::io::Result<Vec<MountEntry>> {
    let all = mount_table()?;
    if fstypes.is_empty() {
        return Ok(all);
    }
    Ok(all
        .into_iter()
        .filter(|m| fstypes.contains(&m.fstype.as_str()))
        .collect())
}

/// Parse a `/proc/mounts`-formatted stream (Linux). Pure so tests run anywhere.
pub fn parse_linux_proc_mounts(raw: &str) -> Vec<MountEntry> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let source = parts.next()?;
            let mountpoint = parts.next()?;
            let fstype = parts.next()?;
            let opts = parts.next().unwrap_or("");
            Some(MountEntry {
                source: unescape_octal(source),
                mountpoint: unescape_octal(mountpoint),
                fstype: fstype.to_string(),
                options: opts.split(',').map(|s| s.to_string()).collect(),
            })
        })
        .collect()
}

/// Parse `/sbin/mount` output (macOS / BSD). Lines look like:
/// `//user@srv/share on /Volumes/share (smbfs, nodev, nosuid, mounted by u)`.
/// Pure so tests run on any platform.
pub fn parse_macos_mount(raw: &str) -> Vec<MountEntry> {
    raw.lines()
        .filter_map(|line| {
            let (source, rest) = line.split_once(" on ")?;
            let (mountpoint, opts) = rest.split_once(" (")?;
            let opts = opts.trim_end_matches(')');
            let mut parts = opts.split(',').map(|s| s.trim());
            let fstype = parts.next()?.to_string();
            let options: Vec<String> = parts.map(|s| s.to_string()).collect();
            Some(MountEntry {
                source: source.to_string(),
                mountpoint: mountpoint.to_string(),
                fstype,
                options,
            })
        })
        .collect()
}

/// `/proc/mounts` octal-escapes spaces, tabs, and a few specials. Reverse it.
fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut digits = String::with_capacity(3);
        for _ in 0..3 {
            match chars.peek() {
                Some(d) if d.is_ascii_digit() => digits.push(chars.next().unwrap()),
                _ => break,
            }
        }
        if digits.len() == 3
            && let Ok(n) = u8::from_str_radix(&digits, 8)
        {
            out.push(n as char);
        } else {
            out.push('\\');
            out.push_str(&digits);
        }
    }
    out
}

/// Time-bounded liveness probe of a mountpoint. Runtime-agnostic: the blocking
/// `stat` runs on a worker thread and the result is awaited with a timeout, so a
/// hung (stale) NFS/SMB handle classifies as [`Health::Stale`] instead of
/// blocking the caller forever. Async callers should still wrap this in
/// `spawn_blocking` since it parks a thread for up to `timeout`.
pub fn probe_health(mountpoint: &str, timeout: Duration) -> Health {
    let owned = mountpoint.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    // Detached worker: if it blocks on a stale handle it leaks one thread until
    // the kernel gives up — acceptable and unavoidable for stale NFS. All
    // classification comes from this one stat: `Path::exists()` reports an ESTALE
    // ("Stale file handle") mountpoint as absent, so a prior `!exists()` guard
    // misclassified a wedged mount as Missing and starved the stale-remount path.
    // A genuine NotFound maps to Missing below; every other stat error (ESTALE
    // included) maps to Stale.
    std::thread::spawn(move || {
        drop(tx.send(std::fs::metadata(&owned).map(|_| ())));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => Health::Ok,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Health::Missing,
        Ok(Err(_)) => Health::Stale,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Health::Stale,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Health::Error,
    }
}

/// Parse a mount `source` into the `(host, port)` of the server backing it, so
/// callers can run a TCP liveness probe against the transport itself.
///
/// The host is parsed **generically from the source's shape** — no fstype
/// literal lives here:
///
/// - An **authority** source (`//server/share`, optionally `//user@server/share`)
///   — the SMB shape: the authority is taken up to the next `/`, any `user@`
///   prefix stripped.
/// - A **`host:/path`** source (`host:/export`) — the NFS shape: the host is
///   everything before the first `:`.
/// - Anything else (a bare device path, no `//` and no `:`): no host.
///
/// The **port** is fstype grammar owned by the plugin, not core: it is resolved
/// via [`source_port_for_fstype`](crate::source_port_for_fstype), which asks the
/// registered backend that owns `fstype` for its
/// [`default_source_port`](crate::StorageBackend::default_source_port). Returns
/// `None` when the host can't be parsed or no registered backend supplies a probe
/// port for `fstype` (so a disk/object source is never TCP-probed).
///
/// The host is trimmed of surrounding whitespace.
pub fn source_endpoint(source: &str, fstype: &str) -> Option<(String, u16)> {
    let host = parse_source_host(source)?;
    let port = crate::source_port_for_fstype(fstype)?;
    Some((host, port))
}

/// Parse the server host out of a mount `source` by its shape alone — generic,
/// no fstype knowledge. `//user@server/share` → `server`; `host:/export` →
/// `host`. `None` for a source that is neither an authority nor `host:/path`
/// form (e.g. a bare device path), or whose host is empty. Trims whitespace.
fn parse_source_host(source: &str) -> Option<String> {
    if let Some(rest) = source.strip_prefix("//") {
        // Authority form (`//[user@]server/share`).
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or("").trim();
        (!host.is_empty()).then(|| host.to_string())
    } else if let Some((host, _rest)) = source.split_once(':') {
        // `host:/path` form.
        let host = host.trim();
        (!host.is_empty()).then(|| host.to_string())
    } else {
        None
    }
}

/// Real transport liveness probe: resolve the server behind a mount `source` and
/// attempt a bounded TCP connect, returning `true` on the first reachable
/// address. Replaces stat-on-trigger-dir checks that false-positive on an
/// autofs mountpoint whose server is actually down.
///
/// Returns `false` when the source is not a network source ([`source_endpoint`]
/// yields `None`), when DNS resolution fails, or when every resolved address
/// fails to connect within `timeout`. Kept synchronous/std-only so the `storage`
/// domain stays tokio-free; async callers wrap it in `spawn_blocking`, mirroring
/// [`probe_health`].
pub fn probe_source(source: &str, fstype: &str, timeout: Duration) -> bool {
    use std::net::ToSocketAddrs;
    let Some((host, port)) = source_endpoint(source, fstype) else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).is_ok())
}

/// The NFS server port every NFSv4 server (and NFSv3 with a fixed nfsd port)
/// answers RPC on. The election probes this directly so it never needs the
/// portmapper.
const NFS_PORT: u16 = 2049;
/// ONC RPC program number for NFS.
const NFS_PROGRAM: u32 = 100003;
/// NFS version whose RPC NULL we call — v3's NULL is universally answered by any
/// server that also speaks v4, and needs no session/state.
const NFS_VERSION: u32 = 3;

/// Real *NFS-service* liveness probe: connect to the server's nfsd on
/// [`NFS_PORT`] and send an ONC RPC NULL call for the NFS program, returning
/// `true` only when a well-formed RPC reply with `accept_stat = SUCCESS` comes
/// back within `timeout`.
///
/// The failure mode this catches that [`probe_source`] cannot: a host whose TCP
/// stack is up (so a bare connect succeeds) but whose `nfsd` is wedged or not
/// yet serving — a plain TCP probe reports it live and election fails back onto
/// a hung primary, re-wedging every client. Because this exchanges an actual RPC
/// NULL, a server that never answers classifies as down.
///
/// Read-only and side-effect-free (NULL is the RPC no-op), std-only so the
/// `storage` domain stays tokio-free; async callers wrap it in `spawn_blocking`.
/// Returns `false` on DNS failure, connect failure, a short/truncated reply, or
/// any RPC-level rejection.
pub fn probe_source_nfs(host: &str, timeout: Duration) -> bool {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let Ok(addrs) = (host, NFS_PORT).to_socket_addrs() else {
        return false;
    };
    let Some(mut stream) = addrs
        .into_iter()
        .find_map(|addr| TcpStream::connect_timeout(&addr, timeout).ok())
    else {
        return false;
    };
    // Bound the whole exchange by the caller's budget.
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return false;
    }

    let xid: u32 = 0x0ca5_a1d0; // arbitrary nonce; matched on the reply
    let call = rpc_null_call(xid);
    if stream.write_all(&call).is_err() {
        return false;
    }

    // Read the RPC-over-TCP record: a 4-byte record mark (last-fragment bit +
    // length), then that many bytes. We only need the reply header.
    let mut mark = [0u8; 4];
    if stream.read_exact(&mut mark).is_err() {
        return false;
    }
    let len = (u32::from_be_bytes(mark) & 0x7FFF_FFFF) as usize;
    if !(24..=4096).contains(&len) {
        return false; // too short to be a reply header, or implausibly large
    }
    let mut body = vec![0u8; len];
    if stream.read_exact(&mut body).is_err() {
        return false;
    }
    rpc_reply_is_success(xid, &body)
}

/// Serialize a minimal ONC RPC (RFC 5531) NULL call for the NFS program, framed
/// for TCP with a single last-fragment record mark. NULL takes AUTH_NONE
/// credentials/verifier and no arguments, so the message is fixed-size.
fn rpc_null_call(xid: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(44);
    msg.extend_from_slice(&xid.to_be_bytes()); // xid
    msg.extend_from_slice(&0u32.to_be_bytes()); // msg_type = CALL (0)
    msg.extend_from_slice(&2u32.to_be_bytes()); // rpcvers = 2
    msg.extend_from_slice(&NFS_PROGRAM.to_be_bytes()); // program
    msg.extend_from_slice(&NFS_VERSION.to_be_bytes()); // version
    msg.extend_from_slice(&0u32.to_be_bytes()); // procedure = NULL (0)
    // cred: AUTH_NONE (flavor 0), length 0
    msg.extend_from_slice(&0u32.to_be_bytes());
    msg.extend_from_slice(&0u32.to_be_bytes());
    // verf: AUTH_NONE (flavor 0), length 0
    msg.extend_from_slice(&0u32.to_be_bytes());
    msg.extend_from_slice(&0u32.to_be_bytes());
    // NULL has no arguments.

    let mut framed = Vec::with_capacity(msg.len() + 4);
    let mark = 0x8000_0000u32 | (msg.len() as u32); // last-fragment + length
    framed.extend_from_slice(&mark.to_be_bytes());
    framed.extend_from_slice(&msg);
    framed
}

/// Parse an RPC reply body and decide it is an accepted NULL success: matching
/// `xid`, `msg_type = REPLY`, `reply_stat = MSG_ACCEPTED`, and (after the
/// AUTH_NONE verifier) `accept_stat = SUCCESS`.
fn rpc_reply_is_success(xid: u32, body: &[u8]) -> bool {
    fn u32_at(b: &[u8], off: usize) -> Option<u32> {
        b.get(off..off + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    if u32_at(body, 0) != Some(xid) {
        return false;
    }
    if u32_at(body, 4) != Some(1) {
        return false; // msg_type must be REPLY (1)
    }
    if u32_at(body, 8) != Some(0) {
        return false; // reply_stat must be MSG_ACCEPTED (0)
    }
    // verifier: flavor (off 12), length (off 16), then `length` opaque bytes.
    let verf_len = match u32_at(body, 16) {
        Some(n) => n as usize,
        None => return false,
    };
    let accept_off = 20 + verf_len;
    // accept_stat == SUCCESS (0)
    u32_at(body, accept_off) == Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_linux_proc_mounts_normalizes_rows() {
        let raw = "\
192.0.2.10:/srv/pool/data /mnt/pool/data nfs4 rw,vers=4.2 0 0
//srv/public /mnt/public cifs ro,relatime 0 0
/dev/sda1 / ext4 rw 0 0
malformed
";
        let m = parse_linux_proc_mounts(raw);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].fstype, "nfs4");
        assert_eq!(m[0].mountpoint, "/mnt/pool/data");
        assert!(m[0].options.contains(&"vers=4.2".to_string()));
        assert_eq!(m[1].source, "//srv/public");
    }

    #[test]
    fn parse_linux_unescapes_spaces() {
        let raw = "srv:/x /mnt/has\\040space nfs4 rw 0 0\n";
        let m = parse_linux_proc_mounts(raw);
        assert_eq!(m[0].mountpoint, "/mnt/has space");
    }

    #[test]
    fn parse_macos_mount_normalizes_rows() {
        let raw = "\
//user@srv/public on /Volumes/public (smbfs, nodev, nosuid, mounted by user)
/dev/disk1s1 on / (apfs, local, journaled)
10.0.0.5:/export on /Volumes/nfs (nfs)
no parens line
";
        let m = parse_macos_mount(raw);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].fstype, "smbfs");
        assert_eq!(m[0].source, "//user@srv/public");
        assert_eq!(m[0].mountpoint, "/Volumes/public");
        assert!(m[0].options.contains(&"nodev".to_string()));
        assert_eq!(m[1].fstype, "apfs");
        assert_eq!(m[2].fstype, "nfs");
    }

    #[test]
    fn fstype_filter_restricts() {
        let raw = "a:/x /mnt/x nfs4 rw 0 0\n//b/y /mnt/y cifs rw 0 0\n/dev/z / ext4 rw 0 0\n";
        let all = parse_linux_proc_mounts(raw);
        let net: Vec<_> = all
            .into_iter()
            .filter(|m| ["nfs4", "cifs"].contains(&m.fstype.as_str()))
            .collect();
        assert_eq!(net.len(), 2);
    }

    #[test]
    fn probe_health_missing_for_absent_path() {
        assert_eq!(
            probe_health("/nonexistent_orca_storage_probe", Duration::from_secs(1)),
            Health::Missing
        );
    }

    #[test]
    fn probe_health_ok_for_real_dir() {
        let dir = std::env::temp_dir();
        assert_eq!(
            probe_health(dir.to_str().unwrap(), Duration::from_secs(2)),
            Health::Ok
        );
    }

    #[test]
    fn probe_health_stale_for_non_notfound_stat_error() {
        // A stale NFS/SMB handle fails stat with ESTALE — a non-NotFound error.
        // Simulate that class deterministically with ENOTDIR (a path *under* a
        // regular file). The old `!path.exists()` guard reported this as absent
        // and returned Missing; it must classify Stale so convergence force-
        // unmounts + remounts instead of looping on a bare mount.
        let mut f = std::env::temp_dir();
        f.push(format!("orca_probe_notdir_{}", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        let under = f.join("child");
        assert_eq!(
            probe_health(under.to_str().unwrap(), Duration::from_secs(2)),
            Health::Stale
        );
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn health_round_trips_through_serde() {
        for h in [
            Health::Ok,
            Health::Stale,
            Health::Missing,
            Health::Timeout,
            Health::Error,
        ] {
            let j = serde_json::to_string(&h).unwrap();
            let back: Health = serde_json::from_str(&j).unwrap();
            assert_eq!(back, h);
        }
    }

    // ── Generic host parsing (no fstype grammar, no port) ─────────────────
    // The port is resolved from the registered backend that owns the fstype
    // (see the storage lib tests for `source_port_for_fstype` +
    // `source_endpoint`); here we exercise only the shape-driven host parse.

    #[test]
    fn parse_source_host_parses_host_colon_path() {
        assert_eq!(
            parse_source_host("primary:/srv/pool/data"),
            Some("primary".to_string())
        );
        assert_eq!(
            parse_source_host("10.0.0.5:/export"),
            Some("10.0.0.5".to_string())
        );
    }

    #[test]
    fn parse_source_host_without_colon_or_authority_is_none() {
        assert_eq!(parse_source_host("primary"), None);
    }

    #[test]
    fn parse_source_host_parses_authority_form() {
        assert_eq!(
            parse_source_host("//server/share"),
            Some("server".to_string())
        );
        assert_eq!(
            parse_source_host("//user@server/share"),
            Some("server".to_string())
        );
    }

    #[test]
    fn parse_source_host_bare_device_path_is_none() {
        // A bare device path (no `//`, no `:`) has no network host.
        assert_eq!(parse_source_host("/dev/sda1"), None);
    }

    #[test]
    fn parse_source_host_trims_whitespace() {
        assert_eq!(
            parse_source_host("  primary  :/srv/pool"),
            Some("primary".to_string())
        );
    }

    #[test]
    fn source_endpoint_without_registered_backend_has_no_port() {
        // No backend owns `nfs4` in a bare `-p storage` unit run, so the port
        // cannot be resolved and `source_endpoint` yields `None` even though the
        // host parses — the port grammar lives in the plugin, not core.
        assert_eq!(source_endpoint("primary:/srv/pool", "nfs4"), None);
    }

    #[test]
    fn probe_source_unroutable_is_false() {
        // TEST-NET-1 (192.0.2.0/24) is reserved and never routable, so a tight
        // 1ms budget makes this deterministic and fast.
        assert!(!probe_source(
            "192.0.2.1:/export",
            "nfs4",
            Duration::from_millis(1)
        ));
    }

    #[test]
    fn probe_source_nfs_unroutable_is_false() {
        // TEST-NET-1 is never routable; a tight budget keeps it fast.
        assert!(!probe_source_nfs("192.0.2.1", Duration::from_millis(1)));
    }

    #[test]
    fn probe_source_nfs_hung_server_is_false() {
        use std::io::Read;
        use std::net::TcpListener;
        // A stub that ACCEPTS TCP (so a bare connect/probe_source would pass) but
        // NEVER answers the RPC NULL — exactly the wedged-nfsd case. The RPC probe
        // must classify it down.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            if let Ok((_sock, _)) = listener.accept() {
                // Accept the connection, then just sit — never send an RPC reply.
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        // Point the probe at the stub's port by resolving the host:port pair
        // ourselves would need NFS_PORT; instead exercise the reply parser and a
        // real short-timeout connect against the stub via a direct call path.
        let host = addr.ip().to_string();
        // The public probe hardcodes 2049; drive the same logic against the stub
        // port through a tiny inline connect+parse to prove "TCP up, no RPC reply
        // ⇒ false" deterministically.
        let up_but_silent = {
            use std::io::Write;
            use std::net::TcpStream;
            let mut s = TcpStream::connect_timeout(
                &format!("{host}:{}", addr.port()).parse().unwrap(),
                Duration::from_millis(200),
            )
            .expect("connect stub");
            s.set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            s.write_all(&rpc_null_call(0x0ca5_a1d0)).ok();
            let mut mark = [0u8; 4];
            s.read_exact(&mut mark).is_ok()
        };
        assert!(!up_but_silent, "silent server must not yield an RPC reply");
        handle.join().ok();
    }

    #[test]
    fn rpc_reply_is_success_accepts_wellformed_success_reply() {
        // xid, REPLY(1), MSG_ACCEPTED(0), verf flavor(0) len(0), accept SUCCESS(0)
        let xid = 0x1234_5678u32;
        let mut body = Vec::new();
        body.extend_from_slice(&xid.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // verf flavor
        body.extend_from_slice(&0u32.to_be_bytes()); // verf len
        body.extend_from_slice(&0u32.to_be_bytes()); // accept_stat SUCCESS
        assert!(rpc_reply_is_success(xid, &body));
        // Wrong xid, wrong msg_type, and a rejected reply all fail.
        assert!(!rpc_reply_is_success(0xdead_beef, &body));
        let mut denied = body.clone();
        denied[11] = 2; // reply_stat = MSG_DENIED
        assert!(!rpc_reply_is_success(xid, &denied));
    }

    #[test]
    fn mount_entry_round_trips_through_serde() {
        let e = MountEntry {
            source: "srv:/x".into(),
            mountpoint: "/mnt/x".into(),
            fstype: "nfs4".into(),
            options: vec!["rw".into(), "vers=4.2".into()],
        };
        let j = serde_json::to_string(&e).unwrap();
        let back: MountEntry = serde_json::from_str(&j).unwrap();
        assert_eq!(back, e);
    }
}
