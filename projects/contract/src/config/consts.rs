pub const APP_NAME: &str = "orca";
/// Name the Claude MCP client registers the server under. It is the whole mesh
/// — every tool takes an id and the daemon routes — so "local" was a misnomer.
pub const APP_MCP_SERVER: &str = "orca";
/// Prior name, pruned from client configs on install/update (#538).
pub const APP_MCP_SERVER_LEGACY: &str = "orca-local";
pub const APP_DB_FILE: &str = "orca.db";
pub const APP_STATE_DIR: &str = ".orca";
pub const APP_PLIST_LABEL: &str = "com.orca.daemon";
/// Subdirectory inside APP_STATE_DIR for file-backed daemon logs.
pub const APP_LOGS_SUBDIR: &str = "logs";

/// The STRUCTURED log: one JSON object per line, written by the subscriber's
/// tee and rotated by it (32 MiB x 5). This is the daemon's real log — what
/// `system.logs` reads and `orca logs` opens.
pub const APP_DAEMON_JSONL_FILE: &str = "daemon.jsonl";

/// Where the SUPERVISOR (launchd/systemd/openrc/unraid) captures the process's
/// stdout+stderr.
///
/// Deliberately NOT the structured log. Both pointed at `daemon.log` and the
/// subscriber also wrote its JSON to stderr, so every line was stored twice —
/// measured at 123 MB + 36 MB of identical content on mint, unrotated (#563).
/// orca cannot rotate this file: the supervisor holds the fd, so a rename
/// leaves it writing to the moved inode. So the subscriber keeps stderr clean
/// instead, and this file holds only what the tee cannot: output from before
/// logging is initialised, panics, and allocator warnings. It stays small.
pub const APP_DAEMON_STDERR_FILE: &str = "daemon.stderr.log";

/// Pre-split filename. Still named so install can tell an operator that the
/// old, unbounded file is now orphaned and safe to delete.
pub const APP_DAEMON_LOG_FILE_LEGACY: &str = "daemon.log";
pub const APP_REPO_URL: &str = "https://github.com/argyle-labs/orca";
pub const APP_REPO_API_URL: &str = "https://api.github.com/repos/argyle-labs/orca";
pub const APP_SYSTEMD_SERVICE: &str = "orca";
pub const APP_KEYRING_SERVICE: &str = "orca";
/// Subdirectory inside APP_STATE_DIR where PKI material (CA, certs) is stored.
pub const APP_PKI_DIR: &str = "pki";
/// Default TCP port the plugin RPC host listens on (mesh mTLS).
pub const APP_PLUGIN_PORT: u16 = 12002;

/// Default TCP port for plain HTTP REST + UI. Homelab-friendly default;
/// no internal CA required. All operator-facing tools default to this
/// when no `--port` override is given. Overridable via orca.toml.
pub const APP_REST_HTTP_PORT: u16 = 12000;

/// Default TCP port for HTTPS REST + UI. Uses the mesh CA server cert
/// by default; production / public exposure usually fronts this with
/// Caddy on an edge peer. Overridable via orca.toml.
pub const APP_REST_HTTPS_PORT: u16 = 12443;

/// Subdirectory inside APP_STATE_DIR that holds per-profile content
/// (`~/.orca/profiles/<profile-id>/`). Profile metadata + ACLs live in `orca.db`.
pub const APP_PROFILES_DIR: &str = "profiles";

/// Implicit local user identity used until multi-user auth is wired up.
/// All single-user installs operate as if this user is signed in. The schema
/// already accepts arbitrary user_ids, so multi-user just adds real identities
/// alongside this one without migration.
pub const LOCAL_USER: &str = "local";
