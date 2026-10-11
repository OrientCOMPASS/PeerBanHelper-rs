//! pbh-config：数据目录与配置装载。
//!
//! 直接读取上游 PeerBanHelper 的 `config.yml`（config-version 48）与 `profile.yml`
//! （config-version 40），Bukkit 风格点分路径访问（如 `module.peer-id-blacklist.enabled`）。
//!
//! 已知限制（M0 范围）：只读。注释保留的写回（yaml round-trip 编辑器）与
//! config-version 迁移脚本链在后续里程碑实现；当前若配置文件缺失，将写入与上游
//! 相同版本的模板（模板拷贝自上游仓库，GPLv3 同源）。
use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde_yaml::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod yaml_file;
pub use yaml_file::{YamlFile, YValue};

pub const TEMPLATE_CONFIG: &str = include_str!("templates/config.yml");
pub const TEMPLATE_PROFILE: &str = include_str!("templates/profile.yml");

/// 点分路径访问的 YAML 文档
#[derive(Clone, Debug)]
pub struct YamlDoc {
    pub root: Value,
}

pub fn yaml_get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.') {
        match cur {
            Value::Mapping(map) => {
                let key = Value::String(part.to_string());
                cur = map.get(&key)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl YamlDoc {
    pub fn parse(text: &str) -> Result<Self> {
        let root: Value = serde_yaml::from_str(text).context("invalid YAML document")?;
        Ok(Self { root })
    }

    pub fn get(&self, path: &str) -> Option<&Value> {
        yaml_get(&self.root, path)
    }

    pub fn get_str(&self, path: &str, def: &str) -> String {
        self.get(path)
            .and_then(value_to_string)
            .unwrap_or_else(|| def.to_string())
    }

    pub fn get_str_opt(&self, path: &str) -> Option<String> {
        self.get(path).and_then(value_to_string)
    }

    pub fn get_i64(&self, path: &str, def: i64) -> i64 {
        match self.get(path) {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(def),
            Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(def),
            _ => def,
        }
    }

    pub fn get_bool(&self, path: &str, def: bool) -> bool {
        match self.get(path) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => match s.to_lowercase().as_str() {
                "true" => true,
                "false" => false,
                _ => def,
            },
            _ => def,
        }
    }

    /// Bukkit getStringList 语义：序列元素转字符串；单值转单元素列表；缺失返回空
    pub fn get_str_list(&self, path: &str) -> Vec<String> {
        match self.get(path) {
            Some(Value::Sequence(seq)) => seq.iter().filter_map(value_to_string).collect(),
            Some(v @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => {
                vec![value_to_string(v).unwrap_or_default()]
            }
            _ => Vec::new(),
        }
    }

    pub fn get_i64_list(&self, path: &str) -> Vec<i64> {
        match self.get(path) {
            Some(Value::Sequence(seq)) => seq
                .iter()
                .filter_map(|v| match v {
                    Value::Number(n) => n.as_i64(),
                    Value::String(s) => s.trim().parse::<i64>().ok(),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// 返回某映射节点的直接子键列表（Bukkit getKeys(false)）
    pub fn section_keys(&self, path: &str) -> Vec<String> {
        match self.get(path) {
            Some(Value::Mapping(map)) => map
                .keys()
                .filter_map(|k| match k {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemapConfig {
    pub ipv4_enabled: bool,
    pub ipv4_range: u8,
    pub ipv6_enabled: bool,
    pub ipv6_range: u8,
}

pub struct AppConfig {
    pub data_dir: PathBuf,
    pub main: YamlDoc,
    pub profile: YamlDoc,
}

impl AppConfig {
    /// 打开（必要时初始化）数据目录：
    /// <data>/config/{config.yml,profile.yml}、<data>/db、<data>/logs、<data>/static
    pub fn load_or_init(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let config_dir = data_dir.join("config");
        std::fs::create_dir_all(&config_dir).context("create config dir")?;
        std::fs::create_dir_all(data_dir.join("db")).context("create db dir")?;
        std::fs::create_dir_all(data_dir.join("logs")).context("create logs dir")?;
        std::fs::create_dir_all(data_dir.join("static")).context("create static dir")?;

        let main_path = config_dir.join("config.yml");
        let profile_path = config_dir.join("profile.yml");
        if !main_path.exists() {
            std::fs::write(&main_path, TEMPLATE_CONFIG).context("write config.yml template")?;
            tracing::info!("Created default config: {}", main_path.display());
        }
        if !profile_path.exists() {
            std::fs::write(&profile_path, TEMPLATE_PROFILE).context("write profile.yml template")?;
            tracing::info!("Created default profile: {}", profile_path.display());
        }

        let main = YamlDoc::parse(
            &std::fs::read_to_string(&main_path).context("read config.yml")?,
        )
        .context("parse config.yml")?;
        let profile = YamlDoc::parse(
            &std::fs::read_to_string(&profile_path).context("read profile.yml")?,
        )
        .context("parse profile.yml")?;

        let main_ver = main.get_i64("config-version", 0);
        let profile_ver = profile.get_i64("config-version", 0);
        if main_ver != 48 {
            tracing::warn!(
                "config.yml config-version = {} (expected 48). 迁移脚本链尚未实现，将继续尝试加载。",
                main_ver
            );
        }
        if profile_ver != 40 {
            tracing::warn!(
                "profile.yml config-version = {} (expected 40). 迁移脚本链尚未实现，将继续尝试加载。",
                profile_ver
            );
        }

        Ok(Self {
            data_dir,
            main,
            profile,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("db").join("peerbanhelper.db")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn static_dir(&self) -> PathBuf {
        self.data_dir.join("static")
    }

    // ---- main config 访问器 ----

    pub fn server_http_port(&self) -> u16 {
        self.main.get_i64("server.http", 9898) as u16
    }

    pub fn server_address(&self) -> String {
        self.main.get_str("server.address", "0.0.0.0")
    }

    pub fn token(&self) -> String {
        self.main.get_str("server.token", "")
    }

    pub fn language(&self) -> String {
        let lang = self.main.get_str("language", "default");
        if lang == "default" {
            detect_system_locale()
        } else {
            pbh_core::lang::LangStore::normalize_locale(Some(&lang))
        }
    }

    pub fn hide_finish_log(&self) -> bool {
        self.main.get_bool("logger.hide-finish-log", false)
    }

    pub fn persist_banlist(&self) -> bool {
        self.main.get_bool("persist.banlist", true)
    }

    pub fn remap_config(&self) -> RemapConfig {
        RemapConfig {
            ipv4_enabled: self.main.get_bool("banlist-remapping.ipv4.enabled", false),
            ipv4_range: self.main.get_i64("banlist-remapping.ipv4.remap-range", 30) as u8,
            ipv6_enabled: self.main.get_bool("banlist-remapping.ipv6.enabled", true),
            ipv6_range: self.main.get_i64("banlist-remapping.ipv6.remap-range", 52) as u8,
        }
    }

    // ---- profile 访问器 ----

    pub fn check_interval_ms(&self) -> u64 {
        self.profile.get_i64("check-interval", 5000).max(1000) as u64
    }

    pub fn ban_duration_ms(&self) -> i64 {
        self.profile.get_i64("ban-duration", 1209600000)
    }

    pub fn ignore_peers_from_addresses(&self) -> Vec<String> {
        self.profile.get_str_list("ignore-peers-from-addresses")
    }

    pub fn module_enabled(&self, name: &str, def: bool) -> bool {
        self.profile
            .get_bool(&format!("module.{}.enabled", name), def)
    }

    /// 模块配置节路径前缀
    pub fn module_path(&self, name: &str, key: &str) -> String {
        format!("module.{}.{}", name, key)
    }
}

fn detect_system_locale() -> String {
    for var in ["PBH_LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() && v != "C" && v != "POSIX" {
                return pbh_core::lang::LangStore::normalize_locale(Some(&v));
            }
        }
    }
    "en_us".to_string()
}


// ---------------------------------------------------------------------------
// 热重载句柄：引擎与 Web 层共享
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<Arc<AppConfig>>>,
    pub data_dir: PathBuf,
}

impl ConfigHandle {
    pub fn load_or_init(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let cfg = Arc::new(AppConfig::load_or_init(&data_dir)?);
        Ok(Self {
            inner: Arc::new(RwLock::new(cfg)),
            data_dir,
        })
    }

    pub fn get(&self) -> Arc<AppConfig> {
        self.inner.read().clone()
    }

    /// 重新从磁盘解析配置并原子替换
    pub fn reload(&self) -> Result<Arc<AppConfig>> {
        let cfg = Arc::new(AppConfig::load_or_init(&self.data_dir)?);
        *self.inner.write() = cfg.clone();
        Ok(cfg)
    }

    pub fn main_file(&self) -> PathBuf {
        self.data_dir.join("config").join("config.yml")
    }

    pub fn profile_file(&self) -> PathBuf {
        self.data_dir.join("config").join("profile.yml")
    }
}

// ---------------------------------------------------------------------------
// GET/PUT /api/general/{config,profile} 的 JSON 视图转换（对齐上游
// handleConfigGet/handleConfigPut 的 replaceKeys/mergeYaml 与特殊列表转换）
// ---------------------------------------------------------------------------

const RULE_LIST_PATHS: &[(&str, &str)] = &[
    ("module.peer-id-blacklist", "banned-peer-id"),
    ("module.client-name-blacklist", "banned-client-name"),
    ("module.ptr-blacklist", "ptr-rules"),
];

fn yaml_value_at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    yaml_get(v, path)
}

/// YAML → WebUI JSON 视图：键 '-'→'_'；三个规则字符串列表 → 对象列表
pub fn config_json_view(doc: &YamlDoc, which: &str) -> serde_json::Value {
    let mut root = doc.root.clone();
    if which == "profile" {
        for (section, field) in RULE_LIST_PATHS {
            if let Some(Value::Sequence(_)) = yaml_value_at(&root, &format!("{}.{}", section, field)) {
                let path = format!("{}.{}", section, field);
                let items: Vec<Value> = doc
                    .get_str_list(&path)
                    .iter()
                    .map(|s| match serde_json::from_str::<serde_json::Value>(s) {
                        Ok(v) => json_to_yaml_value(&v),
                        Err(_) => Value::String(s.clone()),
                    })
                    .collect();
                set_yaml_value(&mut root, &path, Value::Sequence(items));
            }
        }
    }
    if which == "config" {
        // push-notification：字典 → 列表（每项附加 push-notification-name）
        if let Some(Value::Mapping(map)) = yaml_value_at(&root, "push-notification").cloned().as_ref() {
            let mut list = Vec::new();
            for (k, v) in map {
                let name = match k {
                    Value::String(s) => s.clone(),
                    _ => continue,
                };
                let mut entry = match v {
                    Value::Mapping(m) => m.clone(),
                    _ => continue,
                };
                entry.insert(Value::String("push-notification-name".into()), Value::String(name));
                list.push(Value::Mapping(entry));
            }
            set_yaml_value(&mut root, "push-notification", Value::Sequence(list));
        }
    }
    yaml_file::yaml_to_json(&root)
}

/// WebUI JSON 提交 → 合并写回 YAML 文件（保留注释）。
/// 对齐上游 mergeYaml：键 '_'→'-'（若原名存在则保留）、规则对象列表 → JSON 字符串列表、
/// push-notification 列表 → 字典。仅更新 body 中出现的键（合并语义）。
pub fn merge_json_into_file(path: &Path, body: &serde_json::Value, which: &str) -> Result<()> {
    let mut file = YamlFile::load(path)?;
    let current = YamlDoc::parse(&std::fs::read_to_string(path)?)?;
    let obj = body
        .as_object()
        .context("config body must be a JSON object")?;
    merge_level(&mut file, &current, "", obj, which)?;
    file.save()?;
    Ok(())
}

fn merge_level(
    file: &mut YamlFile,
    current: &YamlDoc,
    prefix: &str,
    body: &serde_json::Map<String, serde_json::Value>,
    which: &str,
) -> Result<()> {
    for (k, v) in body {
        // 上游：cfg.contains(key) ? key : key.replace('_','-')
        let yaml_key = if !prefix.is_empty() && current.get(&format!("{}.{}", prefix, k)).is_some() {
            k.clone()
        } else if prefix.is_empty() && current.get(k).is_some() {
            k.clone()
        } else {
            k.replace('_', "-")
        };
        let full = if prefix.is_empty() {
            yaml_key.clone()
        } else {
            format!("{}.{}", prefix, yaml_key)
        };

        // 特殊逆向转换
        let mut v_owned;
        let v: &serde_json::Value = if which == "profile"
            && RULE_LIST_PATHS.iter().any(|(s, f)| full == format!("{}.{}", s, f))
            && v.is_array()
        {
            v_owned = serde_json::Value::Array(
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|item| {
                        if item.is_object() {
                            serde_json::Value::String(item.to_string())
                        } else {
                            item.clone()
                        }
                    })
                    .collect(),
            );
            &v_owned
        } else if which == "config" && full == "push-notification" && v.is_array() {
            let mut dict = serde_json::Map::new();
            for item in v.as_array().unwrap() {
                if let Some(obj) = item.as_object() {
                    let mut obj = obj.clone();
                    let name = obj
                        .remove("push_notification_name")
                        .or_else(|| obj.remove("push-notification-name"))
                        .and_then(|n| n.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "unnamed".to_string());
                    // 视图中的键是下划线形式，转回连字符
                    let converted: serde_json::Map<String, serde_json::Value> = obj
                        .into_iter()
                        .map(|(kk, vv)| (kk.replace('_', "-"), vv))
                        .collect();
                    dict.insert(name, serde_json::Value::Object(converted));
                }
            }
            v_owned = serde_json::Value::Object(dict);
            &v_owned
        } else {
            v_owned = serde_json::Value::Null;
            let _ = &mut v_owned;
            v
        };

        if let Some(obj) = v.as_object() {
            // 若目标存在且是映射 → 递归合并（保留未触及子键的注释）；否则整体替换
            let exists_as_map = matches!(current.get(&full), Some(Value::Mapping(_)));
            if exists_as_map {
                merge_level(file, current, &full, obj, which)?;
                continue;
            }
        }
        file.set(&full, &YValue::from_json(v))?;
    }
    Ok(())
}

fn json_to_yaml_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(serde_yaml::Number::from(i))
            } else {
                Value::Number(serde_yaml::Number::from(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(a) => Value::Sequence(a.iter().map(json_to_yaml_value).collect()),
        serde_json::Value::Object(m) => {
            let mut map = serde_yaml::Mapping::new();
            for (k, val) in m {
                map.insert(Value::String(k.clone()), json_to_yaml_value(val));
            }
            Value::Mapping(map)
        }
    }
}

fn set_yaml_value(root: &mut Value, path: &str, new: Value) {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        let is_last = i + 1 == segs.len();
        match cur {
            Value::Mapping(map) => {
                let key = Value::String(seg.to_string());
                if is_last {
                    map.insert(key, new.clone());
                    return;
                }
                if !map.contains_key(&key) {
                    map.insert(key.clone(), Value::Mapping(serde_yaml::Mapping::new()));
                }
                cur = map.get_mut(&key).unwrap();
            }
            _ => return,
        }
    }
}
