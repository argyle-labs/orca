//! Remaining server-side CLI commands (each slated to move into the
//! matching domain crate). What's left here:
//! - `daemon`     — daemon lifecycle (long-running supervisor); → `system`
//! - `dev_serve`  — binary update server; → `system`
//! - `hook_cmd`   — Claude Code hook handlers; → its own crate
//! - `package`    — deb/rpm/apk builders; → `system`
//! - `spec`       — disk-spec scaffold; → `docs`
//! - `system`     — host lifecycle CLI; → `system`
//! - `update`     — self-update CLI; → `system`

pub mod daemon;
pub mod dev_serve;
pub mod hook_cmd;
pub mod package;
pub mod spec;
pub mod system;
pub mod update;

pub use daemon::{DaemonAction, cmd_daemon};
pub use hook_cmd::{HookAction, cmd_hook};
pub use package::{PackageAction, cmd_package};
pub use spec::{SpecAction, cmd_spec};
pub use system::{SystemAction, cmd_system};
pub use update::startup_update_check;
