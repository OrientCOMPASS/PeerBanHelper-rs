//! pbh-modules：规则模块（对应上游 module/impl/rule 包的行为子集）。
//!
//! MVP 实现三个基础规则模块，语义与上游一致：
//! - PeerIdBlacklist（peer-id-blacklist）
//! - ClientNameBlacklist（client-name-blacklist）
//! - IPBlackList（ip-address-blocker，仅 IP/CIDR + 端口；GeoIP/ASN/网络类型在 M2 接入）
//!
//! 上游 quirk 复刻：PeerIdBlacklist 命中时 reason 使用 MODULE_CNB_MATCH_CLIENT_NAME 键
//! （上游即如此，为保持进程外数据一致不“修复”）。
pub mod anti_vampire;
pub mod auto_range;
pub mod idle_dos;
pub mod multi_dial;
pub mod pcb;
pub mod rule_sub;

use anti_vampire::AntiVampire;
use auto_range::AutoRangeBan;
use idle_dos::IdleConnectionDosProtection;
use multi_dial::MultiDialingBlocker;
use rule_sub::IpBlackRuleList;
use pbh_config::{AppConfig, ConfigHandle};
use pbh_core::banlist::BanListHandle;
use pbh_db::Database;
use pbh_downloader::Downloader;
use pcb::ProgressCheatBlocker;
use pbh_core::check::{CheckResult, PeerAction};
use pbh_core::context;
use pbh_core::ipset::CidrSet;
use pbh_core::lang::LangStore;
use pbh_core::model::{Peer, Torrent};
use pbh_core::rule::{self, parse_rules, Rule};
use pbh_core::translate::{TcParam, TranslationComponent};
use serde_json::{Map, Value};
use std::sync::Arc;

pub trait RuleModule: Send + Sync {
    fn name(&self) -> &'static str;
    fn config_name(&self) -> &'static str;
    fn should_ban_peer(&self, torrent: &Torrent, peer: &Peer, downloader: &dyn Downloader) -> CheckResult;
    /// /api/modules/{configName} 的 data 负载
    fn web_data(&self, locale: &str, lang: &LangStore) -> Value;
    /// 每轮 BanWave 结束（对应 PBHCache 批量回写等收尾动作）
    fn on_wave_complete(&self) {}
    /// 封禁解除事件（对应上游 @Subscribe onPeerUnBan）
    fn on_peer_unban(&self, _torrent_id: &str, _ip: &str) {}
    /// 供宿主按具体类型访问模块扩展能力（如 /api/sub/* 需要 IpBlackRuleList）
    fn as_any(&self) -> &dyn std::any::Any;
}

fn rule_printable_list(rules: &[Rule], lang: &LangStore, locale: &str) -> Vec<Value> {
    rules
        .iter()
        .map(|r| Value::String(r.to_printable_text(lang, locale)))
        .collect()
}

// ---------------------------------------------------------------------------
// PeerIdBlacklist
// ---------------------------------------------------------------------------

pub struct PeerIdBlacklist {
    ban_duration: i64,
    rules: Vec<Rule>,
}

impl PeerIdBlacklist {
    pub fn from_profile(cfg: &AppConfig, _lang: &LangStore) -> Option<Arc<dyn RuleModule>> {
        if !cfg.module_enabled("peer-id-blacklist", true) {
            return None;
        }
        let ban_duration = cfg
            .profile
            .get_i64(&cfg.module_path("peer-id-blacklist", "ban-duration"), 0);
        let rules = parse_rules(&cfg.profile.get_str_list("module.peer-id-blacklist.banned-peer-id"));
        Some(Arc::new(Self { ban_duration, rules }))
    }
}

impl RuleModule for PeerIdBlacklist {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "PeerId Blacklist"
    }
    fn config_name(&self) -> &'static str {
        "peer-id-blacklist"
    }

    fn should_ban_peer(&self, _torrent: &Torrent, peer: &Peer, _downloader: &dyn Downloader) -> CheckResult {
        if peer.handshaking
            && peer.peer_id.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true)
        {
            return CheckResult::handshaking();
        }
        let match_result = rule::match_rule(&self.rules, peer.peer_id.as_deref());
        if match_result.hit {
            if let Some(rule) = &match_result.rule {
                let mut sd = Map::new();
                sd.insert("rule".into(), Value::String(rule.metadata()));
                let reason_param: Vec<TcParam> = match match_result.comment.clone() {
                    Some(tc) => vec![TcParam::Tc(Box::new(tc))],
                    None => vec![],
                };
                return CheckResult::new(
                    context::PEER_ID_BLACKLIST,
                    PeerAction::Ban,
                    self.ban_duration,
                    rule.matcher_name(),
                    TranslationComponent::with("MODULE_CNB_MATCH_CLIENT_NAME", reason_param),
                    sd,
                );
            }
        }
        CheckResult::pass()
    }

    fn web_data(&self, locale: &str, lang: &LangStore) -> Value {
        Value::Object({
            let mut m = Map::new();
            m.insert(
                "peerId".into(),
                Value::Array(rule_printable_list(&self.rules, lang, locale)),
            );
            m
        })
    }
}

