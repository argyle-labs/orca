use anyhow::Result;
use brain_utils::config::Config;
use brain_utils::consts::APP_NAME;
use colored::Colorize;

pub fn cmd_projects(config: &Config) -> Result<()> {
    let root = &config.memory_root;
    if !root.exists() {
        println!("{}", format!("{APP_NAME} vault not found at ~/.{APP_NAME}").red());
        return Ok(());
    }
    println!("{}", "Projects:".green());
    let mut entries: Vec<_> = std::fs::read_dir(root)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let marker = if e.path().join("MEMORY.md").exists() {
            "●"
        } else {
            "○"
        };
        println!("  {marker}  {name}");
    }
    Ok(())
}
