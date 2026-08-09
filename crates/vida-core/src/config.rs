use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Non-secret, device-local GUI state. This deliberately lives outside the
/// encrypted vault: terminal session IDs and recent host IDs describe this
/// computer's workspace and must not be synced to other devices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub recent_host_ids: Vec<String>,
    pub terminal_layout: Vec<TerminalLayoutEntry>,
    pub active_terminal_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLayoutEntry {
    pub session_id: String,
    pub host_id: Option<String>,
    pub number: u32,
}

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

/// Returns the device-local GUI state path.
pub fn ui_state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("ui-state.toml"))
}

/// Load device-local GUI state. Missing or malformed state is treated as a
/// clean first launch so a damaged convenience file can never block unlock.
pub fn load_ui_state() -> UiState {
    let Ok(path) = ui_state_path() else {
        return UiState::default();
    };
    load_ui_state_from(&path)
}

fn load_ui_state_from(path: &std::path::Path) -> UiState {
    let Ok(content) = std::fs::read_to_string(path) else {
        return UiState::default();
    };
    toml::from_str(&content).unwrap_or_default()
}

/// Atomically save device-local GUI state.
pub fn save_ui_state(state: &UiState) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    save_ui_state_to(state, &ui_state_path()?)
}

fn save_ui_state_to(state: &UiState, path: &std::path::Path) -> Result<()> {
    let content = toml::to_string_pretty(state)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{TerminalLayoutEntry, UiState, load_ui_state_from, save_ui_state_to};

    #[test]
    fn ui_state_round_trip_preserves_recent_hosts_and_terminal_layout() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ui-state.toml");
        let state = UiState {
            recent_host_ids: vec!["host-b".into(), "host-a".into()],
            terminal_layout: vec![
                TerminalLayoutEntry {
                    session_id: "local-session".into(),
                    host_id: None,
                    number: 1,
                },
                TerminalLayoutEntry {
                    session_id: "ssh-session".into(),
                    host_id: Some("host-b".into()),
                    number: 2,
                },
            ],
            active_terminal_session_id: Some("ssh-session".into()),
        };

        save_ui_state_to(&state, &path).unwrap();
        assert_eq!(load_ui_state_from(&path), state);
    }
}
