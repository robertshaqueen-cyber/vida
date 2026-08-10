use crate::vault::{AuthMethod, HostEntry};

pub const DEFAULT_NOTES_MARKDOWN: &str =
    "## Installed software\n\n## Ports and firewall\n\n## Warnings and pitfalls\n\n## Change log\n";

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
            AuthMethod::Key {
                private_key_path, ..
            } => format!("密钥文件: {}", private_key_path),
            AuthMethod::KeyInline { .. } => "内嵌密钥".to_owned(),
        };
        doc.push_str(&format!("- 登录方式：{}\n\n", auth_str));
        if !self.notes.is_empty() {
            doc.push_str(&self.notes);
        } else {
            doc.push_str(
                "## 已安装\n\n## 端口 / 防火墙\n\n## 注意事项 / 踩过的坑\n\n## 变更记录\n",
            );
        }
        doc
    }
}

/// Update one level-two Markdown section without disturbing the other sections.
/// Section titles are matched exactly after trimming. When a section does not
/// exist it is appended at the end of the document.
pub fn update_notes_section(notes: &str, section: &str, text: &str, replace: bool) -> String {
    let heading = format!("## {}", section.trim());
    let mut lines = notes.lines().map(str::to_owned).collect::<Vec<_>>();
    let start = lines.iter().position(|line| line.trim() == heading);

    if let Some(start) = start {
        let end = lines[start + 1..]
            .iter()
            .position(|line| line.trim_start().starts_with("## "))
            .map(|offset| start + 1 + offset)
            .unwrap_or(lines.len());
        let existing = lines[start + 1..end].join("\n").trim().to_string();
        let body = if replace || existing.is_empty() {
            text.trim().to_string()
        } else {
            format!("{}\n\n{}", existing, text.trim())
        };
        let replacement = if body.is_empty() {
            Vec::new()
        } else {
            body.lines().map(str::to_owned).collect()
        };
        lines.splice(start + 1..end, replacement);
    } else {
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(heading);
        if !text.trim().is_empty() {
            lines.extend(text.trim().lines().map(str::to_owned));
        }
    }

    let mut output = lines.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::update_notes_section;

    #[test]
    fn section_updates_preserve_unrelated_markdown() {
        let notes = "intro\n\n## Installed\nnginx\n\n## Alerts\nnone\n";
        let appended = update_notes_section(notes, "Installed", "docker", false);
        assert_eq!(
            appended,
            "intro\n\n## Installed\nnginx\n\ndocker\n## Alerts\nnone\n"
        );
        let replaced = update_notes_section(&appended, "Alerts", "disk 90%", true);
        assert!(replaced.contains("## Installed\nnginx\n\ndocker"));
        assert!(replaced.contains("## Alerts\ndisk 90%"));
        assert!(!replaced.contains("none"));
    }

    #[test]
    fn missing_section_is_appended() {
        assert_eq!(
            update_notes_section("summary\n", "Changes", "installed htop", false),
            "summary\n\n## Changes\ninstalled htop\n"
        );
    }
}
