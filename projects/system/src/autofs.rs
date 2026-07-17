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
use crate::source_election::{Election, RemountAggression, Transition, elect, transition};
use plugin_toolkit::storage::{Health, mount_table_of, probe_health, probe_source};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// Network filesystem types orca elects/reads sources for. Backend-agnostic:
/// NFS today, SMB (`cifs`/`smbfs`) next — the same election path serves both.
const NET_FSTYPES: &[&str] = &["nfs4", "nfs", "cifs", "smbfs"];

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
    /// Explicit unix mode to enforce after writing (e.g. `0o600` for a secret
    /// creds-file). `None` leaves the mode at the process umask default — the
    /// behavior for the world-readable autofs map + master files. Serialized as a
    /// plain integer so the field crosses the JSON seam without a mode newtype.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// The privileged operation handed to `orca admin storage-apply` over stdin.
/// A closed, validated vocabulary — the helper does exactly these and nothing
/// else, so the `sudo` grant is a narrow, auditable surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PrivilegedOp {
    /// Write the given config files, then restart autofs via `init`. The daemon
    /// only emits this when at least one file actually differs from disk.
    ///
    /// `keep_creds` is the authoritative set of SMB creds-file paths that should
    /// exist after this apply (every currently-declared inline-SMB mount). The
    /// root helper reaps any creds-file in `SMB_CREDS_DIR` NOT in this set — the
    /// teardown path for a deleted mount or one whose creds changed. It is
    /// distinct from `writes` because an unchanged creds-file is not rewritten but
    /// must still be kept.
    Apply {
        writes: Vec<FileWrite>,
        #[serde(default)]
        keep_creds: Vec<String>,
        init: Init,
    },
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
    let opts = autofs_options(m);
    let locations = ordered_sources(&m.source, m.failover_sources.as_deref()).join(" ");
    format!("{}  {}  {}", m.target, opts, locations)
}

/// Render the direct-map body writing the **single elected source** per mount
/// instead of all ordered sources. `elected` maps a mount `target` to the source
/// its election chose; a mount absent from the map (no live source) is omitted
/// so autofs is never handed a dead location. This is the failback-correct
/// renderer the daemon uses — [`render_map`] (all sources on one line) is kept
/// only for the legacy no-election path.
///
/// Byte-stable (sorted by target, same header) so the on-disk diff stays a
/// reliable drift check.
pub fn render_map_elected(
    mounts: &[ManagedMount],
    elected: &std::collections::HashMap<String, String>,
) -> String {
    let mut lines: Vec<String> = mounts
        .iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .filter_map(|m| elected.get(&m.target).map(|src| map_line_for(m, src)))
        .collect();
    lines.sort();

    let mut map = String::from(HEADER);
    for line in &lines {
        map.push_str(line);
        map.push('\n');
    }
    map
}

/// One direct-map line pinned to a single elected `source`:
/// `<target>  -fstype=…,<opts>  <source>`. Same shape as [`map_line`] but with
/// exactly one location so autofs cannot silently drift to a lower-priority
/// server — orca owns source selection.
fn map_line_for(m: &ManagedMount, source: &str) -> String {
    let opts = autofs_options(m);
    format!("{}  {}  {}", m.target, opts, source)
}

/// Build the autofs `-fstype=…,opt,opt` option string for a mount. The option
/// string is produced by the owning storage backend's `render_options` (so the
/// backend owns its option grammar) rather than by a local comma-split; core then
/// strips the fstab/systemd-only options (`_netdev`, `nofail`, `x-systemd.*`,
/// `auto`/`noauto`) that are meaningless to — and would invalidate — an autofs map
/// entry. A mount whose backend is not registered falls back to the mount's raw
/// option string, so map rendering never depends on a live registry.
fn autofs_options(m: &ManagedMount) -> String {
    let rendered = render_backend_options(&m.backend, m.fstype.as_str(), m.options.as_deref());
    let base = strip_fstab_only(&m.fstype, &rendered);
    // Inline-SMB mounts reference their root-owned creds-file; the backend's
    // `render_options` deliberately emits nothing for inline credentials so no
    // `username=`/`password=` leaks into the world-readable map. Core stamps the
    // concrete `credentials=<path>` here, where the mount target is known.
    if needs_inline_creds(m) {
        use plugin_toolkit::storage::creds_file_path;
        format!("{base},credentials={}", creds_file_path(&m.target))
    } else {
        base
    }
}