// ---------------------------------------------------------------------------
// ClientNameBlacklist
// ---------------------------------------------------------------------------

pub struct ClientNameBlacklist {
    ban_duration: i64,
    rules: Vec<Rule>,
}

impl ClientNameBlacklist {
    pub fn from_profile(cfg: &AppConfig, _lang: &LangStore) -> Option<Arc<dyn RuleModule>> {
        if !cfg.module_enabled("client-name-blacklist", true) {
            return None;
        }
        let ban_duration = cfg
            .profile
            .get_i64(&cfg.module_path("client-name-blacklist", "ban-duration"), 0);
        let rules = parse_rules(&cfg.profile.get_str_list("module.client-name-blacklist.banned-client-name"));
        Some(Arc::new(Self { ban_duration, rules }))
    }
}

impl RuleModule for ClientNameBlacklist {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "ClientName Blacklist"
    }
    fn config_name(&self) -> &'static str {
        "client-name-blacklist"
    }

    fn should_ban_peer(&self, _torrent: &Torrent, peer: &Peer, _downloader: &dyn Downloader) -> CheckResult {
        if peer.handshaking
            && peer
                .client_name
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            return CheckResult::handshaking();
        }
        let match_result = rule::match_rule(&self.rules, peer.client_name.as_deref());
        if match_result.hit {
            if let Some(rule) = &match_result.rule {
                let mut sd = Map::new();
                sd.insert("rule".into(), Value::String(rule.metadata()));
                let reason_param: Vec<TcParam> = match match_result.comment.clone() {
                    Some(tc) => vec![TcParam::Tc(Box::new(tc))],
                    None => vec![],
                };
                return CheckResult::new(
                    context::CLIENT_NAME_BLACKLIST,
                    PeerAction::Ban,
                    self.ban_duration,
                    rule.matcher_name(),
                    TranslationComponent::with("MODULE_CNB_MATCH_CLIENT_NAME", reason_param),
                    sd,
                );
            }
        }
        CheckResult::pass()
    }

    fn web_data(&self, locale: &str, lang: &LangStore) -> Value {
        Value::Object({
            let mut m = Map::new();
            m.insert(
                "clientName".into(),
                Value::Array(rule_printable_list(&self.rules, lang, locale)),
            );
            m
        })
    }
}

// ---------------------------------------------------------------------------
// IPBlackList（ip-address-blocker）
// ---------------------------------------------------------------------------

pub struct IpBlackList {
    ban_duration: i64,
    ports: Vec<i64>,
    /// (网络, 展示串, 压缩串)：展示串对应 Java IPAddress.toString()，压缩串对应 toCompressedString()
    ips: Vec<(ipnet::IpNet, String, String)>,
    asns: Vec<i64>,
    regions: Vec<String>,
    cities: Vec<String>,
    net_types: Vec<String>,
    geoip: Option<Arc<pbh_geoip::IpDb>>,
}

fn display_and_compressed(net: ipnet::IpNet, raw: &str) -> (String, String) {
    let full_prefix = match net {
        ipnet::IpNet::V4(_) => 32,
        ipnet::IpNet::V6(_) => 128,
    };
    if net.prefix_len() == full_prefix && !raw.contains('/') {
        // Java: 无前缀的单 IP，toString 不带 /32
        (net.addr().to_string(), net.addr().to_string())
    } else {
        (net.to_string(), net.trunc().to_string())
    }
}

