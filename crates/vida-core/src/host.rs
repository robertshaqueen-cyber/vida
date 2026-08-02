use crate::vault::{AuthMethod, HostEntry};

/// A host profile with operational notes (Markdown).
#[derive(Debug, Clone)]
pub struct HostProfile {
    pub entry: HostEntry,
    pub notes: String,
}

impl HostProfile {
    pub fn new(entry: HostEntry) -> Self {
        Self {
            entry,
            notes: String::new(),
        }
    }

    /// Render the notes as a full Markdown document.
    pub fn render_markdown(&self) -> String {
        let mut doc = format!("# {}\n\n", self.entry.name);
        doc.push_str("- 用途：\n");
        doc.push_str("- 系统：\n");
        let auth_str = match &self.entry.auth {
            AuthMethod::Password { .. } => "密码".to_owned(),
            AuthMethod::Key { private_key_path, .. } => format!("密钥文件: {}", private_key_path),
            AuthMethod::KeyInline { .. } => "内嵌密钥".to_owned(),
        };
        doc.push_str(&format!("- 登录方式：{}\n\n", auth_str));
        if !self.notes.is_empty() {
            doc.push_str(&self.notes);
        } else {
            doc.push_str("## 已安装\n\n## 端口 / 防火墙\n\n## 注意事项 / 踩过的坑\n\n## 变更记录\n");
        }
        doc
    }
}