/// Render a mount's options through the registered backend named `backend`,
/// falling back to the raw declared string when no such backend is registered.
/// Kept separate so it is trivially testable without touching the global registry.
fn render_backend_options(backend: &str, _fstype: &str, options: Option<&str>) -> String {
    use plugin_toolkit::storage::{
        NormalizedSpec, OptionSet, SecretRef, backend as lookup, render_option_set,
    };
    match lookup(backend) {
        Some(b) => {
            // The autofs map is rendered synchronously and per-source, so we
            // render from the raw option string via the backend's own
            // `render_options`. A backend that has not migrated to a typed
            // `OptionSet` renders `Raw` verbatim — byte-identical to core's prior
            // behavior; a migrated backend applies its own grammar.
            let normalized = NormalizedSpec {
                backend: backend.to_string(),
                target: String::new(),
                fstype: _fstype.to_string(),
                source: String::new(),
                failover_sources: Vec::new(),
                options: OptionSet::Raw {
                    options: options.map(str::to_string),
                },
                credential: None::<SecretRef>,
                remount_policy: None,
                enabled: true,
            };
            b.render_options(&normalized)
        }
        None => options
            .map(str::to_string)
            .map(|o| render_option_set(&OptionSet::Raw { options: Some(o) }))
            .unwrap_or_default(),
    }
}

/// Prepend `fstype=` and strip fstab/systemd-only options from a rendered option
/// string, producing the `-fstype=…,opt,opt` autofs map field. Splitting the
/// strip out from rendering keeps the backend's grammar (rendering) and autofs's
/// constraint (this filter) as separate, independently-tested concerns.
fn strip_fstab_only(fstype: &str, rendered: &str) -> String {
    let mut parts = vec![format!("fstype={fstype}")];
    parts.extend(
        rendered
            .split(',')
            .map(str::trim)
            .filter(|o| !o.is_empty() && !is_fstab_only(o))
            .map(str::to_string),
    );
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

/// Does a master-file line already register our direct map ([`MAP_FILE`])?
/// True for any non-comment line that mounts `/-` at `MAP_FILE` — the shape a
/// duplicate registration takes (whether hand-added or leaked from an
/// `auto.master.d` drop-in). Used by [`merge_master`] to keep exactly one.
fn registers_our_map(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return false;
    }
    t.split_whitespace().any(|field| field == MAP_FILE)
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
        // Guard against double-registration of our own direct map: a foreign
        // line (or a stale `auto.master.d` drop-in copied into the master file)
        // that already points autofs at `MAP_FILE`. We re-add it inside the
        // orca block, so keeping this one would register `/etc/auto.orca` twice
        // and autofs would load the map twice. Drop it.
        if registers_our_map(line) {
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
    path == MAP_FILE
        || path == "/etc/auto.master"
        || path == "/etc/autofs/auto.master"
        // A root-owned 0600 SMB creds-file, but ONLY a path that is a legal,
        // traversal-proof creds-file inside `SMB_CREDS_DIR` (see
        // `storage::is_valid_creds_file_path`) — never an arbitrary path.
        || plugin_toolkit::storage::is_valid_creds_file_path(path)
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
    plan_with_map(mounts, render_map(mounts)).await
}

/// [`plan`] against a pre-rendered map body. Shared by the legacy all-sources
/// path ([`render_map`]) and the elected single-source path
/// ([`render_map_elected`]); both need the same master take-over-merge + diff.
async fn plan_with_map(mounts: &[ManagedMount], map: String) -> PrivilegedOp {
    let master_path = detect_master_file();
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
                mode: None,
            });
        }
    }

    // Materialize a root-owned 0600 creds-file for every enabled SMB mount that
    // declares inline credentials. Prepended so the file exists before autofs is
    // restarted and can serve the very first mount. NFS and file/guest-SMB mounts
    // set no `credential`, so they never enter this loop and are unaffected.
    let mut creds_writes = plan_smb_creds_writes(mounts).await;
    creds_writes.extend(writes);

    // The authoritative post-apply keep-set: every currently-declared inline-SMB
    // mount's creds-file path (whether or not it was rewritten this run). The
    // root helper reaps any creds-file not in this set.
    let keep_creds = mounts
        .iter()
        .filter(|m| needs_inline_creds(m))
        .map(|m| plugin_toolkit::storage::creds_file_path(&m.target))
        .collect();

    PrivilegedOp::Apply {
        writes: creds_writes,
        keep_creds,
        init: detect_init(),
    }
}

/// True for a mount that needs an inline-SMB creds-file: an enabled network share
/// on an SMB transport (`cifs`/`smbfs`) that carries a credential [`SecretRef`].
/// This is the exact discriminator that keeps NFS and file/guest-SMB mounts out
/// of the creds-file path — neither sets `credential` for a creds-file, and only
/// SMB uses `credentials=<path>` in its map entry.
fn needs_inline_creds(m: &ManagedMount) -> bool {
    m.enabled
        && m.kind == "network_share"
        && matches!(m.fstype.as_str(), "cifs" | "smbfs")
        && m.credential.as_deref().is_some_and(|c| !c.is_empty())
}

