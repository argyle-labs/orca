use anyhow::Result;

/// Load API key from macOS Keychain (service: "brain", account: "anthropic_api_key").
pub fn load_api_key_from_keychain() -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "brain",
            "-a",
            "anthropic_api_key",
            "-w",
        ])
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
        .args([
            "delete-generic-password",
            "-s",
            "brain",
            "-a",
            "anthropic_api_key",
        ])
        .status();

    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            "brain",
            "-a",
            "anthropic_api_key",
            "-w",
            key,
        ])
        .status()?;

    if !status.success() {
        anyhow::bail!("failed to store key in macOS Keychain");
    }
    Ok(())
}

pub fn remove_api_key() {
    let _ = std::process::Command::new("security")
        .args([
            "delete-generic-password",
            "-s",
            "brain",
            "-a",
            "anthropic_api_key",
        ])
        .status();
}

pub fn mask_key(key: &str) -> String {
    if key.len() > 12 {
        format!("{}…{}", &key[..8], &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_key_long_key_shows_first_and_last() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let masked = mask_key(key);
        assert!(masked.starts_with("sk-ant-a"), "prefix wrong: {masked}");
        assert!(masked.ends_with("wxyz"), "suffix wrong: {masked}");
        assert!(masked.contains('…'), "no ellipsis: {masked}");
    }

    #[test]
    fn mask_key_short_returns_stars() {
        assert_eq!(mask_key("short"), "****");
    }

    #[test]
    fn mask_key_exactly_12_returns_stars() {
        assert_eq!(mask_key("abcdefghijkl"), "****");
    }

    #[test]
    fn mask_key_13_chars_masks() {
        let key = "abcdefghijklm"; // 13 chars
        let masked = mask_key(key);
        assert!(masked.starts_with("abcdefgh"), "got: {masked}");
        assert!(masked.ends_with("jklm"), "got: {masked}");
    }

    #[test]
    fn mask_key_empty_returns_stars() {
        assert_eq!(mask_key(""), "****");
    }
}
