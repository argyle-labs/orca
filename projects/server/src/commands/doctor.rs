use anyhow::Result;
use colored::Colorize;
use orca_utils::config::APP_NAME;
use orca_utils::config::Config;

pub fn cmd_doctor(config: &Config) -> Result<()> {
    let mut issues: Vec<String> = Vec::new();
    let mut ok_count = 0;

    // 1. Orca vault exists
    if config.app_dir.exists() {
        println!(
            "  {} {APP_NAME} vault: {}",
            "✓".green(),
            config.app_dir.display()
        );
        ok_count += 1;
    } else {
        issues.push(format!(
            "{APP_NAME} vault not found at {}",
            config.app_dir.display()
        ));
    }

    // 2. Agents dir exists and has files
    let agents_dir = config.agents_dir();
    let agent_files: Vec<_> = if agents_dir.exists() {
        std::fs::read_dir(&agents_dir)?
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .collect()
    } else {
        issues.push(format!("agents dir not found: {}", agents_dir.display()));
        vec![]
    };

    // 3. Validate each agent file has required frontmatter
    let mut agent_names: Vec<String> = Vec::new();
    for entry in &agent_files {
        let path = entry.path();
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        agent_names.push(stem.clone());

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let has_name = content.contains("name:");
        let has_desc = content.contains("description:");
        let has_tools = content.contains("tools:");

        if !has_name || !has_desc || !has_tools {
            let missing: Vec<&str> = [
                if !has_name { Some("name") } else { None },
                if !has_desc { Some("description") } else { None },
                if !has_tools { Some("tools") } else { None },
            ]
            .into_iter()
            .flatten()
            .collect();
            issues.push(format!(
                "{}.md: missing frontmatter: {}",
                stem,
                missing.join(", ")
            ));
        } else {
            ok_count += 1;
        }
    }
    println!(
        "  {} {} agent definitions found",
        "✓".green(),
        agent_files.len()
    );

    // 4. Cross-reference wolf.md routing table
    let wolf_path = agents_dir.join("wolf.md");
    if wolf_path.exists() {
        let wolf_content = std::fs::read_to_string(&wolf_path)?;
        // Extract agent names from wolf's table (lines like "| **@name** |")
        let wolf_agents: Vec<String> = wolf_content
            .lines()
            .filter_map(|line| {
                if let Some(start) = line.find("**@") {
                    let rest = &line[start + 3..];
                    rest.find("**").map(|end| rest[..end].to_string())
                } else {
                    None
                }
            })
            .collect();

        // Agents with files but not in wolf's table
        for name in &agent_names {
            if name != "wolf" && !wolf_agents.contains(name) {
                issues.push(format!(
                    "{}.md exists but not in wolf.md routing table",
                    name
                ));
            }
        }

        // Agents in wolf's table but no file
        for name in &wolf_agents {
            if !agent_names.contains(name) {
                issues.push(format!(
                    "@{} in wolf.md routing table but no {}.md file",
                    name, name
                ));
            }
        }

        if wolf_agents.len() == agent_names.len() - 1 {
            // -1 because wolf itself isn't in its own table
            ok_count += 1;
        }
    }

    // 5. Logs dir exists and is writable
    let logs_dir = config.logs_dir();
    if logs_dir.exists() {
        let test_file = logs_dir.join(".doctor_test");
        match std::fs::write(&test_file, "test") {
            Ok(_) => {
                let _ = std::fs::remove_file(&test_file);
                println!("  {} logs dir: writable", "✓".green());
                ok_count += 1;
            }
            Err(_) => issues.push(format!("logs dir not writable: {}", logs_dir.display())),
        }
    } else {
        issues.push(format!("logs dir not found: {}", logs_dir.display()));
    }

    // 6. Memory root exists
    if config.memory_root.exists() {
        let project_count = std::fs::read_dir(&config.memory_root)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .count();
        println!("  {} memory root: {} projects", "✓".green(), project_count);
        ok_count += 1;
    } else {
        issues.push(format!(
            "memory root not found: {}",
            config.memory_root.display()
        ));
    }

    // 7. API key
    if config.anthropic_api_key.is_some() {
        println!("  {} anthropic API key: set", "✓".green());
        ok_count += 1;
    } else {
        println!(
            "  {} anthropic API key: not set (escalation unavailable)",
            "⚠".yellow()
        );
    }

    // Report
    println!();
    if issues.is_empty() {
        println!(
            "{}",
            format!("  all clear — {} checks passed", ok_count).green()
        );
    } else {
        println!("{}", format!("  {} issue(s) found:", issues.len()).red());
        for issue in &issues {
            println!("    {} {}", "✗".red(), issue);
        }
        println!();
        println!("  {} checks passed", ok_count);
    }

    Ok(())
}
