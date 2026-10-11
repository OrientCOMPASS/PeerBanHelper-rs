//! 规则引擎：上游 util/rule 包（RuleParser + matcher 族）的行为等价实现。
//!
//! 兼容性说明（刻意保留的上游历史行为）：
//! - STARTS_WITH/ENDS_WITH/CONTAINS/EQUALS 忽略大小写；REGEX 大小写敏感且为整串匹配；
//! - 显式 hit/miss 覆盖时，注释参数带 "Hit-" 前缀（上游 startswith 的 miss 覆盖同样使用
//!   "Hit-" 前缀，此处按原样复刻）；LENGTH 的默认 miss 注释也是 "Hit-Min-..."，
//!   显式 miss 覆盖时才是 "Miss-Min-..."；
//! - matchRule：任一规则返回 FALSE 立即终止（不命中）；TRUE 记录为命中但可被后续 FALSE 否决；
//!   DEFAULT 继续。
use crate::lang::LangStore;
use crate::translate::TranslationComponent;
use fancy_regex::Regex;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchResultEnum {
    Default,
    True,
    False,
}

impl MatchResultEnum {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "DEFAULT" => Some(Self::Default),
            "TRUE" => Some(Self::True),
            "FALSE" => Some(Self::False),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MatchResult {
    pub result: MatchResultEnum,
    pub comment: Option<TranslationComponent>,
}

impl MatchResult {
    pub fn new(result: MatchResultEnum, comment: Option<TranslationComponent>) -> Self {
        Self { result, comment }
    }
}

#[derive(Clone, Debug)]
pub struct RuleMatchResult {
    pub hit: bool,
    pub rule: Option<Rule>,
    pub comment: Option<TranslationComponent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatcherKind {
    StartsWith,
    EndsWith,
    Contains,
    Equals,
    Regex,
    Length,
}

impl MatcherKind {
    fn lang_key(self) -> &'static str {
        match self {
            MatcherKind::StartsWith => "MATCH_STRING_STARTS_WITH",
            MatcherKind::EndsWith => "MATCH_STRING_ENDS_WITH",
            MatcherKind::Contains => "MATCH_STRING_CONTAINS",
            MatcherKind::Equals => "MATCH_STRING_EQUALS",
            MatcherKind::Regex => "MATCH_STRING_REGEX",
            MatcherKind::Length => "MATCH_STRING_LENGTH",
        }
    }