/// Build the creds-file [`FileWrite`]s (mode `0600`) for every inline-SMB mount,
/// resolving each mount's credential [`SecretRef`] to plaintext via the secrets
/// domain. The plaintext lives only in the returned `contents`, destined for a
/// root-owned `0600` file — it is never logged and never rendered into the map.
/// A mount whose secret fails to resolve is skipped (the map still references the
/// creds-file, so the mount fails closed — it will not authenticate — which is
/// strictly safer than falling back to inline creds in the map).
async fn plan_smb_creds_writes(mounts: &[ManagedMount]) -> Vec<FileWrite> {
    use plugin_toolkit::storage::{creds_file_path, render_creds_file};

    let mut writes = Vec::new();
    for m in mounts.iter().filter(|m| needs_inline_creds(m)) {
        let Some(secret_name) = m.credential.as_deref() else {
            continue;
        };
        // The inline SMB secret is stored as a small JSON blob under the mount's
        // secret name: `{ "username", "password", "domain"? }`. Core resolves the
        // ref via the secrets domain; the smb plugin writes it there at declare
        // time. A resolve/parse failure is logged WITHOUT the value and the mount
        // is left to fail closed.
        match resolve_smb_creds(secret_name) {
            Ok((username, password, domain)) => {
                let path = creds_file_path(&m.target);
                let contents = render_creds_file(&username, &password, domain.as_deref());
                // Diff against disk so an unchanged creds-file yields no write —
                // preserving apply idempotency (no needless autofs restart) and
                // keeping the plaintext off the wire when nothing changed.
                let on_disk = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                if on_disk != contents {
                    writes.push(FileWrite {
                        path,
                        contents,
                        mode: Some(0o600),
                    });
                }
            }
            Err(e) => {
                tracing::warn!(
                    target = %m.target,
                    "resolve inline SMB credential: {e}; mount will fail closed"
                );
            }
        }
    }
    writes
}

/// Resolve a mount's credential [`SecretRef`] name to `(username, password,
/// domain)`. The inline SMB secret is a JSON object `{username, password,
/// domain?}` stored in the encrypted secrets domain. Returns an error (never the
/// value) on a missing secret or a malformed blob.
fn resolve_smb_creds(secret_name: &str) -> anyhow::Result<(String, String, Option<String>)> {
    #[derive(serde::Deserialize)]
    struct InlineSmb {
        username: String,
        password: String,
        #[serde(default)]
        domain: Option<String>,
    }
    let raw = plugin_toolkit::secrets::get_required(secret_name)?;
    let parsed: InlineSmb = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("malformed inline SMB secret blob: {e}"))?;
    Ok((parsed.username, parsed.password, parsed.domain))
}

/// Render + plan + apply for a mount set. Idempotent: an unchanged host makes no
/// privileged call at all.
pub async fn apply(mounts: &[ManagedMount]) -> ApplyOutcome {
    apply_op(plan(mounts).await).await
}

/// Render the **elected single-source** map, plan, and apply. This is the
/// failback-correct daemon path: each mount is pinned to the source its election
/// chose (see [`crate::source_election`]). Idempotent — no privileged call when
/// the on-disk map already matches.
pub async fn apply_elected(
    mounts: &[ManagedMount],
    elected: &std::collections::HashMap<String, String>,
) -> ApplyOutcome {
    apply_op(plan_with_map(mounts, render_map_elected(mounts, elected)).await).await
}

/// Run a planned [`PrivilegedOp::Apply`], short-circuiting an empty diff.
async fn apply_op(op: PrivilegedOp) -> ApplyOutcome {
    match op {
        PrivilegedOp::Apply { ref writes, .. } if writes.is_empty() => ApplyOutcome::default(),
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
        PrivilegedOp::Apply {
            writes,
            keep_creds,
            init,
        } => {
            let mut res = PrivilegedResult::default();
            for w in &writes {
                if !is_allowed_write(&w.path) {
                    res.errors
                        .push(format!("refused non-allowlisted path: {}", w.path));
                    continue;
                }
                match write_atomic(&w.path, &w.contents, w.mode).await {
                    Ok(()) => res.changed.push(w.path.clone()),
                    Err(e) => res.errors.push(format!("write {}: {e}", w.path)),
                }
            }
            // Teardown: prune creds-files not in the authoritative keep-set. When
            // a mount is deleted or its creds change target, its stale creds-file
            // is removed so a resolved secret never lingers on disk. Scoped to
            // `SMB_CREDS_DIR`; foreign files there are left alone.
            reap_orphan_creds_files(&keep_creds, &mut res).await;
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
/// over the target so a reader never sees a half-written map. When `mode` is set
/// (a secret creds-file), the mode is applied to the **temp file before rename**
/// so the file is never visible at a laxer mode — there is no window in which the
/// secret is world-readable.
async fn write_atomic(path: &str, contents: &str, mode: Option<u32>) -> std::io::Result<()> {
    let p = Path::new(path);
    if let Some(dir) = p.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = format!("{path}.orca.tmp");
    tokio::fs::write(&tmp, contents).await?;
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m)).await?;
    }
    tokio::fs::rename(&tmp, path).await
}

