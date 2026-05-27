//! Conversation crate — interactive REPL/TUI sessions, background agent
//! jobs, session logs. Carved out of the server crate so the HTTP daemon
//! doesn't carry REPL code, and so other front-ends (mobile, web shell)
//! can embed the session loop directly.

pub mod jobs;
pub mod log_cmd;
pub mod run;
pub mod sessions;
