use anyhow::Result;
use brain_agents as agents;
use brain_utils::config::Config;
use colored::Colorize;

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

pub fn cmd_install_agents(config: &Config) -> Result<()> {
    let target = config.agents_dir();
    println!(
        "{} installing agents into {}",
        "brain".cyan(),
        target.display()
    );

    let report = agents::install_agents(&target)?;

    for name in &report.written {
        println!("  {} {name}", "↑".green());
    }
    for name in &report.removed {
        println!("  {} {name}", "✗".red());
    }

    println!(
        "\n{} {} written, {} removed, {} unchanged",
        "✓".green(),
        report.written.len(),
        report.removed.len(),
        report.unchanged,
    );
    Ok(())
}