    fn identifier(self) -> &'static str {
        match self {
            MatcherKind::StartsWith => "peerbanhelper:stringstartswithmatcher",
            MatcherKind::EndsWith => "peerbanhelper:stringendswithmatcher",
            MatcherKind::Contains => "peerbanhelper:stringcontainsmatcher",
            MatcherKind::Equals => "peerbanhelper:stringequalsmatcher",
            MatcherKind::Regex => "peerbanhelper:stringregexmatcher",
            MatcherKind::Length => "peerbanhelper:stringlengthmatcher",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Rule {
    /// 虚拟规则：null / JsonNull → 恒 TRUE 无注释
    DumbNull,
    DumbBool(bool),
    DumbNum(i64),
    DumbBoolStr(bool),
    Matcher(Box<Matcher>),
}

#[derive(Clone, Debug)]
pub struct Matcher {
    pub kind: MatcherKind,
    /// 字符串类匹配的内容（已按上游规则小写化；Regex 保留原样）
    pub content: String,
    /// 面向用户展示的原始内容
    pub display: String,
    pub min: i64,
    pub max: i64,
    pub cond: Option<Box<Rule>>,
    pub hit: MatchResult,
    pub miss: MatchResult,
    pub regex: Option<Regex>,
}

impl Rule {
    pub fn match_content(&self, content: Option<&str>) -> MatchResult {
        match self {
            Rule::DumbNull => MatchResult::new(MatchResultEnum::True, None),
            Rule::DumbBool(b) => MatchResult::new(
                if *b {
                    MatchResultEnum::True
                } else {
                    MatchResultEnum::False
                },
                Some(TranslationComponent::new("MATCH_CONDITION_BOOLEAN")),
            ),
            Rule::DumbNum(n) => MatchResult::new(
                if *n != 0 {
                    MatchResultEnum::True
                } else {
                    MatchResultEnum::False
                },
                Some(TranslationComponent::new("MATCH_CONDITION_BOOLEAN_BY_INTEGER")),
            ),
            Rule::DumbBoolStr(b) => MatchResult::new(
                if *b {
                    MatchResultEnum::True
                } else {
                    MatchResultEnum::False
                },
                Some(TranslationComponent::new("MATCH_CONDITION_BOOLEAN_BY_STRING")),
            ),
            Rule::Matcher(m) => m.match_content(content),
        }
    }

    pub fn metadata(&self) -> String {
        match self {
            Rule::Matcher(m) => m.metadata(),
            _ => String::new(),
        }
    }

    pub fn matcher_name(&self) -> TranslationComponent {
        match self {
            Rule::Matcher(m) => m.matcher_name(),
            _ => TranslationComponent::new("N/A"),
        }
    }

    pub fn matcher_identifier(&self) -> &'static str {
        match self {
            Rule::Matcher(m) => m.kind.identifier(),
            Rule::DumbNull => "dumb:null",
            Rule::DumbBool(_) => "dumb:boolean",
            Rule::DumbNum(_) => "dumb:number",
            Rule::DumbBoolStr(_) => "dumb:boolstring",
        }
    }

    /// Rule.toPrintableText：{matcherName(已翻译), matcherIdentifier, metadata} 的 JSON 串
    pub fn to_printable_text(&self, lang: &LangStore, locale: &str) -> String {
        let mut info = serde_json::Map::new();
        info.insert(
            "matcherName".into(),
            Value::String(lang.tlc(locale, &self.matcher_name())),
        );
        info.insert(
            "matcherIdentifier".into(),
            Value::String(self.matcher_identifier().to_string()),
        );
        info.insert("metadata".into(), Value::String(self.metadata()));
        Value::Object(info).to_string()
    }
}

impl Matcher {
    fn match_content(&self, content: Option<&str>) -> MatchResult {
        let content_owned;
        let content: &str = match content {
            Some(c) => c,
            None => {
                content_owned = String::new();
                &content_owned
            }
        };
        if let Some(cond) = &self.cond {
            if cond.match_content(Some(content)).result == MatchResultEnum::False {
                // 上游：JSON_MATCHER_NOT_MET(条件printable, 自身printable)，使用服务端默认 locale
                let locale = &lang_locale();
                let lang = lang_store();
                let cond_text = cond.to_printable_text(lang, locale);
                let self_text = Rule::Matcher(Box::new(self.clone())).to_printable_text(lang, locale);
                return MatchResult::new(
                    MatchResultEnum::False,
                    Some(TranslationComponent::strs(
                        "JSON_MATCHER_NOT_MET",
                        vec![cond_text, self_text],
                    )),
                );
            }
        }
        self.match0(content)
    }

    fn match0(&self, content: &str) -> MatchResult {
        let lower = content.to_lowercase();
        let matched = match self.kind {
            MatcherKind::StartsWith => lower.starts_with(&self.content),
            MatcherKind::EndsWith => lower.ends_with(&self.content),
            MatcherKind::Contains => lower.contains(&self.content),
            MatcherKind::Equals => lower == self.content,
            MatcherKind::Regex => self
                .regex
                .as_ref()
                .map(|r| r.is_match(content).unwrap_or(false))
                .unwrap_or(false),
            MatcherKind::Length => {
                let len = content.chars().count() as i64;
                len >= self.min && len <= self.max
            }
        };
        if matched {
            self.hit.clone()
        } else {
            self.miss.clone()
        }
    }

    fn metadata(&self) -> String {
        match self.kind {
            MatcherKind::Length => format!("min: {}, max: {}", self.min, self.max),
            _ => self.display.clone(),
        }
    }

