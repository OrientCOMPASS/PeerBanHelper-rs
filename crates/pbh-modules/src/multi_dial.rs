//! MultiDialingBlocker（多拨追猎）——上游同名模块的行为等价实现。
//!
//! 同一子网（IPv4 /24、IPv6 /56，可配）对同一种子出现超过 tolerate-num 个 IP 时，
//! 封禁当前 IP（同子网其余 IP 会在后续周期被逐一封禁）；keep-hunting 开启时，
//! 命中过的子网在 keep-hunting-time 内持续追猎新出现的 IP。
//!
//! 缓存语义对齐 Guava Cache：cache=expireAfterWrite(cache-lifespan)，
//! subnetCounter=expireAfterAccess，huntingList=expireAfterAccess(keep-hunting-time)，
//! 容量上限 TORRENT_PEER_MAX_NUM=1024 / PEER_MAX_NUM_PER_SUBNET=16。
use crate::RuleModule;
use parking_lot::Mutex;
use pbh_config::AppConfig;
use pbh_core::check::{CheckResult, PeerAction};
use pbh_core::lang::LangStore;
use pbh_core::model::{Millis, Peer, Torrent};
use pbh_core::translate::TranslationComponent;
use pbh_downloader::Downloader;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

pub const CTX_MDB: &str = "com.ghostchu.peerbanhelper.module.impl.rule.MultiDialingBlocker";
const TORRENT_PEER_MAX_NUM: usize = 1024;
const PEER_MAX_NUM_PER_SUBNET: usize = 16;

struct MdbConfig {
    ban_duration: i64,
    subnet_mask_length: u8,
    subnet_mask_v6_length: u8,
    tolerate_num_v4: i64,
    tolerate_num_v6: i64,
    cache_lifespan_ms: i64,
    keep_hunting: bool,
    keep_hunting_time_ms: i64,
}

#[derive(Default)]
struct Caches {
    /// torrentId@ip → last write ts（expireAfterWrite）
    cache: HashMap<String, i64>,
    /// torrentId@subnet → (peers: ip → last access ts, last access ts)
    subnet_counter: HashMap<String, (HashMap<String, i64>, i64)>,
    /// torrentId@subnet → last access ts（expireAfterAccess keepHuntingTime）
    hunting_list: HashMap<String, i64>,
}

pub struct MultiDialingBlocker {
    cfg: MdbConfig,
    caches: Mutex<Caches>,
}

impl MultiDialingBlocker {
    pub fn from_profile(cfg: &AppConfig) -> Option<Arc<dyn RuleModule>> {
        if !cfg.module_enabled("multi-dialing-blocker", true) {
            return None;
        }
        let p = |k: &str| cfg.module_path("multi-dialing-blocker", k);
        Some(Arc::new(Self {
            cfg: MdbConfig {
                ban_duration: cfg.profile.get_i64(&p("ban-duration"), 0),
                subnet_mask_length: cfg.profile.get_i64(&p("subnet-mask-length"), 24) as u8,
                subnet_mask_v6_length: cfg.profile.get_i64(&p("subnet-mask-v6-length"), 56) as u8,
                tolerate_num_v4: cfg.profile.get_i64(&p("tolerate-num-ipv4"), 2),
                tolerate_num_v6: cfg.profile.get_i64(&p("tolerate-num-ipv6"), 5),
                cache_lifespan_ms: cfg.profile.get_i64(&p("cache-lifespan"), 86400) * 1000,
                keep_hunting: cfg.profile.get_bool(&p("keep-hunting"), false),
                keep_hunting_time_ms: cfg.profile.get_i64(&p("keep-hunting-time"), 2592000) * 1000,
            },
            caches: Mutex::new(Caches::default()),
        }))
    }

    fn subnet_of(&self, ip: IpAddr) -> Option<String> {
        let prefix = match ip {
            IpAddr::V4(_) => self.cfg.subnet_mask_length,
            IpAddr::V6(_) => self.cfg.subnet_mask_v6_length,
        };
        let net = ipnet::IpNet::new(ip, prefix).ok()?;
        Some(net.trunc().to_string())
    }

    fn purge_expired(&self, c: &mut Caches, now: i64) {
        let life = self.cfg.cache_lifespan_ms;
        c.cache.retain(|_, ts| now - *ts < life);
        c.subnet_counter.retain(|_, (_, ts)| now - *ts < life);
        for (_, (peers, _)) in c.subnet_counter.iter_mut() {
            peers.retain(|_, ts| now - *ts < life);
        }
        let hunt = self.cfg.keep_hunting_time_ms;
        c.hunting_list.retain(|_, ts| now - *ts < hunt);
    }
}

