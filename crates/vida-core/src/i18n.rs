//! 轻量多语言支持：内嵌 TOML 语言文件，运行时按 key 查表。
//!
//! 新增语言流程：复制 `i18n/en.toml` 为 `<lang>.toml`，翻译 value，
//! 在 `Lang` 枚举加变体并在 `strings_for` 中注册即可，无需改调用方。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 支持的语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    ZhCn,
    En,
}

impl Lang {
    /// 语言代码，用于配置文件持久化。
    pub fn code(self) -> &'static str {
        match self {
            Lang::ZhCn => "zh-CN",
            Lang::En => "en",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        match code {
            "zh-CN" => Some(Lang::ZhCn),
            "en" => Some(Lang::En),
            _ => None,
        }
    }
}

static ZH_CN: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| parse(include_str!("i18n/zh-CN.toml")));
static EN: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| parse(include_str!("i18n/en.toml")));

fn parse(raw: &str) -> HashMap<String, String> {
    toml::from_str(raw).unwrap_or_default()
}

/// 翻译器。启动时创建一次，持有语言选择。
#[derive(Debug, Clone)]
pub struct I18n {
    lang: Lang,
}

impl I18n {
    pub fn new(lang: Lang) -> Self {
        Self { lang }
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// 取无参数文本。key 不存在时返回 key 本身，便于调试遗漏。
    /// 注意：key 必须是字符串字面量（'static）。
    pub fn tr(&self, key: &'static str) -> &'static str {
        let table = match self.lang {
            Lang::ZhCn => &*ZH_CN,
            Lang::En => &*EN,
        };
        table.get(key).map(|s| s.as_str()).unwrap_or(key)
    }

    /// 取无参数文本，支持动态 key。key 不存在时返回 key 本身。
    pub fn tr_dyn(&self, key: &str) -> String {
        let table = match self.lang {
            Lang::ZhCn => &*ZH_CN,
            Lang::En => &*EN,
        };
        table.get(key).cloned().unwrap_or_else(|| key.to_string())
    }

    /// 取带参数文本，用 `{0}` `{1}` 占位。key 不存在时原样返回 key。
    pub fn trf(&self, key: &'static str, args: &[&str]) -> String {
        let template = self.tr(key);
        let mut out = template.to_string();
        for (i, arg) in args.iter().enumerate() {
            out = out.replace(&format!("{{{}}}", i), arg);
        }
        out
    }
}

impl Default for I18n {
    fn default() -> Self {
        Self::new(Lang::ZhCn)
    }
}

/// 检测语言：配置显式选择 > 系统语言 > 中文兜底。
pub fn detect_lang() -> Lang {
    match crate::config::load_language_choice() {
        Some(choice) if choice == "system" => system_lang(),
        Some(choice) => Lang::from_code(&choice).unwrap_or_else(system_lang),
        None => system_lang(),
    }
}

/// 读系统语言：zh* → 中文，其他 → 英文。
fn system_lang() -> Lang {
    match sys_locale::get_locale() {
        Some(loc) if loc.to_ascii_lowercase().starts_with("zh") => Lang::ZhCn,
        _ => Lang::En,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_returns_chinese() {
        let i18n = I18n::new(Lang::ZhCn);
        assert_eq!(i18n.tr("common_save"), "保存");
    }

    #[test]
    fn en_returns_english() {
        let i18n = I18n::new(Lang::En);
        assert_eq!(i18n.tr("common_save"), "Save");
    }

    #[test]
    fn missing_key_returns_key() {
        let i18n = I18n::new(Lang::En);
        assert_eq!(i18n.tr("no_such_key"), "no_such_key");
    }

    #[test]
    fn trf_substitutes_positional_args() {
        let i18n = I18n::new(Lang::En);
        assert_eq!(
            i18n.trf("daemon_host_not_found_id", &["web01"]),
            "Host not found: web01"
        );
        let zh = I18n::new(Lang::ZhCn);
        assert_eq!(
            zh.trf("backup_saved", &["/Users/test/vida.age", "123"]),
            "备份已保存到 /Users/test/vida.age（123 字节）"
        );
    }

    #[test]
    fn lang_code_roundtrip() {
        for lang in [Lang::ZhCn, Lang::En] {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Lang::from_code("fr"), None);
    }

    #[test]
    fn both_langs_have_same_keys() {
        for (key, zh) in ZH_CN.iter() {
            assert!(EN.contains_key(key), "en.toml missing key: {}", key);
            assert_eq!(zh, &ZH_CN[key]);
        }
        for key in EN.keys() {
            assert!(ZH_CN.contains_key(key), "zh-CN.toml missing key: {}", key);
        }
    }
}