    fn matcher_name(&self) -> TranslationComponent {
        match self.kind {
            MatcherKind::Length => TranslationComponent::strs(
                "MATCH_STRING_LENGTH",
                vec![format!("Min-{}, Max-{}", self.min, self.max)],
            ),
            _ => TranslationComponent::strs(self.kind.lang_key(), vec![self.display.clone()]),
        }
    }
}

// ---- 全局 LangStore 引用（供规则构造期生成注释；由宿主进程初始化一次） ----
static LANG: std::sync::OnceLock<LangStore> = std::sync::OnceLock::new();

pub fn init_lang(store: LangStore) {
    let _ = LANG.set(store);
}

pub fn lang_store() -> &'static LangStore {
    LANG.get_or_init(|| LangStore::embedded("en_us"))
}

pub fn lang_locale() -> String {
    lang_store().default_locale.clone()
}

/// RuleParser.matchRule 语义
pub fn match_rule(rules: &[Rule], content: Option<&str>) -> RuleMatchResult {
    let mut result = RuleMatchResult {
        hit: false,
        rule: None,
        comment: None,
    };
    for rule in rules {
        let m = rule.match_content(content);
        match m.result {
            MatchResultEnum::False => {
                return RuleMatchResult {
                    hit: false,
                    rule: Some(rule.clone()),
                    comment: m.comment,
                };
            }
            MatchResultEnum::True => {
                result = RuleMatchResult {
                    hit: true,
                    rule: Some(rule.clone()),
                    comment: m.comment,
                };
            }
            MatchResultEnum::Default => {}
        }
    }
    result
}

/// 解析 profile 中的规则字符串列表（每项为 JSON）
pub fn parse_rules(items: &[String]) -> Vec<Rule> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match parse_rule_str(item) {
            Ok(rule) => out.push(rule),
            Err(e) => tracing::warn!("Invalid rule syntax: {} ({})", item, e),
        }
    }
    out
}

pub fn parse_rule_str(s: &str) -> Result<Rule, String> {
    let v: Value = serde_json::from_str(s).map_err(|e| e.to_string())?;
    parse_rule_value(&v)
}

pub fn parse_rule_value(v: &Value) -> Result<Rule, String> {
    match v {
        Value::Null => Ok(Rule::DumbNull),
        Value::Bool(b) => Ok(Rule::DumbBool(*b)),
        Value::Number(n) => Ok(Rule::DumbNum(n.as_i64().unwrap_or(0))),
        Value::String(s) => {
            // Java Boolean.parseBoolean：仅 "true"（忽略大小写）为真
            Ok(Rule::DumbBoolStr(s.eq_ignore_ascii_case("true")))
        }
        Value::Object(obj) => {
            let method = obj
                .get("method")
                .and_then(|m| m.as_str())
                .ok_or_else(|| "missing method".to_string())?;
            let kind = match method {
                "STARTS_WITH" => MatcherKind::StartsWith,
                "ENDS_WITH" => MatcherKind::EndsWith,
                "CONTAINS" => MatcherKind::Contains,
                "EQUALS" => MatcherKind::Equals,
                "REGEX" => MatcherKind::Regex,
                "LENGTH" => MatcherKind::Length,
                other => return Err(format!("Unexpected method value: {}", other)),
            };
            let cond = match obj.get("if") {
                Some(c) => Some(Box::new(parse_rule_value(c)?)),
                None => None,
            };

            if kind == MatcherKind::Length {
                let min = obj.get("min").and_then(|v| v.as_i64()).unwrap_or(0);
                let max = obj
                    .get("max")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(i32::MAX as i64);
                let hit = MatchResult::new(
                    MatchResultEnum::True,
                    Some(TranslationComponent::strs(
                        "MATCH_STRING_LENGTH",
                        vec![format!("Hit-Min-{}, Max-{}", min, max)],
                    )),
                );
                let miss = MatchResult::new(
                    MatchResultEnum::Default,
                    Some(TranslationComponent::strs(
                        "MATCH_STRING_LENGTH",
                        vec![format!("Hit-Min-{}, Max-{}", min, max)],
                    )),
                );
                let hit = override_hit(obj, hit, "MATCH_STRING_LENGTH", &format!("Hit-Min-{}, Max-{}", min, max));
                let miss = override_miss(obj, miss, "MATCH_STRING_LENGTH", &format!("Miss-Min-{}, Max-{}", min, max));
                return Ok(Rule::Matcher(Box::new(Matcher {
                    kind,
                    content: String::new(),
                    display: String::new(),
                    min,
                    max,
                    cond,
                    hit,
                    miss,
                    regex: None,
                })));
            }

            let raw = obj
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let display = raw.clone();
            let content = if kind == MatcherKind::Regex {
                raw.clone()
            } else {
                raw.to_lowercase()
            };
            let name_key = kind.lang_key();
            let hit = MatchResult::new(
                MatchResultEnum::True,
                Some(TranslationComponent::strs(name_key, vec![display.clone()])),
            );
            let miss = MatchResult::new(
                MatchResultEnum::Default,
                Some(TranslationComponent::strs(name_key, vec![display.clone()])),
            );
            // 上游：hit/miss 显式覆盖时注释使用 "Hit-" 前缀（miss 亦然，原样复刻）
            let hit = override_hit(obj, hit, name_key, &format!("Hit-{}", display));
            let miss = override_miss(obj, miss, name_key, &format!("Hit-{}", display));
            let regex = if kind == MatcherKind::Regex {
                // Java matches() 为整串匹配；此处显式加锚
                Some(
                    Regex::new(&format!("\\A(?:{})\\z", raw))
                        .map_err(|e| format!("invalid regex: {}", e))?,
                )
            } else {
                None
            };
            Ok(Rule::Matcher(Box::new(Matcher {
                kind,
                content,
                display,
                min: 0,
                max: 0,
                cond,
                hit,
                miss,
                regex,
            })))
        }
        other => Err(format!("Unsupported rule element: {}", other)),
    }
}