impl RuleModule for MultiDialingBlocker {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "Multi Dialing Blocker"
    }
    fn config_name(&self) -> &'static str {
        "multi-dialing-blocker"
    }

    fn should_ban_peer(&self, torrent: &Torrent, peer: &Peer, _dl: &dyn Downloader) -> CheckResult {
        if peer.handshaking {
            return CheckResult::handshaking();
        }
        let Some(ip) = peer.address.ip_addr() else {
            return CheckResult::pass();
        };
        let Some(subnet) = self.subnet_of(ip) else {
            return CheckResult::pass();
        };
        let ip_str = pbh_core::model::compress_ip(ip);
        let now = Millis::now().millis();

        let mut c = self.caches.lock();
        self.purge_expired(&mut c, now);

        let torrent_ip = format!("{}@{}", torrent.id, ip_str);
        // 容量控制（对应 maximumSize）：超限时清除最旧条目
        if c.cache.len() >= TORRENT_PEER_MAX_NUM {
            if let Some(oldest) = c.cache.iter().min_by_key(|(_, ts)| **ts).map(|(k, _)| k.clone()) {
                c.cache.remove(&oldest);
            }
        }
        c.cache.insert(torrent_ip, now);

        let torrent_subnet = format!("{}@{}", torrent.id, subnet);
        let entry = c.subnet_counter.entry(torrent_subnet.clone()).or_default();
        if entry.0.len() >= PEER_MAX_NUM_PER_SUBNET && !entry.0.contains_key(&ip_str) {
            if let Some(oldest) = entry.0.iter().min_by_key(|(_, ts)| **ts).map(|(k, _)| k.clone()) {
                entry.0.remove(&oldest);
            }
        }
        entry.0.insert(ip_str.clone(), now);
        entry.1 = now;
        let subnet_peers_size = entry.0.len() as i64;

        let tolerate = match ip {
            IpAddr::V4(_) => self.cfg.tolerate_num_v4,
            IpAddr::V6(_) => self.cfg.tolerate_num_v6,
        };
        if subnet_peers_size > tolerate {
            c.hunting_list.insert(torrent_subnet.clone(), now);
            let mut sd = Map::new();
            sd.insert("subnetPeersSize".into(), Value::Number(subnet_peers_size.into()));
            sd.insert("subnet".into(), Value::String(torrent_subnet.clone()));
            return CheckResult::new(
                CTX_MDB,
                PeerAction::Ban,
                self.cfg.ban_duration,
                TranslationComponent::new("MDB_MULTI_DIALING_DETECTED"),
                TranslationComponent::strs(
                    "MODULE_MDB_MULTI_DIALING_DETECTED",
                    vec![subnet.clone(), ip_str.clone()],
                ),
                sd,
            );
        }

        if self.cfg.keep_hunting {
            if let Some(ts) = c.hunting_list.get(&torrent_subnet).copied() {
                if now - ts < self.cfg.keep_hunting_time_ms {
                    c.hunting_list.insert(torrent_subnet.clone(), now);
                    let mut sd = Map::new();
                    sd.insert("subnetPeersSize".into(), Value::Number(subnet_peers_size.into()));
                    sd.insert("subnet".into(), Value::String(torrent_subnet));
                    return CheckResult::new(
                        CTX_MDB,
                        PeerAction::Ban,
                        self.cfg.ban_duration,
                        TranslationComponent::new("MDB_MULTI_HUNTING"),
                        TranslationComponent::strs(
                            "MODULE_MDB_MULTI_DIALING_HUNTING_TRIGGERED",
                            vec![subnet, ip_str],
                        ),
                        sd,
                    );
                } else {
                    c.hunting_list.remove(&torrent_subnet);
                }
            }
        }
        CheckResult::pass()
    }

    fn on_wave_complete(&self) {
        let mut c = self.caches.lock();
        self.purge_expired(&mut c, Millis::now().millis());
    }

    fn web_data(&self, _locale: &str, _lang: &LangStore) -> Value {
        serde_json::json!({
            "subnetMaskLength": self.cfg.subnet_mask_length,
            "subnetMaskV6Length": self.cfg.subnet_mask_v6_length,
            "tolerateNumV4": self.cfg.tolerate_num_v4,
            // 上游 handleConfig 的键名 typo（tolerateNumV5）原样保留
            "tolerateNumV5": self.cfg.tolerate_num_v6,
            "cacheLifespan": self.cfg.cache_lifespan_ms / 1000,
            "keepHunting": self.cfg.keep_hunting,
            "keepHuntingTime": self.cfg.keep_hunting_time_ms / 1000,
        })
    }
}
