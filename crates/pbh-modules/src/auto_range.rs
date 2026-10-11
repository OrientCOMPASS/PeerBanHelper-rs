//! AutoRangeBan（自动连锁封禁）——上游同名模块的行为等价实现。
//! 若某 Peer 的地址落在任一已封禁地址的前缀块（IPv4 /30、IPv6 /48，可配）内，
//! 则连锁封禁该 Peer。
use crate::RuleModule;
use ipnet::IpNet;
use pbh_config::AppConfig;
use pbh_core::banlist::BanListHandle;
use pbh_core::check::{CheckResult, PeerAction};
use pbh_core::lang::LangStore;
use pbh_core::model::{compress_ip, host_net, Millis, Peer, Torrent};
use pbh_core::translate::TranslationComponent;
use pbh_downloader::Downloader;
use serde_json::{Map, Value};
use std::net::IpAddr;
use std::sync::Arc;

pub const CTX_ARB: &str = "com.ghostchu.peerbanhelper.module.impl.rule.AutoRangeBan";

pub struct AutoRangeBan {
    ban_duration: i64,
    ipv4_prefix: u8,
    ipv6_prefix: u8,
    banlist: BanListHandle,
}

impl AutoRangeBan {
    pub fn from_profile(cfg: &AppConfig, banlist: BanListHandle) -> Option<Arc<dyn RuleModule>> {
        if !cfg.module_enabled("auto-range-ban", true) {
            return None;
        }
        let p = |k: &str| cfg.module_path("auto-range-ban", k);
        Some(Arc::new(Self {
            ban_duration: cfg.profile.get_i64(&p("ban-duration"), 0),
            ipv4_prefix: cfg.profile.get_i64(&p("ipv4"), 30) as u8,
            ipv6_prefix: cfg.profile.get_i64(&p("ipv6"), 48) as u8,
            banlist,
        }))
    }

    /// v4-mapped v6 → v4（对应 IPAddress.isIPv4Convertible/toIPv4）
    fn normalize(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            other => other,
        }
    }
}

impl RuleModule for AutoRangeBan {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "Auto Range Ban"
    }
    fn config_name(&self) -> &'static str {
        "auto-range-ban"
    }

    fn should_ban_peer(&self, _torrent: &Torrent, peer: &Peer, _dl: &dyn Downloader) -> CheckResult {
        if peer.handshaking {
            // 注意：上游此处返回 pass() 而非 handshaking()
            return CheckResult::pass();
        }
        let Some(raw_ip) = peer.address.ip_addr() else {
            return CheckResult::pass();
        };
        let peer_addr = Self::normalize(raw_ip);
        let peer_host = host_net(peer_addr);
        {
            let list = self.banlist.read();
            if list.contains(&peer_host) || list.contains_addr(peer_addr) {
                return CheckResult::pass();
            }
            let is_v4 = peer_addr.is_ipv4();
            for (banned_addr, meta) in list.iter() {
                if meta.ban_for_disconnect {
                    continue;
                }
                let banned_ip = Self::normalize(banned_addr.addr());
                if is_v4 != banned_ip.is_ipv4() {
                    continue;
                }
                let (address_type, banned_cidr): (String, IpNet) = match banned_ip {
                    IpAddr::V4(_) => (
                        format!("IPv4/{}", self.ipv4_prefix),
                        match IpNet::new(banned_ip, self.ipv4_prefix) {
                            Ok(n) => n.trunc(),
                            Err(_) => continue,
                        },
                    ),
                    IpAddr::V6(_) => (
                        format!("IPv6/{}", self.ipv6_prefix),
                        match IpNet::new(banned_ip, self.ipv6_prefix) {
                            Ok(n) => n.trunc(),
                            Err(_) => continue,
                        },
                    ),
                };
                if banned_cidr.contains(&peer_addr) {
                    let mut sd = Map::new();
                    sd.insert(
                        "relatedBannedAddress".into(),
                        Value::String(compress_ip(banned_ip)),
                    );
                    let _ = Millis::now();
                    return CheckResult::new(
                        CTX_ARB,
                        PeerAction::Ban,
                        self.ban_duration,
                        TranslationComponent::new(address_type.clone()),
                        TranslationComponent::strs(
                            "ARB_BANNED",
                            vec![
                                compress_ip(peer_addr),
                                banned_addr.to_string(),
                                banned_cidr.to_string(),
                                address_type,
                            ],
                        ),
                        sd,
                    );
                }
            }
        }
        CheckResult::pass()
    }

    fn web_data(&self, _locale: &str, _lang: &LangStore) -> Value {
        serde_json::json!({
            "ipv4-prefix": self.ipv4_prefix,
            "ipv6-prefix": self.ipv6_prefix,
        })
    }
}