fn override_hit(obj: &serde_json::Map<String, Value>, def: MatchResult, key: &str, param: &str) -> MatchResult {
    match obj.get("hit").and_then(|v| v.as_str()) {
        Some(name) => match MatchResultEnum::parse(name) {
            Some(r) => MatchResult::new(r, Some(TranslationComponent::strs(key, vec![param.to_string()]))),
            None => def,
        },
        None => def,
    }
}

fn override_miss(obj: &serde_json::Map<String, Value>, def: MatchResult, key: &str, param: &str) -> MatchResult {
    match obj.get("miss").and_then(|v| v.as_str()) {
        Some(name) => match MatchResultEnum::parse(name) {
            Some(r) => MatchResult::new(r, Some(TranslationComponent::strs(key, vec![param.to_string()]))),
            None => def,
        },
        None => def,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_basic() {
        let rules = parse_rules(&[
            r#"{"method":"STARTS_WITH","content":"-hp"}"#.to_string(),
        ]);
        assert!(match_rule(&rules, Some("-HP1234")).hit);
        assert!(!match_rule(&rules, Some("-qB4500-")).hit);
    }

    #[test]
    fn false_veto() {
        let rules = parse_rules(&[
            r#"{"method":"STARTS_WITH","content":"-qb","hit":"TRUE"}"#.to_string(),
            r#"{"method":"EQUALS","content":"-qB4500-","hit":"FALSE"}"#.to_string(),
        ]);
        // FALSE 具有一票否决
        assert!(!match_rule(&rules, Some("-qB4500-")).hit);
        assert!(match_rule(&rules, Some("-qB4600-")).hit);
    }

    #[test]
    fn regex_full_match_case_sensitive() {
        let rules = parse_rules(&[r#"{"method":"REGEX","content":"-abc.*"}"#.to_string()]);
        assert!(match_rule(&rules, Some("-abc123")).hit);
        assert!(!match_rule(&rules, Some("-ABC123")).hit);
        assert!(!match_rule(&rules, Some("x-abc123")).hit);
    }

    #[test]
    fn null_content_treated_as_empty() {
        let rules = parse_rules(&[r#"{"method":"EQUALS","content":""}"#.to_string()]);
        assert!(match_rule(&rules, None).hit);
    }

    #[test]
    fn length_matcher() {
        let rules = parse_rules(&[r#"{"method":"LENGTH","min":3,"max":5}"#.to_string()]);
        assert!(match_rule(&rules, Some("abcd")).hit);
        assert!(!match_rule(&rules, Some("ab")).hit);
    }

    #[test]
    fn condition_if() {
        // 上游语义：if 条件仅在返回 FALSE 时否决；默认 miss=DEFAULT 不否决
        let rules = parse_rules(&[
            r#"{"method":"CONTAINS","content":"xunlei","if":{"method":"STARTS_WITH","content":"-xl"}}"#.to_string(),
        ]);
        assert!(match_rule(&rules, Some("-qb xunlei")).hit); // cond miss=DEFAULT → 继续 match0 → 命中
        assert!(match_rule(&rules, Some("-xl xunlei")).hit);

        // 显式 miss=FALSE 才产生否决
        let rules = parse_rules(&[
            r#"{"method":"CONTAINS","content":"xunlei","if":{"method":"STARTS_WITH","content":"-xl","miss":"FALSE"}}"#.to_string(),
        ]);
        assert!(!match_rule(&rules, Some("-qb xunlei")).hit);
        assert!(match_rule(&rules, Some("-xl xunlei")).hit);
    }
}
