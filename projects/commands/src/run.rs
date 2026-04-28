use anyhow::Result;
use brain_utils::config::Config;
// NOTE: requires brain crate — crate::context::ProjectContext not yet extracted
// NOTE: requires brain crate — crate::session::Session not yet extracted

pub async fn cmd_run(config: &Config, agent: &str, prompt: &str) -> Result<()> {
    // TODO: crate::context stays in server for now
    // TODO: crate::session stays in server for now
    let _ = (config, agent, prompt);
    anyhow::bail!("cmd_run requires session/context — must remain in server crate for now")
}
