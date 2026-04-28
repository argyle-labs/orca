use anyhow::Result;
use brain_utils::auth;
use brain_core::backend; // NOTE: requires brain crate
use brain_utils::config::Config;
use brain_utils::types::Message;
// NOTE: requires brain crate
use colored::Colorize;

pub fn cmd_login(config: &Config) -> Result<()> {
    if let Some(key) = &config.anthropic_api_key {
        println!(
            "{} API key already set: {}",
            "✓".green(),
            auth::mask_key(key).dimmed()
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

    if !key.starts_with("sk-ant-") {
        eprintln!(
            "{}",
            "key doesn't look right (expected sk-ant-…) — saving anyway".yellow()
        );
    }

    auth::store_api_key(&key)?;
    println!("{}", "API key stored in macOS Keychain.".green());
    println!(
        "{}",
        "Use /escalate or /model claude-* in sessions.".dimmed()
    );
    Ok(())
}

pub fn cmd_logout() -> Result<()> {
    auth::remove_api_key();
    println!("{}", "API key removed from keychain.".green());
    Ok(())
}

pub fn cmd_auth(config: &Config) -> Result<()> {
    match &config.anthropic_api_key {
        Some(key) => {
            println!(
                "{} Anthropic API key: {}",
                "✓".green(),
                auth::mask_key(key).dimmed()
            );
        }
        None => {
            println!("{} Anthropic API key: not set", "✗".red());
            println!(
                "{}",
                "  run `brain login` to store one for Claude escalation".dimmed()
            );
        }
    }

    // LM Studio connectivity
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

pub async fn cmd_escalate(config: &Config, question: &str, project: Option<&str>) -> Result<()> {
    use backend::ClaudeBackend;
    use backend::ModelBackend;
    let api_key = config
        .anthropic_api_key
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no API key — run `brain login`"))?;

    let system = if let Some(_p) = project {
        // TODO: crate::context stays in server for now
        String::new()
    } else {
        String::new()
    };

    let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
    let messages = vec![Message::user(question)];
    let cancel = tokio_util::sync::CancellationToken::new();
    let output = backend::stdout_sink();
    claude
        .chat(&messages, &[], &system, cancel, &output)
        .await?;
    Ok(())
}

pub fn rpassword_or_stdin() -> Result<String> {
    // Try to read without echo using stty
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut input = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!(); // newline after hidden input
    Ok(input)
}
