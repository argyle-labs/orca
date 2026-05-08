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
/// Default TCP port the plugin RPC host listens on.
pub const APP_PLUGIN_PORT: u16 = 12002;
