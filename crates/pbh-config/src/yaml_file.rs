//! 保留注释的行级 YAML 编辑器（对应上游 YamlConfiguration set/save 的行为子集）。
//!
//! 上游使用 Bukkit 系 YamlConfiguration：`set(path, value)` 后 `save()` 会保留文件中的
//! 全部注释。Rust 生态没有现成等价物，这里针对 PBH 配置文件的受限 YAML 子集
//! （块映射 / 块序列 / 标量，2 空格缩进，行注释）实现行级编辑：
//! - 定位点分路径对应的行/子树
//! - 标量替换、子树替换、缺失路径创建、子树删除
//! - 其余行（含注释、空行）原样保留
//!
//! 局限：被替换子树内部的注释会丢失（上游同样如此——Bukkit 只保留仍存在的键的注释）。
use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
struct Line {
    indent: usize,
    text: String, // 原始行文本（含缩进）
}

fn is_blank_or_comment(l: &Line) -> bool {
    let t = l.text.trim_start();
    t.is_empty() || t.starts_with('#')
}

/// 提取一个映射行的键（`key:` 或 `key: value`），键可能带引号
fn line_key(l: &Line) -> Option<String> {
    let t = l.text.trim_start();
    if t.starts_with("- ") || t == "-" || t.starts_with('#') || t.is_empty() {
        return None;
    }
    let colon = find_key_colon(t)?;
    let key = t[..colon].trim();
    Some(unquote(key))
}

fn find_key_colon(t: &str) -> Option<usize> {
    // 找到不在引号内的第一个 ": "（或行尾 ":"）
    let b = t.as_bytes();
    let mut in_s = false;
    let mut in_d = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        match c {
            '\'' if !in_d => in_s = !in_s,
            '"' if !in_s => in_d = !in_d,
            '#' if !in_s && !in_d && i > 0 && (b[i - 1] as char).is_whitespace() => return None,
            ':' if !in_s && !in_d => {
                if i + 1 == b.len() || (b[i + 1] as char).is_whitespace() {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 {
        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

pub struct YamlFile {
    path: PathBuf,
    lines: Vec<Line>,
}

/// YAML 值的受限表示（写入用）
#[derive(Clone, Debug, PartialEq)]
pub enum YValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<YValue>),
    Map(Vec<(String, YValue)>), // 有序
}

impl YValue {
    pub fn from_json(v: &serde_json::Value) -> YValue {
        match v {
            serde_json::Value::Null => YValue::Null,
            serde_json::Value::Bool(b) => YValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    YValue::Int(i)
                } else {
                    YValue::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => YValue::Str(s.clone()),
            serde_json::Value::Array(a) => YValue::List(a.iter().map(YValue::from_json).collect()),
            serde_json::Value::Object(m) => YValue::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), YValue::from_json(v)))
                    .collect(),
            ),
        }
    }
}

fn format_scalar(v: &YValue) -> String {
    match v {
        YValue::Null => "\"\"".to_string(),
        YValue::Bool(b) => b.to_string(),
        YValue::Int(i) => i.to_string(),
        YValue::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{:.1}", f)
            } else {
                format!("{}", f)
            }
        }
        YValue::Str(s) => quote_str(s),
        YValue::Map(_) => "{}".to_string(),
        YValue::List(_) => "[]".to_string(),
    }
}

fn quote_str(s: &str) -> String {
    // 需要引号的情形：空串、含特殊字符、可能被解析为非字符串
    let need = s.is_empty()
        || s.starts_with(|c: char| {
            ['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`']
                .contains(&c)
        })
        || s.contains(": ")
        || s.contains(" #")
        || s.trim() != s
        || s == "~"
        || s.parse::<f64>().is_ok()
        || matches!(s.to_lowercase().as_str(), "true" | "false" | "null" | "yes" | "no" | "on" | "off");
    if need {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                _ => out.push(c),
            }
        }
        out.push('"');
        out
    } else {
        s.to_string()
    }
}

