use anyhow::Result;

/// Load API key from macOS Keychain (service: "brain", account: "anthropic_api_key").
pub fn load_api_key_from_keychain() -> Option<String> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", "brain", "-a", "anthropic_api_key", "-w"])
        .output()
        .ok()?;
    if output.status.success() {
        let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !key.is_empty() { Some(key) } else { None }
    } else {
        None
    }
}

pub fn store_api_key(key: &str) -> Result<()> {
    // Remove existing entry first (ignore error if absent)
    let _ = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", "brain", "-a", "anthropic_api_key"])
        .status();

    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-s", "brain",
            "-a", "anthropic_api_key",
            "-w", key,
        ])
        .status()?;

    if !status.success() {
        anyhow::bail!("failed to store key in macOS Keychain");
    }
    Ok(())
}

pub fn remove_api_key() {
    let _ = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", "brain", "-a", "anthropic_api_key"])
        .status();
}

pub fn mask_key(key: &str) -> String {
    if key.len() > 12 {
        format!("{}…{}", &key[..8], &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}
