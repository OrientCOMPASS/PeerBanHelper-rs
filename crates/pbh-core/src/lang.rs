//! 多语言存储：直接加载上游 `lang/*/messages.yml`（键为 Lang 枚举名，值为带 `{}` 占位的模板）。
use crate::translate::{TcParam, TranslationComponent};
use std::collections::HashMap;

type Messages = HashMap<String, String>;

pub struct LangStore {
    locales: HashMap<String, Messages>,
    fallback: Messages,
    /// 服务端默认 locale（对应 Java 侧 Main.DEF_LOCALE），用于规则引擎构造期文案
    pub default_locale: String,
}

fn parse(text: &str) -> Messages {
    serde_yaml::from_str::<Messages>(text).unwrap_or_default()
}

impl LangStore {
    pub fn embedded(default_locale: impl Into<String>) -> Self {
        let mut locales = HashMap::new();
        locales.insert("zh_cn".to_string(), parse(include_str!("langdata/zh_cn.yml")));
        locales.insert("zh_tw".to_string(), parse(include_str!("langdata/zh_tw.yml")));
        locales.insert("en_us".to_string(), parse(include_str!("langdata/en_us.yml")));
        Self {
            locales,
            fallback: parse(include_str!("langdata/fallback.yml")),
            default_locale: default_locale.into(),
        }
    }

    /// 将 Accept-Language / 配置语言归一化为 pbh 支持的 locale 键
    pub fn normalize_locale(input: Option<&str>) -> String {
        let Some(input) = input else {
            return "en_us".to_string();
        };
        let lower = input.to_lowercase();
        // 取第一个候选
        let first = lower.split(',').next().unwrap_or("").trim();
        if first.starts_with("zh") {
            if first.contains("tw") || first.contains("hk") || first.contains("hant") || first.contains("mo")
            {
                return "zh_tw".to_string();
            }
            return "zh_cn".to_string();
        }
        "en_us".to_string()
    }

    fn template(&self, locale: &str, key: &str) -> Option<&str> {
        if let Some(map) = self.locales.get(locale) {
            if let Some(v) = map.get(key) {
                return Some(v.as_str());
            }
        }
        self.fallback.get(key).map(|v| v.as_str())
    }

    /// 翻译一个键并按顺序替换 `{}` 占位符（对应上游 TextManager.tl 行为）
    pub fn tl(&self, locale: &str, key: &str, params: &[TcParam]) -> String {
        let Some(tpl) = self.template(locale, key) else {
            // 上游找不到键时返回键本身
            return key.to_string();
        };
        let mut out = String::with_capacity(tpl.len() + 16);
        let mut rest = tpl;
        let mut idx = 0usize;
        while let Some(pos) = rest.find("{}") {
            out.push_str(&rest[..pos]);
            if idx < params.len() {
                match &params[idx] {
                    TcParam::S(s) => out.push_str(s),
                    TcParam::Tc(tc) => out.push_str(&self.tlc(locale, tc)),
                }
                idx += 1;
            } else {
                out.push_str("{}");
            }
            rest = &rest[pos + 2..];
        }
        out.push_str(rest);
        out
    }

    pub fn tlc(&self, locale: &str, tc: &TranslationComponent) -> String {
        self.tl(locale, &tc.key, &tc.params)
    }

    pub fn tl_default(&self, key: &str, params: &[TcParam]) -> String {
        let locale = self.default_locale.clone();
        self.tl(&locale, key, params)
    }
}
