use anyhow::Result;
use crate::config::Config;
use crate::context::ProjectContext;
use crate::session::Session;

pub async fn cmd_run(config: &Config, agent: &str, prompt: &str) -> Result<()> {
    let ctx = ProjectContext::default();
    let mut session = Session::new(config.clone(), ctx).await?;

    // If a specific agent was requested, wrap as a delegation task
    if agent != "wolf" && agent != "brain" {
        let delegate_prompt = format!("Delegate this to @{agent}: {prompt}");
        session.one_shot(delegate_prompt).await
    } else {
        session.one_shot(prompt.to_string()).await
    }
}
