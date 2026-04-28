pub mod agents;
pub mod auth;
pub mod codegen;
pub mod doctor;
pub mod log_cmd;
pub mod projects;
pub mod run;
pub mod spec;

pub use spec::{SpecAction, cmd_spec};
pub use log_cmd::{LogAction, cmd_log};
pub use auth::{cmd_login, cmd_logout, cmd_auth, cmd_escalate};
pub use agents::{cmd_agents, cmd_install_agents};
pub use doctor::cmd_doctor;
pub use projects::cmd_projects;
pub use run::cmd_run;
pub use codegen::cmd_gen;