fn serialize_block(v: &YValue, indent: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    match v {
        YValue::Map(entries) => {
            for (k, val) in entries {
                match val {
                    YValue::Map(inner) if !inner.is_empty() => {
                        out.push(format!("{}{}:", pad, k));
                        serialize_block(val, indent + 2, out);
                    }
                    YValue::List(items) if !items.is_empty() => {
                        out.push(format!("{}{}:", pad, k));
                        serialize_block(val, indent, out);
                    }
                    YValue::Map(_) | YValue::List(_) => {
                        // 空映射/空序列
                        let inline = if matches!(val, YValue::Map(_)) { "{}" } else { "[]" };
                        out.push(format!("{}{}: {}", pad, k, inline));
                    }
                    scalar => out.push(format!("{}{}: {}", pad, k, format_scalar(scalar))),
                }
            }
        }
        YValue::List(items) => {
            for item in items {
                match item {
                    YValue::Map(entries) if !entries.is_empty() => {
                        // - key: val 形式
                        let mut sub = Vec::new();
                        serialize_block(item, indent + 2, &mut sub);
                        if let Some(first) = sub.first() {
                            out.push(format!("{}- {}", pad, first.trim_start()));
                            out.extend(sub[1..].to_vec());
                        } else {
                            out.push(format!("{}- {{}}", pad));
                        }
                        let _ = entries;
                    }
                    YValue::Map(_) => out.push(format!("{}- {{}}", pad)),
                    YValue::List(_) => {
                        out.push(format!("{}-", pad));
                        serialize_block(item, indent + 2, out);
                    }
                    scalar => out.push(format!("{}- {}", pad, format_scalar(scalar))),
                }
            }
        }
        scalar => out.push(format!("{}{}", pad, format_scalar(scalar))),
    }
}

struct Located {
    /// 键所在行号
    key_line: usize,
    /// 子树结束（不含）：下一个 indent <= key_indent 的非注释行的行号；注释归属保守处理
    end: usize,
    indent: usize,
}

