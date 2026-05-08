use anyhow::Result;
use colored::Colorize;
use config::Config;
use orca_agents as agents;

pub fn cmd_agents(_config: &Config) -> Result<()> {
    println!("{}", "Agents:".green());
    for (name, desc) in agents::list_embedded_agents() {
        let short: String = desc.chars().take(72).collect();
        let ellipsis = if desc.len() > 72 { "…" } else { "" };
        println!(
            "  {}  {}{}",
            format!("@{name:<10}").cyan(),
            short.dimmed(),
            ellipsis.dimmed()
        );
    }
    Ok(())
}