impl IpBlackList {
    pub fn from_profile(
        cfg: &AppConfig,
        _lang: &LangStore,
        geoip: Option<Arc<pbh_geoip::IpDb>>,
    ) -> Option<Arc<dyn RuleModule>> {
        if !cfg.module_enabled("ip-address-blocker", true) {
            return None;
        }
        let ban_duration = cfg
            .profile
            .get_i64(&cfg.module_path("ip-address-blocker", "ban-duration"), 0);
        let ports = cfg.profile.get_i64_list("module.ip-address-blocker.ports");
        let asns = cfg.profile.get_i64_list("module.ip-address-blocker.asns");
        let regions = cfg.profile.get_str_list("module.ip-address-blocker.regions");
        let cities = cfg.profile.get_str_list("module.ip-address-blocker.cities");
        let net_types = cfg.profile.get_str_list("module.ip-address-blocker.net-type");
        let mut ips = Vec::new();
        for raw in cfg.profile.get_str_list("module.ip-address-blocker.ips") {
            match pbh_core::ipset::parse_cidr(&raw) {
                Some(net) => {
                    let (d, c) = display_and_compressed(net, &raw);
                    ips.push((net, d, c));
                }
                None => tracing::warn!("Invalid IP/CIDR entry in ip-address-blocker: {}", raw),
            }
        }
        if !(asns.is_empty() && regions.is_empty() && cities.is_empty() && net_types.is_empty())
            && geoip.is_none()
        {
            tracing::warn!("ip-address-blocker 配置了 GeoIP/ASN/城市/网络类型规则，但 GeoIP 数据库不可用，这部分规则将不生效");
        }
        Some(Arc::new(Self { ban_duration, ports, ips, asns, regions, cities, net_types, geoip }))
    }
}

impl IpBlackList {
    /// 对应上游 checkIPDB：asn → region → netType → city（gate：regions 与 asns 均空则跳过，
    /// 与上游一致——上游此 gate 未包含 cities/netType，属历史行为，原样复刻）
    fn check_ipdb(&self, ip: std::net::IpAddr) -> Option<CheckResult> {
        if self.regions.is_empty() && self.asns.is_empty() {
            return None;
        }
        let geoip = self.geoip.as_ref()?;
        let geo = geoip.query(ip)?;
        if !self.asns.is_empty() {
            if let Some(asn) = geo.as_data.as_ref().and_then(|a| a.number) {
                if self.asns.contains(&asn) {
                    let mut sd = Map::new();
                    sd.insert("type".into(), Value::String("asn".into()));
                    sd.insert("rule".into(), Value::Number(asn.into()));
                    return Some(CheckResult::new(
                        context::IP_BLACKLIST,
                        PeerAction::Ban,
                        self.ban_duration,
                        TranslationComponent::strs("IP_BLACKLIST_ASN_RULE", vec![asn.to_string()]),
                        TranslationComponent::strs("MODULE_IBL_MATCH_ASN", vec![asn.to_string()]),
                        sd,
                    ));
                }
            }
        }
        if !self.regions.is_empty() {
            if let Some(iso) = geo.country.as_ref().and_then(|c| c.iso.clone()) {
                if self.regions.iter().any(|r| *r == iso) {
                    let mut sd = Map::new();
                    sd.insert("type".into(), Value::String("iso".into()));
                    sd.insert("rule".into(), Value::String(iso.clone()));
                    return Some(CheckResult::new(
                        context::IP_BLACKLIST,
                        PeerAction::Ban,
                        self.ban_duration,
                        TranslationComponent::strs("IP_BLACKLIST_REGION_RULE", vec![iso.clone()]),
                        TranslationComponent::strs("MODULE_IBL_MATCH_REGION", vec![iso]),
                        sd,
                    ));
                }
            }
        }
        if !self.net_types.is_empty() {
            if let Some(nt) = geo.network.as_ref().and_then(|n| n.net_type.clone()) {
                if pbh_geoip::net_type_matches(&nt, &self.net_types) {
                    let mut sd = Map::new();
                    sd.insert("type".into(), Value::String("netTypes".into()));
                    sd.insert("rule".into(), Value::String(nt.clone()));
                    return Some(CheckResult::new(
                        context::IP_BLACKLIST,
                        PeerAction::Ban,
                        self.ban_duration,
                        TranslationComponent::strs("IP_BLACKLIST_NETTYPE_RULE", vec![nt.clone()]),
                        TranslationComponent::strs("MODULE_IBL_MATCH_IP", vec![nt]),
                        sd,
                    ));
                }
            }
        }
        if !self.cities.is_empty() {
            if let Some(name) = geo.city.as_ref().and_then(|c| c.name.clone()) {
                for c in &self.cities {
                    if name.contains(c.as_str()) {
                        let mut sd = Map::new();
                        sd.insert("type".into(), Value::String("city".into()));
                        sd.insert("rule".into(), Value::String(c.clone()));
                        return Some(CheckResult::new(
                            context::IP_BLACKLIST,
                            PeerAction::Ban,
                            self.ban_duration,
                            TranslationComponent::strs("IP_BLACKLIST_CITY_RULE", vec![c.clone()]),
                            TranslationComponent::strs("MODULE_IBL_MATCH_IP", vec![c.clone()]),
                            sd,
                        ));
                    }
                }
            }
        }
        None
    }
}

