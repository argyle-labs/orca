use anyhow::Result;
use colored::Colorize;
use config::APP_NAME;
use config::Config;
use db;

const KEY_NAME: &str = "anthropic_api_key";

pub fn cmd_login(config: &Config) -> Result<()> {
    if let Some(key) = &config.anthropic_api_key {
        println!(
            "{} API key already set: {}",
            "✓".green(),
            db::settings::mask_key(key).dimmed()
        );
        println!("  {}  no", "[1]".dimmed());
        println!("  {}  yes, replace", "[2]".dimmed());
        print!("{} ", "[1]:".cyan());
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
        if input.trim() != "2" {
            return Ok(());
        }
    }

    println!("{}", "Enter your Anthropic API key (sk-ant-…):".cyan());
    println!(
        "{}",
        "  Get one at: https://console.anthropic.com/settings/keys".dimmed()
    );
    print!("> ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let key = rpassword_or_stdin()?;
    let key = key.trim().to_string();

    if !db::settings::looks_like_anthropic_key(&key) {
        eprintln!(
            "{}",
            "key doesn't look right (expected sk-ant-…) — saving anyway".yellow()
        );
    }

    let conn = db::open_default()?;
    db::settings::secret_set(&conn, KEY_NAME, &key)?;
    println!("{}", "API key stored in encrypted orca DB.".green());
    println!(
        "{}",
        "Use /escalate or /model claude-* in sessions.".dimmed()
    );
    Ok(())
}

pub fn cmd_logout() -> Result<()> {
    let conn = db::open_default()?;
    let removed = db::settings::secret_delete(&conn, KEY_NAME)?;
    if removed {
        println!("{}", "API key removed from orca DB.".green());
    } else {
        println!("{}", "No API key was stored.".dimmed());
    }
    Ok(())
}

pub fn cmd_auth(config: &Config) -> Result<()> {
    match &config.anthropic_api_key {
        Some(key) => {
            println!(
                "{} Anthropic API key: {}",
                "✓".green(),
                db::settings::mask_key(key).dimmed()
            );
        }
        None => {
            println!("{} Anthropic API key: not set", "✗".red());
            println!(
                "{}",
                format!("  run `{APP_NAME} login` to store one for Claude escalation").dimmed()
            );
        }
    }

    let lms_url = &config.lmstudio_url;
    let lms_status = std::process::Command::new("curl")
        .args(["-sf", &format!("{lms_url}/v1/models"), "-o", "/dev/null"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if lms_status {
        println!("{} LM Studio: reachable at {lms_url}", "✓".green());
    } else {
        println!("{} LM Studio: not reachable at {lms_url}", "✗".yellow());
        println!(
            "{}",
            "  enable Local Server in LM Studio → Developer tab".dimmed()
        );
    }

    Ok(())
}

pub fn rpassword_or_stdin() -> Result<String> {
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut input = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!();
    Ok(input)
}