impl YamlFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let text = std::fs::read_to_string(&path)?;
        Ok(Self::from_text(path, &text))
    }

    pub fn from_text(path: PathBuf, text: &str) -> Self {
        let lines = text
            .lines()
            .map(|l| Line {
                indent: l.len() - l.trim_start().len(),
                text: l.to_string(),
            })
            .collect();
        Self { path, lines }
    }

    fn locate(&self, path: &str) -> Option<Located> {
        let segs: Vec<&str> = path.split('.').collect();
        self.locate_segs(&segs, 0, 0, self.lines.len())
    }

    fn locate_segs(&self, segs: &[&str], depth: usize, start: usize, end: usize, ) -> Option<Located> {
        if depth >= segs.len() {
            return None;
        }
        let key = segs[depth];
        // 在 [start,end) 内、以「当前层缩进」寻找 key
        // 当前层缩进 = 范围内第一个非注释行的缩进
        let mut cur_indent = None;
        let mut i = start;
        while i < end {
            let l = &self.lines[i];
            if is_blank_or_comment(l) {
                i += 1;
                continue;
            }
            if cur_indent.is_none() {
                cur_indent = Some(l.indent);
            }
            let ind = cur_indent.unwrap();
            if l.indent < ind {
                break;
            }
            if l.indent == ind {
                if line_key(l).as_deref() == Some(key) {
                    // 计算子树范围
                    let mut j = i + 1;
                    while j < end {
                        let lj = &self.lines[j];
                        if is_blank_or_comment(lj) {
                            j += 1;
                            continue;
                        }
                        if lj.indent <= ind {
                            break;
                        }
                        j += 1;
                    }
                    // 回退尾部注释/空行：它们属于下一个键
                    while j > i + 1 {
                        let lp = &self.lines[j - 1];
                        if is_blank_or_comment(lp) {
                            j -= 1;
                        } else {
                            break;
                        }
                    }
                    let located = Located {
                        key_line: i,
                        end: j,
                        indent: ind,
                    };
                    if depth + 1 == segs.len() {
                        return Some(located);
                    }
                    // 深入子树（键行之后）
                    return self.locate_segs(segs, depth + 1, i + 1, located.end);
                }
            }
            i += 1;
        }
        None
    }

    pub fn contains(&self, path: &str) -> bool {
        self.locate(path).is_some()
    }

    /// 设置标量或子树；路径不存在时递归创建
    pub fn set(&mut self, path: &str, value: &YValue) -> Result<()> {
        if let Some(loc) = self.locate(path) {
            let mut new_lines = Vec::new();
            let key = seg_last(path);
            match value {
                YValue::Map(entries) if !entries.is_empty() => {
                    // 键行改为 `key:`，子树重新序列化
                    new_lines.push(format!("{}{}:", " ".repeat(loc.indent), key));
                    serialize_block(value, loc.indent + 2, &mut new_lines);
                }
                YValue::List(items) if !items.is_empty() => {
                    new_lines.push(format!("{}{}:", " ".repeat(loc.indent), key));
                    serialize_block(value, loc.indent, &mut new_lines);
                }
                scalar => {
                    new_lines.push(format!(
                        "{}{}: {}",
                        " ".repeat(loc.indent),
                        key,
                        format_scalar(scalar)
                    ));
                }
            }
            let new_lines: Vec<Line> = new_lines
                .into_iter()
                .map(|t| Line {
                    indent: t.len() - t.trim_start().len(),
                    text: t,
                })
                .collect();
            self.lines.splice(loc.key_line..loc.end, new_lines);
            return Ok(());
        }
        // 不存在 → 确保父路径存在后插入
        let segs: Vec<&str> = path.split('.').collect();
        if segs.len() == 1 {
            // 顶层新键：追加到文件末尾
            let mut out = Vec::new();
            let v = YValue::Map(vec![(segs[0].to_string(), value.clone())]);
            serialize_block(&v, 0, &mut out);
            for t in out {
                self.lines.push(Line {
                    indent: t.len() - t.trim_start().len(),
                    text: t,
                });
            }
            return Ok(());
        }
        let parent = segs[..segs.len() - 1].join(".");
        if !self.contains(&parent) {
            // 父链缺失：整体构建嵌套映射并追加（避免产生 `key: {}` 内联空映射 + 子级的非法 YAML）
            let mut nested = YValue::Map(vec![(seg_last(path).to_string(), value.clone())]);
            for seg in segs[..segs.len() - 1].iter().rev() {
                nested = YValue::Map(vec![((*seg).to_string(), nested)]);
            }
            let mut out = Vec::new();
            serialize_block(&nested, 0, &mut out);
            for t in out {
                self.lines.push(Line {
                    indent: t.len() - t.trim_start().len(),
                    text: t,
                });
            }
            return Ok(());
        }
        let ploc = self
            .locate(&parent)
            .ok_or_else(|| anyhow::anyhow!("parent vanished: {}", parent))?;
        let child_indent = if ploc.end > ploc.key_line + 1 {
            // 已有子节点：取第一个非注释子行的缩进
            (ploc.key_line + 1..ploc.end)
                .find_map(|i| {
                    let l = &self.lines[i];
                    if is_blank_or_comment(l) {
                        None
                    } else {
                        Some(l.indent)
                    }
                })
                .unwrap_or(ploc.indent + 2)
        } else {
            ploc.indent + 2
        };
        let mut out = Vec::new();
        let v = YValue::Map(vec![(seg_last(path).to_string(), value.clone())]);
        serialize_block(&v, child_indent, &mut out);
        let insert_at = ploc.end;
        let new_lines: Vec<Line> = out
            .into_iter()
            .map(|t| Line {
                indent: t.len() - t.trim_start().len(),
                text: t,
            })
            .collect();
        self.lines.splice(insert_at..insert_at, new_lines);
        Ok(())
    }

    pub fn remove(&mut self, path: &str) -> bool {
        if let Some(loc) = self.locate(path) {
            self.lines.drain(loc.key_line..loc.end);
            true
        } else {
            false
        }
    }

    pub fn text(&self) -> String {
        let mut s = self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    /// 原子保存
    pub fn save(&self) -> Result<()> {
        let tmp = self.path.with_extension("yml.tmp");
        std::fs::write(&tmp, self.text())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn seg_last(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// YAML(serde_yaml::Value) <-> JSON 转换，含上游的键名 '-'↔'_' 替换
// ---------------------------------------------------------------------------

pub fn yaml_to_json(v: &serde_yaml::Value) -> serde_json::Value {
    match v {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::json!(i)
            } else if let Some(u) = n.as_u64() {
                serde_json::json!(u)
            } else {
                serde_json::json!(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_yaml::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => {
            serde_json::Value::Array(seq.iter().map(yaml_to_json).collect())
        }
        serde_yaml::Value::Mapping(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                let key = match k {
                    serde_yaml::Value::String(s) => s.replace('-', "_"),
                    other => format!("{:?}", other),
                };
                out.insert(key, yaml_to_json(val));
            }
            serde_json::Value::Object(out)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

/// 键 '_'→'-'（仅当目标 YAML 中不存在原始下划线键时替换，对齐上游 mergeYaml）
pub fn json_key_to_yaml(key: &str, exists_with_underscore: bool) -> String {
    if exists_with_underscore {
        key.to_string()
    } else {
        key.replace('_', "-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# 顶部注释
config-version: 48
# 服务器设置
server:
  # 端口注释
  http: 9898
  address: "0.0.0.0"
  token: ""
client:
  qb-1:
    type: qbittorrent
    name: qb
"#;

    #[test]
    fn set_scalar_preserves_comments() {
        let mut f = YamlFile::from_text(PathBuf::from("x.yml"), SAMPLE);
        f.set("server.token", &YValue::Str("abc123".into())).unwrap();
        let t = f.text();
        assert!(t.contains("# 端口注释"));
        assert!(t.contains("# 服务器设置"));
        assert!(t.contains("token: abc123"));
        assert!(!t.contains("token: \"\""));
    }

    #[test]
    fn set_existing_map_key_inline() {
        let mut f = YamlFile::from_text(PathBuf::from("x.yml"), SAMPLE);
        f.set("server.http", &YValue::Int(1234)).unwrap();
        assert!(f.text().contains("http: 1234"));
        assert!(f.text().contains("address: \"0.0.0.0\""));
    }

    #[test]
    fn create_missing_path() {
        let mut f = YamlFile::from_text(PathBuf::from("x.yml"), SAMPLE);
        f.set("btn.enabled", &YValue::Bool(true)).unwrap();
        let t = f.text();
        assert!(t.contains("btn:"));
        assert!(t.contains("enabled: true"));
        // 原内容保持
        assert!(t.contains("config-version: 48"));
    }

    #[test]
    fn replace_subtree() {
        let mut f = YamlFile::from_text(PathBuf::from("x.yml"), SAMPLE);
        let v = YValue::Map(vec![
            ("qb-2".to_string(),
             YValue::Map(vec![
                 ("type".to_string(), YValue::Str("qbittorrent".into())),
                 ("endpoint".to_string(), YValue::Str("http://127.0.0.1:1".into())),
             ])),
        ]);
        f.set("client", &v).unwrap();
        let t = f.text();
        assert!(t.contains("qb-2:"));
        assert!(!t.contains("qb-1"));
        assert!(t.contains("# 顶部注释"));
    }

    #[test]
    fn remove_subtree() {
        let mut f = YamlFile::from_text(PathBuf::from("x.yml"), SAMPLE);
        assert!(f.remove("client"));
        let t = f.text();
        assert!(!t.contains("qb-1"));
        assert!(t.contains("server:"));
    }

    #[test]
    fn quote_rules() {
        assert_eq!(quote_str(""), "\"\"");
        assert_eq!(quote_str("0.0.0.0"), "0.0.0.0"); // 非法数字 → 不加引号
        assert_eq!(quote_str("5000"), "\"5000\""); // 数字样式字符串保持字符串类型
        assert_eq!(quote_str("true"), "\"true\"");
    }

    #[test]
    fn list_roundtrip() {
        let mut f = YamlFile::from_text(PathBuf::from("x.yml"), SAMPLE);
        let v = YValue::List(vec![
            YValue::Str("1.2.3.4".into()),
            YValue::Str("::/0".into()),
        ]);
        f.set("module.ip-address-blocker.ips", &v).unwrap();
        let t = f.text();
        assert!(t.contains("- 1.2.3.4"));
        assert!(t.contains("- \"::/0\"") || t.contains("- ::/0"));
    }
}