/// Remove creds-files under [`plugin_toolkit::storage::SMB_CREDS_DIR`] not in the
/// authoritative `keep` set — the teardown path. A deleted mount, or one whose
/// creds moved to file/guest/none, is absent from `keep`, so its stale creds-file
/// is reaped here rather than lingering with a resolved secret on disk. Only files
/// this scan can prove are orca creds-files (valid creds-file names) are touched;
/// any foreign file in the directory is left alone. If the directory does not
/// exist yet (no inline-SMB mount has ever applied), this is a no-op.
async fn reap_orphan_creds_files(keep: &[String], res: &mut PrivilegedResult) {
    reap_orphan_creds_files_in(plugin_toolkit::storage::SMB_CREDS_DIR, keep, res).await
}

/// [`reap_orphan_creds_files`] against an explicit directory. Split out so the
/// teardown logic is testable without the fixed `SMB_CREDS_DIR` const. A file is
/// reaped iff its full path passes [`is_valid_creds_file_path`] (proving it is an
/// orca creds-file, not a foreign file) AND is absent from `keep`.
async fn reap_orphan_creds_files_in(dir: &str, keep: &[String], res: &mut PrivilegedResult) {
    use plugin_toolkit::storage::is_valid_creds_file_path;

    let kept: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();

    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(d) => d,
        Err(_) => return, // dir absent → nothing to reap
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let Some(path_str) = path.to_str() else {
            continue;
        };
        // Under the real SMB_CREDS_DIR the full path is validated. Under a test
        // dir the full path won't match SMB_CREDS_DIR, so validate the basename
        // shape via a synthesized SMB_CREDS_DIR-rooted path — same classification.
        let name = entry.file_name();
        let synth = format!(
            "{}/{}",
            plugin_toolkit::storage::SMB_CREDS_DIR,
            name.to_string_lossy()
        );
        if is_valid_creds_file_path(&synth)
            && !kept.contains(path_str)
            && let Err(e) = tokio::fs::remove_file(&path).await
        {
            res.errors.push(format!("reap creds-file {path_str}: {e}"));
        }
    }
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

// ── Source-liveness election + failback (the autofs-can't-do-it core) ──────────

/// The source currently mounted at `target`, read from the **live kernel mount
/// table** filtered to network filesystem types — not a `stat` on the autofs
/// trigger dir, which is a false positive (the trigger dir exists whether or not
/// anything is mounted through it). `None` means nothing network-shaped is
/// mounted there right now. Runtime-agnostic; offloaded to the blocking pool.
pub async fn current_source_for_target(target: &str) -> Option<String> {
    let target = target.to_string();
    tokio::task::spawn_blocking(move || {
        mount_table_of(NET_FSTYPES)
            .ok()?
            .into_iter()
            .find(|e| e.mountpoint == target)
            .map(|e| e.source)
    })
    .await
    .ok()
    .flatten()
}

/// Elect the first live source for one mount by TCP-probing its ordered sources
/// (real transport probe — NFS `:2049` / SMB `:445` — not a directory `stat`).
/// Deterministic: index 0 (primary) wins whenever live, so a recovered primary
/// always re-wins == fail-back. Returns [`Election::Empty`] if every source is
/// down. Each probe is offloaded so a black-holed host can't stall the runtime.
pub async fn elect_live_source(m: &ManagedMount, probe_timeout: Duration) -> Election {
    let sources = ordered_sources(&m.source, m.failover_sources.as_deref());
    let fstype = m.fstype.clone();
    let mut live = std::collections::HashSet::new();
    for src in &sources {
        let (s, f) = (src.clone(), fstype.clone());
        let ok = tokio::task::spawn_blocking(move || probe_source(&s, &f, probe_timeout))
            .await
            .unwrap_or(false);
        if ok {
            live.insert(src.clone());
        }
    }
    elect(&sources, |s| live.contains(s))
}

/// Is `target` currently held open by a process (a container reading it)?
/// Best-effort, unprivileged (`fuser -sm`): a busy mount must not be forcibly
/// remounted under the Safe policy — we log a pending failback instead. A probe
/// error is treated as **busy** (fail safe: never disrupt on uncertainty).
async fn is_busy(target: &str) -> bool {
    match Command::new("fuser")
        .args(["-sm", "--", target])
        .output()
        .await
    {
        // `fuser -s` exits 0 when *something* holds the path, 1 when nothing does.
        Ok(out) => out.status.success(),
        Err(_) => true,
    }
}

