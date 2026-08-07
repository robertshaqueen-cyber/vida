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
        .ok_or_else(|| {
            anyhow::anyhow!("无法确定配置目录路径。请设置环境变量 VIDA_CONFIG_DIR 指定配置目录。")
        })
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

/// Ensures the config directory structure exists（首次启动必须调用）。
/// 错误信息说人话：发生了什么 + 原因 + 建议下一步。
pub fn ensure_dirs() -> Result<()> {
    let base = config_dir()?;
    // 注意：不用 with_context —— 它会保留 cause 链里的 "os error N"。
    // 用户只需要知道「发生了什么 + 建议」，原始 errno 无助于普通用户。
    if let Err(e) = std::fs::create_dir_all(&base) {
        return Err(anyhow::anyhow!(
            "无法创建配置目录 {}：{}。请检查权限（当前用户需对该路径有写权限）",
            base.display(),
            friendly_io_error(&e)
        ));
    }
    let backups = backup_dir()?;
    if let Err(e) = std::fs::create_dir_all(&backups) {
        return Err(anyhow::anyhow!(
            "无法创建备份目录 {}：{}。请检查权限（当前用户需对该路径有写权限）",
            backups.display(),
            friendly_io_error(&e)
        ));
    }
    Ok(())
}

/// 把 io 错误转成一句话的人话（不含 "os error N" 字样）。
fn friendly_io_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "权限不足".to_string(),
        std::io::ErrorKind::NotFound => "路径不存在".to_string(),
        std::io::ErrorKind::AlreadyExists => "路径已存在".to_string(),
        std::io::ErrorKind::ReadOnlyFilesystem => "文件系统只读".to_string(),
        _ => "写入失败".to_string(),
    }
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
    table
        .get("language")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Save the language choice to config file.
/// Valid choices: "system" | "zh-CN" | "en".
pub fn save_language_choice(choice: &str) -> Result<()> {
    if !matches!(choice, "system" | "zh-CN" | "en") {
        anyhow::bail!("无效的语言选择: {}", choice);
    }
    let dir = config_dir()?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(anyhow::anyhow!(
            "无法创建配置目录 {}：{}。请检查权限（当前用户需对该路径有写权限）",
            dir.display(),
            friendly_io_error(&e)
        ));
    }
    let content = format!("language = {:?}\n", choice);
    let tmp = dir.join("language.toml.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, language_path()?)?;
    Ok(())
}