impl RuleModule for IpBlackList {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "IP Blacklist"
    }
    fn config_name(&self) -> &'static str {
        "ip-address-blocker"
    }

    fn should_ban_peer(&self, _torrent: &Torrent, peer: &Peer, _downloader: &dyn Downloader) -> CheckResult {
        if peer.handshaking {
            return CheckResult::handshaking();
        }
        let port = peer.address.port as i64;
        if self.ports.contains(&port) {
            let mut sd = Map::new();
            sd.insert("type".into(), Value::String("port".into()));
            sd.insert("rule".into(), Value::Number(port.into()));
            return CheckResult::new(
                context::IP_BLACKLIST,
                PeerAction::Ban,
                self.ban_duration,
                TranslationComponent::strs("IP_BLACKLIST_PORT_RULE", vec![port.to_string()]),
                TranslationComponent::strs("MODULE_IBL_MATCH_PORT", vec![port.to_string()]),
                sd,
            );
        }
        if let Some(ip) = peer.address.ip_addr() {
            for (net, display, compressed) in &self.ips {
                if net.contains(&ip) {
                    let mut sd = Map::new();
                    sd.insert("type".into(), Value::String("ip".into()));
                    sd.insert("rule".into(), Value::String(compressed.clone()));
                    return CheckResult::new(
                        context::IP_BLACKLIST,
                        PeerAction::Ban,
                        self.ban_duration,
                        TranslationComponent::strs(
                            "IP_BLACKLIST_CIDR_RULE",
                            vec![display.clone()],
                        ),
                        TranslationComponent::strs("MODULE_IBL_MATCH_IP", vec![display.clone()]),
                        sd,
                    );
                }
            }
            if let Some(r) = self.check_ipdb(ip) {
                return r;
            }
        }
        CheckResult::pass()
    }

    fn web_data(&self, _locale: &str, _lang: &LangStore) -> Value {
        let mut m = Map::new();
        m.insert(
            "ip".into(),
            Value::Array(self.ips.iter().map(|(_, d, _)| Value::String(d.clone())).collect()),
        );
        m.insert(
            "port".into(),
            Value::Array(self.ports.iter().map(|p| Value::Number((*p).into())).collect()),
        );
        m.insert("asn".into(), Value::Array(self.asns.iter().map(|a| Value::Number((*a).into())).collect()));
        m.insert("region".into(), Value::Array(self.regions.iter().map(|a| Value::String(a.clone())).collect()));
        m.insert("city".into(), Value::Array(self.cities.iter().map(|a| Value::String(a.clone())).collect()));
        m.insert("netType".into(), Value::Array(self.net_types.iter().map(|a| Value::String(a.clone())).collect()));
        Value::Object(m)
    }
}

// ---------------------------------------------------------------------------
// ProgressCheatBlocker 配置
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PcbConfig {
    pub ban_duration: i64,
    pub minimum_size: i64,
    pub block_excessive: bool,
    pub excessive_threshold: f64,
    pub max_difference: f64,
    pub rewind_max_difference: f64,
    pub ipv4_prefix_length: u8,
    pub ipv6_prefix_length: u8,
    pub enable_persist: bool,
    pub persist_duration: i64,
    pub max_wait_duration: i64,
    pub fast_test_percentage: f64,
    pub fast_test_block_duration: i64,
}

impl PcbConfig {
    pub fn from_profile(cfg: &AppConfig) -> Option<Self> {
        if !cfg.module_enabled("progress-cheat-blocker", true) {
            return None;
        }
        let p = |k: &str| cfg.module_path("progress-cheat-blocker", k);
        Some(Self {
            ban_duration: cfg.profile.get_i64(&p("ban-duration"), 2592000000),
            minimum_size: cfg.profile.get_i64(&p("minimum-size"), 50_000_000),
            block_excessive: cfg.profile.get_bool(&p("block-excessive-clients"), true),
            excessive_threshold: f64_or(&cfg.profile, &p("excessive-threshold"), 1.5),
            max_difference: f64_or(&cfg.profile, &p("maximum-difference"), 0.1),
            rewind_max_difference: f64_or(&cfg.profile, &p("rewind-maximum-difference"), 0.07),
            ipv4_prefix_length: cfg.profile.get_i64(&p("ipv4-prefix-length"), 32) as u8,
            ipv6_prefix_length: cfg.profile.get_i64(&p("ipv6-prefix-length"), 56) as u8,
            enable_persist: cfg.profile.get_bool(&p("enable-persist"), true),
            persist_duration: cfg.profile.get_i64(&p("persist-duration"), 1209600000),
            max_wait_duration: cfg.profile.get_i64(&p("max-wait-duration"), 30000),
            fast_test_percentage: f64_or(&cfg.profile, &p("fast-pcb-test-percentage"), 0.1),
            fast_test_block_duration: cfg.profile.get_i64(&p("fast-pcb-test-block-duration"), 15000),
        })
    }
}