/// Reconcile one mount's live source: elect, compare to what's mounted, and
/// (when they differ) remount to the elected source per the `aggression` policy.
/// The map re-render is handled by the caller's `apply`; this drives the actual
/// mount swap. Returns the [`Transition`] taken so the caller logs it non-silently.
///
/// Safety (the Plex/Jellyfin guarantee): under [`RemountAggression::Safe`] a
/// **busy** mount is never force-swapped — the elected source is already in the
/// freshly-rendered map, so autofs serves it on the next idle re-trigger, and we
/// return the transition with a logged *pending* note. [`RemountAggression::Force`]
/// escalates a busy mount to a lazy force-unmount + retrigger.
pub async fn reconcile_source(
    m: &ManagedMount,
    aggression: RemountAggression,
    probe_timeout: Duration,
) -> (Transition, Vec<String>) {
    let mut errors = Vec::new();
    let sources = ordered_sources(&m.source, m.failover_sources.as_deref());
    let election = elect_live_source(m, probe_timeout).await;
    let current = current_source_for_target(&m.target).await;
    let trans = transition(&sources, current.as_deref(), &election);

    match &trans {
        // Nothing to do, or nothing we can do.
        Transition::Unchanged | Transition::EmptyTarget => {}
        // A swap is required (mount / degrade / failback). Choose safety.
        Transition::Mount { .. } | Transition::Degrade { .. } | Transition::FailBack { .. } => {
            let busy = is_busy(&m.target).await;
            match (aggression, busy) {
                // Not busy: a clean remount is safe under either policy.
                (_, false) => {
                    errors.extend(remount_to_elected(&m.target, probe_timeout).await);
                }
                // Busy + Safe (default): don't disrupt live I/O. The elected
                // source is already in the re-rendered map; autofs serves it on
                // next idle re-trigger. Caller logs the pending failback.
                (RemountAggression::Safe, true) => {}
                // Busy + Force (opt-in): escalate to lazy force-unmount.
                (RemountAggression::Force, true) => {
                    errors.extend(force_remount_to_elected(&m.target, probe_timeout).await);
                }
            }
        }
    }
    (trans, errors)
}

/// Clean remount of a not-busy target: lazy-detach the current mount so the next
/// access re-triggers autofs against the freshly-elected single-source map, then
/// re-access to bring it up now. Routes the unmount through the privileged seam.
async fn remount_to_elected(target: &str, _probe_timeout: Duration) -> Vec<String> {
    let mut errors = Vec::new();
    let r = run_privileged(&PrivilegedOp::Unmount {
        targets: vec![target.to_string()],
    })
    .await;
    errors.extend(r.errors);
    errors.extend(trigger(std::slice::from_ref(&target.to_string())).await);
    errors
}

