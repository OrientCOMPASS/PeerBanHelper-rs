//! AntiVampire（任务反吸血预设）——上游同名模块的行为等价实现。
//! 当前实现迅雷预设：
//! - 非 0019 版本的迅雷客户端一律封禁（任何版本的迅雷都不会做种）
//! - 0019 版本迅雷仅在本地任务处于做种状态时封禁（下载中会正常参与分享，放行）
use crate::RuleModule;
use pbh_config::AppConfig;
use pbh_core::check::{CheckResult, PeerAction};
use pbh_core::lang::LangStore;
use pbh_core::model::{Peer, Torrent};
use pbh_core::translate::TranslationComponent;
use pbh_downloader::Downloader;
use serde_json::{Map, Value};
use std::sync::Arc;

pub const CTX_AV: &str = "com.ghostchu.peerbanhelper.module.impl.rule.AntiVampire";

pub struct AntiVampire {
    ban_duration: i64,
    xunlei_preset: bool,
}

impl AntiVampire {
    pub fn from_profile(cfg: &AppConfig) -> Option<Arc<dyn RuleModule>> {
        if !cfg.module_enabled("anti-vampire", true) {
            return None;
        }
        let p = |k: &str| cfg.module_path("anti-vampire", k);
        Some(Arc::new(Self {
            ban_duration: cfg.profile.get_i64(&p("ban-duration"), 0),
            xunlei_preset: cfg.profile.get_bool(&p("presets.xunlei.enabled"), true),
        }))
    }

    fn check_xunlei(&self, torrent: &Torrent, peer: &Peer) -> Option<CheckResult> {
        let mut is_xunlei = false;
        let mut is_xunlei_0019 = false;
        if let Some(pid) = &peer.peer_id {
            let pid = pid.to_lowercase();
            if pid.starts_with("-xl") {
                is_xunlei = true;
                if pid.starts_with("-xl0019") {
                    is_xunlei_0019 = true;
                }
            }
        }
        if let Some(cn) = &peer.client_name {
            let cn = cn.to_lowercase();
            if cn.starts_with("xunlei") {
                is_xunlei = true;
                if cn.contains("0019") || cn.contains("0.0.1.9") {
                    is_xunlei_0019 = true;
                }
            }
        }
        if !is_xunlei {
            return None;
        }
        let seeding = torrent.progress >= 1.0;
        if !is_xunlei_0019 {
            let mut sd = Map::new();
            sd.insert("xunleiType".into(), Value::String("non-0019".into()));
            sd.insert("seeding".into(), Value::Bool(seeding));
            return Some(CheckResult::new(
                CTX_AV,
                PeerAction::Ban,
                self.ban_duration,
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_TITLE"),
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_DESCRIPTION_XUNLEI_NON_0019"),
                sd,
            ));
        }
        if seeding {
            let mut sd = Map::new();
            sd.insert("xunleiType".into(), Value::String("0019".into()));
            sd.insert("seeding".into(), Value::Bool(seeding));
            return Some(CheckResult::new(
                CTX_AV,
                PeerAction::Ban,
                self.ban_duration,
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_TITLE"),
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_DESCRIPTION_XUNLEI_0019_SEEDING"),
                sd,
            ));
        }
        None
    }
}

impl RuleModule for AntiVampire {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "Anti Vampire"
    }
    fn config_name(&self) -> &'static str {
        "anti-vampire"
    }

    fn should_ban_peer(&self, torrent: &Torrent, peer: &Peer, _dl: &dyn Downloader) -> CheckResult {
        if self.xunlei_preset {
            if let Some(r) = self.check_xunlei(torrent, peer) {
                return r;
            }
        }
        CheckResult::pass()
    }

    fn web_data(&self, _locale: &str, _lang: &LangStore) -> Value {
        serde_json::json!({
            "presets": { "xunlei": { "enabled": self.xunlei_preset } },
        })
    }
}
