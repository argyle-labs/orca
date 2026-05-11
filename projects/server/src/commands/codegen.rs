use anyhow::Result;
use colored::Colorize;
use orca_utils::config::APP_NAME;

pub async fn cmd_gen(url: &str, out: &str) -> Result<()> {
    // Poll until the backend is reachable (up to 30s after a cargo-watch restart)
    let spec_url = format!("{url}/api/openapi.json");
    let client = orca_utils::http::Client::new();
    let mut attempts = 0;
    loop {
        match client.get(&spec_url).send().await {
            Ok(_) => break,
            Err(_) => {
                attempts += 1;
                if attempts >= 30 {
                    anyhow::bail!("backend not reachable at {spec_url} after 30s");
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }

    // Run the TypeScript generator in frontend/
    let repo_root = std::env::current_exe()?
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();

    // Fall back to cwd-relative projects/frontend/ if the exe path heuristic fails
    let site_dir = if repo_root.join("projects/frontend/scripts/gen.ts").exists() {
        repo_root.join("projects/frontend")
    } else {
        std::env::current_dir()?.join("projects/frontend")
    };

    println!(
        "{} generating types and hooks from {spec_url}",
        format!("{APP_NAME} gen").cyan()
    );

    let status = std::process::Command::new("npx")
        .args(["tsx", "scripts/gen.ts", "--url", url, "--out", out])
        .current_dir(&site_dir)
        .status()?;

    if status.success() {
        println!("{} {}/{}", "✓".green(), site_dir.display(), out);
    } else {
        anyhow::bail!("generator failed — check frontend/scripts/gen.ts");
    }
    Ok(())
}
