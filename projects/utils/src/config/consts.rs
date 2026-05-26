pub const APP_NAME: &str = "orca";
pub const APP_MCP_SERVER: &str = "orca-local";
pub const APP_DB_FILE: &str = "orca.db";
pub const APP_STATE_DIR: &str = ".orca";
pub const APP_PLIST_LABEL: &str = "com.orca.daemon";
pub const APP_DAEMON_LOG: &str = "/tmp/orca-daemon.log";
pub const APP_REPO_URL: &str = "https://github.com/scottdkey/orca";
pub const APP_REPO_API_URL: &str = "https://api.github.com/repos/scottdkey/orca";
pub const APP_SYSTEMD_SERVICE: &str = "orca";
pub const APP_KEYRING_SERVICE: &str = "orca";
/// Subdirectory inside APP_STATE_DIR where PKI material (CA, certs) is stored.
pub const APP_PKI_DIR: &str = "pki";
/// Default TCP port the plugin RPC host listens on (pod mesh mTLS).
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