fn f64_or(doc: &pbh_config::YamlDoc, path: &str, def: f64) -> f64 {
    match doc.get(path) {
        Some(serde_yaml::Value::Number(n)) => n.as_f64().unwrap_or(def),
        Some(serde_yaml::Value::String(s)) => s.trim().parse::<f64>().unwrap_or(def),
        _ => def,
    }
}

// ---------------------------------------------------------------------------

/// 按上游 registerModules 顺序构建模块列表（仅启用的规则模块）：
/// IPBlackList → PeerIdBlacklist → ClientNameBlacklist → (ExpressionRule 未实现)
/// → ProgressCheatBlocker → (MultiDialing/AutoRangeBan/BtnNetwork/IPBlackRuleList 见后续里程碑)
/// peer-id-blacklist → client-name-blacklist → ip-address-blocker
pub struct BuiltModules {
    pub list: Vec<Arc<dyn RuleModule>>,
    pub rule_sub: Option<Arc<IpBlackRuleList>>,
}

pub fn build_modules(
    cfg: &AppConfig,
    lang: &LangStore,
    db: Arc<Database>,
    banlist: BanListHandle,
    handle: &ConfigHandle,
    geoip: Option<Arc<pbh_geoip::IpDb>>,
) -> BuiltModules {
    let mut modules: Vec<Arc<dyn RuleModule>> = Vec::new();
    let mut rule_sub: Option<Arc<IpBlackRuleList>> = None;
    if let Some(m) = IpBlackList::from_profile(cfg, lang, geoip) {
        modules.push(m);
    }
    if let Some(m) = PeerIdBlacklist::from_profile(cfg, lang) {
        modules.push(m);
    }
    if let Some(m) = ClientNameBlacklist::from_profile(cfg, lang) {
        modules.push(m);
    }
    // ExpressionRule（AviatorScript）：按项目决策禁用（BTN 脚本不执行；本地表达式规则未实现）
    if cfg.module_enabled("expression-rule", false) {
        tracing::warn!("expression-rule 模块在 pbh-rs 中未实现（AviatorScript 引擎已按决策禁用），跳过");
    }
    if let Some(pcb_cfg) = PcbConfig::from_profile(cfg) {
        modules.push(ProgressCheatBlocker::new(pcb_cfg, db.clone()));
    }
    if let Some(m) = MultiDialingBlocker::from_profile(cfg) {
        modules.push(m);
    }
    if let Some(m) = AutoRangeBan::from_profile(cfg, banlist) {
        modules.push(m);
    }
    // BtnNetworkOnline：BTN 网络已按项目决策禁用
    if let Some(m) = IpBlackRuleList::from_profile(cfg, db.clone(), handle) {
        rule_sub = Some(m.clone());
        modules.push(m);
    }
    if let Some(m) = IdleConnectionDosProtection::from_profile(cfg) {
        modules.push(m);
    }
    if let Some(m) = AntiVampire::from_profile(cfg) {
        modules.push(m);
    }
    BuiltModules { list: modules, rule_sub }
}

/// 对单个 Peer 运行全部模块并合并结果（上游 DigestionSession 语义）
pub fn check_peer(
    modules: &[Arc<dyn RuleModule>],
    torrent: &Torrent,
    peer: &Peer,
    downloader: &dyn Downloader,
    ignored: &CidrSet,
) -> CheckResult {
    if let Some(ip) = peer.address.ip_addr() {
        if ignored.contains(ip) {
            return CheckResult::ignored_address();
        }
    }
    let mut result = CheckResult::pass();
    for module in modules {
        let r = module.should_ban_peer(torrent, peer, downloader);
        result = CheckResult::merge(result, r);
    }
    result
}

// 说明：规则引擎的全局 LangStore 由宿主进程启动时通过 pbh_core::rule::init_lang 注入。
