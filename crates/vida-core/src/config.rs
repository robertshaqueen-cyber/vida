use anyhow::{Context, Result};
use std::path::PathBuf;

/// Returns the config directory.
///
/// Priority:
/// 1. VIDA_CONFIG_DIR environment variable (for testing / custom location)
/// 2. Platform standard location:
///    - macOS: ~/Library/Application Support/vida
///    - Linux: ~/.config/vida
///    - Windows: {FOLDERID_RoamingAppData}/vida
///
/// Returns an error if the standard config directory cannot be determined.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("VIDA_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!(
            "无法确定配置目录路径。请设置环境变量 VIDA_CONFIG_DIR 指定配置目录。"
        ))
        .map(|d| d.join("vida"))
}

/// Returns the vault file path.
pub fn vault_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("vault.age"))
}

/// Returns the local backup directory.
pub fn backup_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("backups"))
}

/// Ensures the config directory structure exists.
pub fn ensure_dirs() -> Result<()> {
    let base = config_dir()?;
    std::fs::create_dir_all(&base)
        .with_context(|| format!("Failed to create config directory: {}", base.display()))?;
    let backups = backup_dir()?;
    std::fs::create_dir_all(&backups)
        .with_context(|| format!("Failed to create backup directory: {}", backups.display()))?;
    Ok(())
}

/// Returns the language config file path.
pub fn language_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("language.toml"))
}

/// Load the language choice from config file.
///
/// Returns `None` if the file does not exist (use system language).
/// Valid values: "system" | "zh-CN" | "en".
pub fn load_language_choice() -> Option<String> {
    let path = language_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = toml::from_str(&content).ok()?;
    table.get("language").and_then(|v| v.as_str()).map(String::from)
}

/// Save the language choice to config file.
/// Valid choices: "system" | "zh-CN" | "en".
pub fn save_language_choice(choice: &str) -> Result<()> {
    if !matches!(choice, "system" | "zh-CN" | "en") {
        anyhow::bail!("无效的语言选择: {}", choice);
    }
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let content = format!("language = {:?}\n", choice);
    let tmp = dir.join("language.toml.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, language_path()?)?;
    Ok(())
}