/// Force remount of a **busy** target (opt-in `Force` policy only). Same lazy
/// unmount + retrigger — `umount -lf` detaches the namespace entry even while
/// held, so open handles drain against the old server while new access hits the
/// elected source. Killing holders (`fuser -k`) is intentionally NOT done here;
/// it would be the only place to add it and stays out unless a future explicit
/// opt-in demands it. Loud by contract: the caller logs a `warn!`.
async fn force_remount_to_elected(target: &str, probe_timeout: Duration) -> Vec<String> {
    remount_to_elected(target, probe_timeout).await
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

    fn elected(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(t, s)| (t.to_string(), s.to_string()))
            .collect()
    }

    #[test]
    fn map_line_for_pins_single_elected_source() {
        let m = mount(
            "data",
            "primary:/srv/pool/data",
            Some("secondary:/srv/pool/data"),
        );
        // even with a failover declared, the elected line carries ONE source
        assert_eq!(
            map_line_for(&m, "secondary:/srv/pool/data"),
            "/mnt/data  -fstype=nfs4,vers=4.2,hard,nconnect=4  secondary:/srv/pool/data"
        );
    }

    #[test]
    fn render_map_elected_writes_only_elected_source() {
        let m = mount("data", "primary:/d", Some("secondary:/d"));
        let map = render_map_elected(&[m], &elected(&[("/mnt/data", "primary:/d")]));
        let body: Vec<&str> = map.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(body.len(), 1);
        assert!(body[0].ends_with("primary:/d"));
        assert!(!body[0].contains("secondary"));
    }

    #[test]
    fn render_map_elected_omits_mounts_with_no_live_source() {
        // `up` has an election, `down` does not → only `up` is rendered
        let up = mount("up", "primary:/u", None);
        let down = mount("down", "primary:/x", None);
        let map = render_map_elected(&[up, down], &elected(&[("/mnt/up", "primary:/u")]));
        let body: Vec<&str> = map.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(body.len(), 1);
        assert!(body[0].starts_with("/mnt/up"));
    }

    #[test]
    fn render_map_elected_empty_is_header_only() {
        let m = mount("x", "primary:/x", None);
        // no election for the mount → nothing rendered
        assert_eq!(render_map_elected(&[m], &elected(&[])), HEADER);
    }

    #[test]
    fn merge_master_evicts_duplicate_direct_map_registration() {
        // A leaked/duplicate `/-  /etc/auto.orca` registration (e.g. copied from
        // an auto.master.d drop-in) must NOT survive — we re-add it in the block.
        let existing = format!("/net\t-hosts\n/-  {MAP_FILE} --timeout=60\n");
        let out = merge_master(&existing, &[]);
        // exactly one registration of our map, and it's inside the orca block
        assert_eq!(out.matches(MAP_FILE).count(), 1);
        assert!(out.contains("/net\t-hosts"));
        assert!(out.contains(BLOCK_BEGIN));
    }

    #[test]
    fn registers_our_map_matches_only_map_registrations() {
        assert!(registers_our_map(&format!("/-  {MAP_FILE} --timeout=60")));
        assert!(registers_our_map(&format!("/-\t{MAP_FILE}")));
        assert!(!registers_our_map("/net\t-hosts"));
        assert!(!registers_our_map(&format!("# /-  {MAP_FILE}")));
        assert!(!registers_our_map(""));
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
                mode: None,
            }],
            keep_creds: Vec::new(),
            init: Init::OpenRc,
        };
        let s = serde_json::to_string(&op).unwrap();
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
    }

    // ── autofs_options ────────────────────────────────────────────────────

    // These exercise the fstab-only strip + `-fstype=` framing that core applies
    // to whatever the backend's `render_options` produced. An unregistered backend
    // (the test path) renders its raw option string verbatim via `OptionSet::Raw`,
    // so `render_backend_options(_, fstype, opts)` then `strip_fstab_only` is the
    // exact prior `autofs_options(fstype, opts)` behavior — asserted byte-for-byte.
    fn autofs_options_raw(fstype: &str, options: Option<&str>) -> String {
        strip_fstab_only(fstype, &render_backend_options("nfs", fstype, options))
    }

    #[test]
    fn autofs_options_bare_fstype_when_no_options() {
        assert_eq!(autofs_options_raw("nfs4", None), "-fstype=nfs4");
    }

    #[test]
    fn autofs_options_empty_string_options_yields_bare_fstype() {
        assert_eq!(autofs_options_raw("nfs4", Some("")), "-fstype=nfs4");
    }

    #[test]
    fn autofs_options_keeps_real_opts_and_drops_fstab_only() {
        assert_eq!(
            autofs_options_raw(
                "nfs4",
                Some("_netdev,nofail,x-systemd.automount,vers=4.2,hard,nconnect=4,noauto,auto")
            ),
            "-fstype=nfs4,vers=4.2,hard,nconnect=4"
        );
    }

    #[test]
    fn autofs_options_trims_whitespace_around_opts() {
        assert_eq!(
            autofs_options_raw("cifs", Some(" ro , vers=3.0 ")),
            "-fstype=cifs,ro,vers=3.0"
        );
    }

    #[test]
    fn autofs_options_drops_empty_segments_from_double_commas() {
        assert_eq!(
            autofs_options_raw("nfs", Some("ro,,rw")),
            "-fstype=nfs,ro,rw"
        );
    }

    #[test]
    fn autofs_options_drops_comment_option() {
        assert_eq!(
            autofs_options_raw("nfs", Some("comment=x-gvfs-show,ro")),
            "-fstype=nfs,ro"
        );
    }

    // ── is_fstab_only ─────────────────────────────────────────────────────

    #[test]
    fn is_fstab_only_recognizes_systemd_and_fstab_opts() {
        assert!(is_fstab_only("_netdev"));
        assert!(is_fstab_only("nofail"));
        assert!(is_fstab_only("auto"));
        assert!(is_fstab_only("noauto"));
        assert!(is_fstab_only("comment=foo"));
        assert!(is_fstab_only("x-systemd.automount"));
        assert!(is_fstab_only("x-systemd.idle-timeout=60"));
    }

    #[test]
    fn is_fstab_only_passes_real_mount_opts() {
        assert!(!is_fstab_only("vers=4.2"));
        assert!(!is_fstab_only("hard"));
        assert!(!is_fstab_only("nconnect=4"));
        assert!(!is_fstab_only("ro"));
    }

    // ── master_line ───────────────────────────────────────────────────────

    #[test]
    fn master_line_points_at_map_file_with_timeout() {
        assert_eq!(
            master_line(),
            format!("/-  {MAP_FILE} --timeout={TIMEOUT_SECS}")
        );
    }

    // ── master_mountpoint ─────────────────────────────────────────────────

    #[test]
    fn master_mountpoint_extracts_first_field() {
        assert_eq!(master_mountpoint("/misc\t/etc/auto.misc"), Some("/misc"));
        assert_eq!(
            master_mountpoint("  /mnt/pool  /etc/auto.pool --ghost"),
            Some("/mnt/pool")
        );
    }

    #[test]
    fn master_mountpoint_none_for_blank_and_comment() {
        assert_eq!(master_mountpoint(""), None);
        assert_eq!(master_mountpoint("   "), None);
        assert_eq!(master_mountpoint("# a comment"), None);
        assert_eq!(master_mountpoint("   # indented comment"), None);
    }

    // ── render_map header + empty ─────────────────────────────────────────

    #[test]
    fn render_map_empty_input_is_header_only() {
        assert_eq!(render_map(&[]), HEADER);
    }

    #[test]
    fn render_map_all_disabled_is_header_only() {
        let mut m = mount("x", "primary:/x", None);
        m.enabled = false;
        assert_eq!(render_map(&[m]), HEADER);
    }

    #[test]
    fn render_map_starts_with_header_and_ends_with_newline() {
        let rendered = render_map(&[mount("a", "primary:/a", None)]);
        assert!(rendered.starts_with(HEADER));
        assert!(rendered.ends_with('\n'));
    }

    // ── map_line with multiline failovers ─────────────────────────────────

    #[test]
    fn map_line_joins_multiline_failover_sources() {
        let m = mount(
            "data",
            "primary:/srv/data",
            Some("secondary:/srv/data\ntertiary:/srv/data\n"),
        );
        assert_eq!(
            map_line(&m),
            "/mnt/data  -fstype=nfs4,vers=4.2,hard,nconnect=4  \
             primary:/srv/data secondary:/srv/data tertiary:/srv/data"
        );
    }

    #[test]
    fn map_line_single_source_when_no_failover() {
        let mut m = mount("solo", "primary:/s", None);
        m.options = None;
        assert_eq!(map_line(&m), "/mnt/solo  -fstype=nfs4  primary:/s");
    }

    // ── merge_master edge cases ───────────────────────────────────────────

    #[test]
    fn merge_master_empty_input_yields_only_block() {
        let out = merge_master("", &[]);
        assert!(out.starts_with(BLOCK_BEGIN));
        assert!(out.contains(&master_line()));
        assert!(out.trim_end().ends_with(BLOCK_END));
        assert_eq!(out.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn merge_master_trims_trailing_blank_foreign_lines() {
        let out = merge_master("/net\t-hosts\n\n\n", &[]);
        // No blank line should sit between the foreign entry and the block.
        assert!(out.contains(&format!("/net\t-hosts\n{BLOCK_BEGIN}")));
    }

    #[test]
    fn merge_master_no_managed_targets_keeps_all_foreign() {
        let existing = "/mnt/pool  /etc/auto.pool\n/misc  /etc/auto.misc\n";
        let out = merge_master(existing, &[]);
        assert!(out.contains("/mnt/pool  /etc/auto.pool"));
        assert!(out.contains("/misc  /etc/auto.misc"));
    }

    #[test]
    fn merge_master_evicts_exact_and_ancestor_shadows_keeps_descendant() {
        // Managing `/mnt/pool` evicts the exact entry and any ANCESTOR entry
        // that would shadow it (`/mnt`), but keeps a more-specific descendant
        // (`/mnt/pool/data`) and unrelated foreign entries.
        let existing = "/mnt  /etc/auto.mnt\n/mnt/pool  /etc/auto.pool\n/mnt/pool/data  /etc/auto.data\n/keep  -hosts\n";
        let out = merge_master(existing, &["/mnt/pool".to_string()]);
        assert!(!out.contains("/etc/auto.mnt"));
        assert!(!out.contains("/etc/auto.pool"));
        assert!(out.contains("/etc/auto.data"));
        assert!(out.contains("/keep  -hosts"));
    }

    // ── PrivilegedOp::Unmount + PrivilegedResult serde ────────────────────

    #[test]
    fn unmount_op_roundtrips_json() {
        let op = PrivilegedOp::Unmount {
            targets: vec!["/mnt/a".into(), "/mnt/b".into()],
        };
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("\"op\":\"unmount\""));
        assert_eq!(serde_json::from_str::<PrivilegedOp>(&s).unwrap(), op);
    }

    #[test]
    fn privileged_result_default_is_empty() {
        let r = PrivilegedResult::default();
        assert!(r.changed.is_empty() && r.errors.is_empty() && !r.restarted);
    }

    #[test]
    fn init_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Init::Systemd).unwrap(),
            "\"systemd\""
        );
        assert_eq!(serde_json::to_string(&Init::OpenRc).unwrap(), "\"open_rc\"");
    }

    // ── detect_master_file / detect_init are deterministic ────────────────

    #[test]
    fn detect_master_file_returns_a_known_path() {
        assert!(matches!(
            detect_master_file(),
            "/etc/autofs/auto.master" | "/etc/auto.master"
        ));
    }

    #[test]
    fn detect_init_returns_a_variant() {
        assert!(matches!(detect_init(), Init::Systemd | Init::OpenRc));
    }

    // ── write_atomic ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_atomic_creates_parent_and_leaves_no_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("deep/nested/auto.orca");
        let path = target.to_str().unwrap().to_string();
        write_atomic(&path, "body\n", None).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "body\n");
        assert!(!std::path::Path::new(&format!("{path}.orca.tmp")).exists());
    }

    #[tokio::test]
    async fn write_atomic_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("f");
        std::fs::write(&target, "old").unwrap();
        write_atomic(target.to_str().unwrap(), "new", None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    // ── execute_privileged: allowlist refusal (no root/restart needed) ────

    // ── inline-SMB creds-file seam ────────────────────────────────────────

    fn smb_inline_mount(name: &str) -> ManagedMount {
        ManagedMount {
            name: name.into(),
            backend: "smb".into(),
            kind: "network_share".into(),
            source: format!("//nas/{name}"),
            failover_sources: None,
            target: format!("/mnt/{name}"),
            fstype: "cifs".into(),
            options: Some("vers=3.1.1,ro".into()),
            // A credential SecretRef name marks this as an inline-creds mount.
            credential: Some("smb.svc.creds".into()),
            remount_policy: None,
            addresses: Vec::new(),
            enabled: true,
        }
    }

    #[test]
    fn needs_inline_creds_only_for_smb_with_secretref() {
        // inline SMB → yes
        assert!(needs_inline_creds(&smb_inline_mount("media")));

        // NFS with no credential → no (unaffected)
        assert!(!needs_inline_creds(&mount("data", "primary:/d", None)));

        // SMB with NO credential (guest / file-based) → no
        let mut guest = smb_inline_mount("guest");
        guest.credential = None;
        assert!(!needs_inline_creds(&guest));

        // SMB with empty credential string → no
        let mut empty = smb_inline_mount("empty");
        empty.credential = Some(String::new());
        assert!(!needs_inline_creds(&empty));

        // disabled → no
        let mut off = smb_inline_mount("off");
        off.enabled = false;
        assert!(!needs_inline_creds(&off));

        // NFS that somehow carries a credential ref → still no (fstype gate)
        let mut nfs_cred = mount("nfscred", "primary:/d", None);
        nfs_cred.credential = Some("something".into());
        assert!(!needs_inline_creds(&nfs_cred));
    }

    #[test]
    fn map_references_creds_file_and_never_the_secret() {
        use plugin_toolkit::storage::creds_file_path;
        let m = smb_inline_mount("media");
        let line = map_line(&m);
        // The map entry references the creds-file path…
        assert!(
            line.contains(&format!("credentials={}", creds_file_path("/mnt/media"))),
            "line: {line}"
        );
        // …and NEVER leaks the SecretRef name or an inline username/password.
        assert!(!line.contains("smb.svc.creds"), "secret ref leaked: {line}");
        assert!(
            !line.contains("username="),
            "inline username leaked: {line}"
        );
        assert!(!line.contains("password="), "password leaked: {line}");
    }

    #[test]
    fn nfs_map_entry_has_no_credentials_field() {
        // Regression guard: the creds-file stamping must not touch NFS mounts.
        let line = map_line(&mount("data", "primary:/d", None));
        assert!(
            !line.contains("credentials="),
            "nfs got a creds-file: {line}"
        );
    }

    #[test]
    fn allowlist_accepts_valid_creds_file_rejects_traversal() {
        use plugin_toolkit::storage::creds_file_path;
        // A legal creds-file path for a declared target is allowed.
        assert!(is_allowed_write(&creds_file_path("/mnt/media")));
        // The existing map/master paths still pass.
        assert!(is_allowed_write(MAP_FILE));
        // Traversal / out-of-scope paths are refused.
        assert!(!is_allowed_write("/etc/orca/smb-creds/../../shadow"));
        assert!(!is_allowed_write("/etc/orca/smb-creds/sub/x.creds"));
        assert!(!is_allowed_write("/etc/shadow"));
    }

    #[tokio::test]
    async fn write_atomic_enforces_0600_on_creds_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("mnt_media.creds");
        let path = target.to_str().unwrap();
        write_atomic(path, "username=svc\npassword=p\n", Some(0o600))
            .await
            .unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "creds-file must be 0600");
        // No temp file left behind, and the secret is on disk exactly once.
        assert!(!std::path::Path::new(&format!("{path}.orca.tmp")).exists());
    }

    #[tokio::test]
    async fn reap_removes_orphan_creds_but_keeps_declared_and_foreign() {
        // Real end-to-end teardown: a declared (kept) creds-file, an orphan
        // creds-file (deleted mount), and a foreign file must be, respectively,
        // kept, removed, and left alone.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap().to_string();
        let kept = tmp.path().join("mnt_keep.creds");
        let orphan = tmp.path().join("mnt_gone.creds");
        let foreign = tmp.path().join("README.txt");
        std::fs::write(&kept, "username=a\npassword=b\n").unwrap();
        std::fs::write(&orphan, "username=x\npassword=y\n").unwrap();
        std::fs::write(&foreign, "not ours").unwrap();

        let keep = [kept.to_str().unwrap().to_string()];
        let mut res = PrivilegedResult::default();
        reap_orphan_creds_files_in(&dir, &keep, &mut res).await;

        assert!(kept.exists(), "declared creds-file must survive");
        assert!(!orphan.exists(), "orphan creds-file must be reaped");
        assert!(foreign.exists(), "foreign file must be left alone");
        assert!(
            res.errors.is_empty(),
            "clean reap has no errors: {:?}",
            res.errors
        );
    }

    #[tokio::test]
    async fn execute_privileged_refuses_non_allowlisted_path() {
        let op = PrivilegedOp::Apply {
            writes: vec![FileWrite {
                path: "/etc/passwd".into(),
                contents: "x".into(),
                mode: None,
            }],
            keep_creds: Vec::new(),
            init: Init::OpenRc,
        };
        let res = execute_privileged(op).await;
        assert!(res.changed.is_empty());
        assert!(!res.restarted, "no restart when nothing written");
        assert!(
            res.errors
                .iter()
                .any(|e| e.contains("refused non-allowlisted"))
        );
    }
}
