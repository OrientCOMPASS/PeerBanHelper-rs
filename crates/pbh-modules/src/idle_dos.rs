//! IdleConnectionDosProtection（空闲连接 DoS 保护）——上游同名模块的行为等价实现。
//! 默认禁用（enabled: false）。当 Peer 长时间低速/无速且状态无变化时视为空闲连接并封禁。
//!
//! bug-compat 说明：上游 percentageChange = |progress*100 - stored_percentage|，而
//! stored_percentage 存的是原始 progress（0-1），单位不一致属上游历史行为，此处原样复刻。
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
use std::sync::Arc;

pub const CTX_ICDP: &str =
    "com.ghostchu.peerbanhelper.module.impl.rule.IdleConnectionDosProtection";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProtectionMode {
    DeterminedByPeerFlags,
    AlwaysSeeding,
    AlwaysSeedingAndDownloading,
}

#[derive(Clone)]
struct ConnectionInfo {
    idle_start_time: i64,
    percentage: f64,
    uploaded: i64,
    downloaded: i64,
}

struct IcdpConfig {
    ban_duration: i64,
    max_allowed_idle_time: i64,
    idle_speed_threshold: i64,
    min_status_change_percentage: f64,
    reset_on_status_change: bool,
    protection_mode: ProtectionMode,
}

pub struct IdleConnectionDosProtection {
    cfg: IcdpConfig,
    idle_connections: Mutex<HashMap<(String, u16), ConnectionInfo>>,
}

/// libtorrent 标准 flags 字符串的解析子集（qB 报告的 "d u e p" 风格）
fn flag_interesting(flags: &str) -> bool {
    flags.split_whitespace().any(|f| f == "d")
}
fn flag_remote_interested(flags: &str) -> bool {
    flags.split_whitespace().any(|f| f == "D")
}

impl IdleConnectionDosProtection {
    pub fn from_profile(cfg: &AppConfig) -> Option<Arc<dyn RuleModule>> {
        if !cfg.module_enabled("idle-connection-dos-protection", false) {
            return None;
        }
        let p = |k: &str| cfg.module_path("idle-connection-dos-protection", k);
        let mode = match cfg.profile.get_i64(&p("protect-mode"), 0) {
            1 => ProtectionMode::AlwaysSeeding,
            2 => ProtectionMode::AlwaysSeedingAndDownloading,
            _ => ProtectionMode::DeterminedByPeerFlags,
        };
        Some(Arc::new(Self {
            cfg: IcdpConfig {
                ban_duration: cfg.profile.get_i64(&p("ban-duration"), 0),
                max_allowed_idle_time: cfg.profile.get_i64(&p("max-allowed-idle-time"), 300000),
                idle_speed_threshold: cfg.profile.get_i64(&p("idle-speed-threshold"), 64),
                min_status_change_percentage: cfg
                    .profile
                    .get(&p("min-status-change-percentage"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.001),
                reset_on_status_change: cfg.profile.get_bool(&p("reset-on-status-change"), true),
                protection_mode: mode,
            },
            idle_connections: Mutex::new(HashMap::new()),
        }))
    }
}

impl RuleModule for IdleConnectionDosProtection {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "Idle Connection DoS Protection"
    }
    fn config_name(&self) -> &'static str {
        "idle-connection-dos-protection"
    }

    fn should_ban_peer(&self, torrent: &Torrent, peer: &Peer, _dl: &dyn Downloader) -> CheckResult {
        let seeding = torrent.progress >= 1.0;
        if self.cfg.protection_mode == ProtectionMode::AlwaysSeeding && !seeding {
            return CheckResult::pass();
        }
        if self.cfg.protection_mode == ProtectionMode::DeterminedByPeerFlags && !seeding {
            let Some(flags) = peer.flags.as_deref() else {
                return CheckResult::pass(); // 不支持 Peer Flags 的下载器不保护下载任务
            };
            // 对应开关 pbh.module.idle-connection-dos-protection.ignore-if-any-interested（默认 true）
            if flag_interesting(flags) || flag_remote_interested(flags) {
                return CheckResult::pass();
            }
        }

        let key = (peer.address.ip.clone(), peer.address.port);
        let now = Millis::now().millis();

        if peer.upload_speed > self.cfg.idle_speed_threshold
            || peer.download_speed > self.cfg.idle_speed_threshold
        {
            self.idle_connections.lock().remove(&key);
            return CheckResult::pass();
        }

        let mut map = self.idle_connections.lock();
        let info = map
            .entry(key.clone())
            .or_insert_with(|| ConnectionInfo {
                idle_start_time: now,
                percentage: peer.progress, // bug-compat：存原始 progress
                uploaded: peer.uploaded,
                downloaded: peer.downloaded,
            })
            .clone();

        let computed_avg_upload =
            (peer.uploaded - info.uploaded) / (now - info.idle_start_time + 1).max(1);
        let computed_avg_download =
            (peer.downloaded - info.downloaded) / (now - info.idle_start_time + 1).max(1);
        let percentage_change = (peer.progress * 100.0 - info.percentage).abs();

        if computed_avg_upload > self.cfg.idle_speed_threshold
            || computed_avg_download > self.cfg.idle_speed_threshold
        {
            map.remove(&key);
            return CheckResult::pass();
        }
        if self.cfg.reset_on_status_change
            && percentage_change >= self.cfg.min_status_change_percentage
        {
            map.remove(&key);
            return CheckResult::pass();
        }
        let already_idled = now - info.idle_start_time;
        if already_idled > self.cfg.max_allowed_idle_time {
            map.remove(&key);
            let host_and_port = format!("{}:{}", peer.address.ip, peer.address.port);
            let mut sd = Map::new();
            sd.insert(
                "ip".into(),
                Value::String(format!(
                    "{}:{}",
                    peer.address.ip, peer.address.port
                )),
            );
            sd.insert("idle_type".into(), Value::String("timeout".into()));
            sd.insert("idle_start".into(), Value::Number(info.idle_start_time.into()));
            sd.insert(
                "last_percentage".into(),
                serde_json::Number::from_f64(info.percentage)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
            sd.insert("last_uploaded".into(), Value::Number(info.uploaded.into()));
            sd.insert("last_downloaded".into(), Value::Number(info.downloaded.into()));
            sd.insert(
                "percentage".into(),
                serde_json::Number::from_f64(peer.progress)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
            sd.insert("uploaded".into(), Value::Number(peer.uploaded.into()));
            sd.insert("downloaded".into(), Value::Number(peer.downloaded.into()));
            sd.insert("upload_speed".into(), Value::Number(peer.upload_speed.into()));
            sd.insert("download_speed".into(), Value::Number(peer.download_speed.into()));
            return CheckResult::new(
                CTX_ICDP,
                PeerAction::Ban,
                self.cfg.ban_duration,
                TranslationComponent::new("MODULE_ICDP_RULE_TITLE"),
                TranslationComponent::strs("MODULE_ICDP_RULE_DESCRIPTION", vec![host_and_port]),
                sd,
            );
        }
        CheckResult::pass()
    }

    fn web_data(&self, _locale: &str, _lang: &LangStore) -> Value {
        serde_json::json!({
            "maxAllowedIdleTime": self.cfg.max_allowed_idle_time,
            "idleSpeedThreshold": self.cfg.idle_speed_threshold,
            "protectMode": self.cfg.protection_mode as u8,
        })
    }
}
