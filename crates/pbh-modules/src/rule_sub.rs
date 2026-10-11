//! IPBlackRuleList（网络 IP 集规则订阅）——上游同名模块的行为等价实现。
//!
//! - 订阅规则文件（默认 all-in-one → https://bcr.pbh-btn.com/combine/all.txt）
//! - 文件格式：`#`/`//` 行为累积注释（应用于其后的 IP 行），IP/CIDR 行可带行内注释；
//!   重复条目注释以 \n 合并（stringToIPList 语义）
//! - 下载到 <data>/sub/<ruleId>.txt；拉取失败回退本地缓存（IP_BAN_RULE_USE_CACHE）
//! - 内容无变化（字节一致）时跳过重建；变化时更新 rule_sub_info 并写 rule_sub_log
//! - 定时刷新（check-interval，默认 4h）
use crate::RuleModule;
use parking_lot::RwLock;
use pbh_config::ConfigHandle;
use pbh_core::check::{CheckResult, PeerAction};
use pbh_core::lang::LangStore;
use pbh_core::model::{Millis, Peer, Torrent};
use pbh_core::translate::TranslationComponent;
use pbh_db::Database;
use pbh_downloader::Downloader;
use serde_json::{Map, Value};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const CTX_IBRL: &str = "com.ghostchu.peerbanhelper.module.impl.rule.IPBlackRuleList";

/// 订阅规则定义（profile: module.ip-address-blocker-rules.rules.<id>）
#[derive(Clone, Debug)]
pub struct RuleSub {
    pub id: String,
    pub name: String,
    pub url: String,
    pub enabled: bool,
}

/// 高效 IP 匹配器：排序区间数组 + 二分（v4/u32、v6/u128 分开存储）
pub struct IpMatcher {
    pub rule_id: String,
    pub rule_name: String,
    v4: Vec<(u32, u32, u32)>, // (start, end, comment_idx)
    v6: Vec<(u128, u128, u32)>,
    comments: Vec<String>,
}

fn ip_to_u32(ip: std::net::Ipv4Addr) -> u32 {
    u32::from(ip)
}
fn ip_to_u128(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(v4) => u32::from(v4) as u128,
        IpAddr::V6(v6) => u128::from(v6),
    }
}

impl IpMatcher {
    /// 解析规则文本（stringToIPList + parseRuleLine 语义）
    pub fn parse(rule_id: &str, rule_name: &str, text: &str) -> Self {
        let mut v4: Vec<(u32, u32, u32)> = Vec::new();
        let mut v6: Vec<(u128, u128, u32)> = Vec::new();
        let mut comments: Vec<String> = Vec::new();
        let mut pending_comments: Vec<String> = Vec::new();
        let mut entries: Vec<(IpAddr, IpAddr, String)> = Vec::new();

        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r');
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix('#') {
                pending_comments.push(rest.trim().to_string());
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("//") {
                pending_comments.push(rest.trim().to_string());
                continue;
            }
            // IP 行：可带行内注释（空白或 # 分隔）
            let mut parts = trimmed.splitn(2, '#');
            let ip_part = parts.next().unwrap_or("").trim();
            let inline = parts.next().map(|s| s.trim().to_string());
            let ip_token = ip_part.split_whitespace().next().unwrap_or("");
            let mut comment_parts = pending_comments.clone();
            if let Some(mut rest) = inline {
                if !rest.is_empty() {
                    comment_parts.push(rest.trim().to_string());
                }
                let _ = &mut rest;
            }
            // 无 # 时，IP 后跟的其它文本也视为注释（parseRuleLine 宽松语义）
            let extra = ip_part.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
            if !extra.is_empty() {
                comment_parts.push(extra);
            }
            let comment = comment_parts.join("\n");
            pending_comments.clear();

            if let Some(net) = pbh_core::ipset::parse_cidr(ip_token) {
                let lo = net.network();
                let hi = net.broadcast();
                entries.push((lo, hi, comment));
            } else if !ip_token.is_empty() {
                tracing::debug!("Unable parse rule line: {}", ip_token);
            }
        }

        // 去重合并注释（同键注释以 \n 合并，对齐上游 trie put 语义）
        let mut merged: Vec<(IpAddr, IpAddr, String)> = Vec::with_capacity(entries.len());
        for (lo, hi, comment) in entries {
            if let Some(existing) = merged.iter_mut().rev().take(64).find(|(l, h, _)| *l == lo && *h == hi) {
                if !comment.is_empty() {
                    if existing.2.is_empty() {
                        existing.2 = comment;
                    } else {
                        existing.2.push('\n');
                        existing.2.push_str(&comment);
                    }
                }
            } else {
                merged.push((lo, hi, comment));
            }
        }

        for (lo, hi, comment) in merged {
            let idx = comments.len() as u32;
            comments.push(comment);
            match (lo, hi) {
                (IpAddr::V4(a), IpAddr::V4(b)) => v4.push((ip_to_u32(a), ip_to_u32(b), idx)),
                _ => v6.push((ip_to_u128(lo), ip_to_u128(hi), idx)),
            }
        }
        v4.sort_unstable_by_key(|x| x.0);
        v6.sort_unstable_by_key(|x| x.0);
        Self {
            rule_id: rule_id.to_string(),
            rule_name: rule_name.to_string(),
            v4,
            v6,
            comments,
        }
    }

    pub fn size(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    /// 返回命中条目的注释（无注释时返回空串）
    pub fn match_ip(&self, ip: IpAddr) -> Option<&str> {
        match ip {
            IpAddr::V4(v4) => {
                let key = u32::from(v4);
                let idx = self.v4.partition_point(|e| e.0 <= key);
                // 检查 idx 之前的若干条目（区间可能重叠；上游 trie 取最精确匹配，
                // 这里向前扫描有限窗口内包含该地址的条目）
                let start = idx.saturating_sub(32);
                for e in self.v4[start..idx].iter().rev() {
                    if e.0 <= key && key <= e.1 {
                        return Some(self.comments[e.2 as usize].as_str());
                    }
                    if e.1 < key && key - e.1 > 1 << 24 {
                        break;
                    }
                }
                None
            }
            IpAddr::V6(v6) => {
                let key = u128::from(v6);
                let idx = self.v6.partition_point(|e| e.0 <= key);
                let start = idx.saturating_sub(32);
                for e in self.v6[start..idx].iter().rev() {
                    if e.0 <= key && key <= e.1 {
                        return Some(self.comments[e.2 as usize].as_str());
                    }
                    if e.1 < key && key - e.1 > 1 << 96 {
                        break;
                    }
                }
                None
            }
        }
    }
}

pub struct IpBlackRuleList {
    ban_duration: i64,
    pub check_interval: i64,
    matchers: RwLock<Vec<Arc<IpMatcher>>>,
    db: Arc<Database>,
    sub_dir: PathBuf,
    handle: ConfigHandle,
    http: reqwest::Client,
    shutdown: Arc<AtomicBool>,
}

impl Drop for IpBlackRuleList {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl IpBlackRuleList {
    pub fn from_profile(
        cfg: &pbh_config::AppConfig,
        db: Arc<Database>,
        handle: &ConfigHandle,
    ) -> Option<Arc<Self>> {
        if !cfg.module_enabled("ip-address-blocker-rules", true) {
            return None;
        }
        let p = |k: &str| cfg.module_path("ip-address-blocker-rules", k);
        let me = Arc::new(Self {
            ban_duration: cfg.profile.get_i64(&p("ban-duration"), 0),
            check_interval: cfg.profile.get_i64(&p("check-interval"), 14400000),
            matchers: RwLock::new(Vec::new()),
            db,
            sub_dir: handle.data_dir.join("sub"),
            handle: handle.clone(),
            http: reqwest::Client::builder()
                .user_agent(concat!("PeerBanHelper-RS/", env!("CARGO_PKG_VERSION")))
                .build()
                .ok()?,
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        // 定时刷新任务（对应 registerScheduledTask(reloadConfig, 0, checkInterval)）
        {
            let me2 = me.clone();
            let shutdown = me.shutdown.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    loop {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        let interval = {
                            let cfg = me2.handle.get();
                            cfg.profile
                                .get_i64(&cfg.module_path("ip-address-blocker-rules", "check-interval"), 14400000)
                                .max(60000) as u64
                        };
                        me2.update_all("AUTO").await;
                        // 分片 sleep 以便及时响应 shutdown
                        let mut slept = 0u64;
                        while slept < interval {
                            if shutdown.load(Ordering::Relaxed) {
                                return;
                            }
                            let step = 1000u64.min(interval - slept);
                            tokio::time::sleep(std::time::Duration::from_millis(step)).await;
                            slept += step;
                        }
                    }
                });
            } else {
                tracing::warn!("No tokio runtime available; rule subscription auto-update disabled");
            }
        }
        Some(me)
    }

    pub fn rules_from_cfg(cfg: &pbh_config::AppConfig) -> Vec<RuleSub> {
        let mut out = Vec::new();
        for id in cfg.profile.section_keys("module.ip-address-blocker-rules.rules") {
            let base = format!("module.ip-address-blocker-rules.rules.{}", id);
            out.push(RuleSub {
                name: cfg.profile.get_str(&format!("{}.name", base), &id),
                url: cfg.profile.get_str(&format!("{}.url", base), ""),
                enabled: cfg.profile.get_bool(&format!("{}.enabled", base), false),
                id,
            });
        }
        out
    }

    /// 更新全部订阅规则；返回 (rule_id, 结果)
    pub async fn update_all(&self, update_type: &str) -> Vec<(String, Result<i64, String>)> {
        let cfg = self.handle.get();
        let rules = Self::rules_from_cfg(&cfg);
        let mut results = Vec::new();
        let mut active_ids: Vec<String> = Vec::new();
        for rule in &rules {
            if !rule.enabled {
                self.matchers.write().retain(|m| m.rule_id != rule.id);
                let _ = self.db.upsert_rule_sub_info(
                    &rule.id,
                    false,
                    &rule.name,
                    &rule.url,
                    Millis::now().millis(),
                    0,
                );
                results.push((rule.id.clone(), Ok(0)));
                continue;
            }
            active_ids.push(rule.id.clone());
            results.push((rule.id.clone(), self.update_rule(rule, update_type).await));
        }
        // 清理已移除规则的 matcher
        self.matchers.write().retain(|m| active_ids.contains(&m.rule_id));
        results
    }

    /// 更新单条规则（对应 updateRule）
    pub async fn update_rule(&self, rule: &RuleSub, update_type: &str) -> Result<i64, String> {
        let _ = std::fs::create_dir_all(&self.sub_dir);
        let file = self.sub_dir.join(format!("{}.txt", rule.id));

        let mut data: Option<Vec<u8>> = None;
        if !rule.url.is_empty() {
            match self.http.get(&rule.url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                    Ok(b) => data = Some(b.to_vec()),
                    Err(e) => tracing::warn!("[RuleSub:{}] read body failed: {}", rule.id, e),
                },
                Ok(resp) => tracing::warn!(
                    "[RuleSub:{}] download failed: HTTP {}",
                    rule.id,
                    resp.status()
                ),
                Err(e) => tracing::warn!("[RuleSub:{}] download failed: {}", rule.id, e),
            }
        }

        let from_cache;
        let text = match &data {
            Some(bytes) => {
                from_cache = false;
                // 与本地缓存比对（对应 sha256 比较），无变化则跳过重建
                if let Ok(existing) = std::fs::read(&file) {
                    if existing == *bytes
                        && self.matchers.read().iter().any(|m| m.rule_id == rule.id)
                    {
                        return Ok(self
                            .matchers
                            .read()
                            .iter()
                            .find(|m| m.rule_id == rule.id)
                            .map(|m| m.size() as i64)
                            .unwrap_or(0));
                    }
                }
                String::from_utf8_lossy(bytes).to_string()
            }
            None => {
                // 拉取失败 → 本地缓存回退（IP_BAN_RULE_USE_CACHE）
                match std::fs::read(&file) {
                    Ok(bytes) => {
                        from_cache = true;
                        tracing::warn!("[RuleSub:{}] using local cache: {}", rule.id, file.display());
                        String::from_utf8_lossy(&bytes).to_string()
                    }
                    Err(_) => return Err("download failed and no local cache".to_string()),
                }
            }
        };

        let matcher = Arc::new(IpMatcher::parse(&rule.id, &rule.name, &text));
        let count = matcher.size() as i64;
        {
            let mut list = self.matchers.write();
            list.retain(|m| m.rule_id != rule.id);
            list.push(matcher);
        }
        if !from_cache {
            if let Some(bytes) = &data {
                let _ = std::fs::write(&file, bytes);
            }
        }
        let now = Millis::now().millis();
        let _ = self
            .db
            .upsert_rule_sub_info(&rule.id, true, &rule.name, &rule.url, now, count);
        let _ = self.db.insert_rule_sub_log(&rule.id, now, count, update_type);
        tracing::info!(
            "[RuleSub] Rule {} updated: {} entries ({})",
            rule.name,
            count,
            if from_cache { "cache" } else { "remote" }
        );
        Ok(count)
    }

    pub fn matcher_count(&self) -> usize {
        self.matchers.read().len()
    }

    pub fn total_entries(&self) -> usize {
        self.matchers.read().iter().map(|m| m.size()).sum()
    }
}

impl RuleModule for IpBlackRuleList {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "IP Blacklist Rule List"
    }
    fn config_name(&self) -> &'static str {
        "ip-address-blocker-rules"
    }

    fn should_ban_peer(&self, _torrent: &Torrent, peer: &Peer, _dl: &dyn Downloader) -> CheckResult {
        if peer.handshaking {
            return CheckResult::handshaking();
        }
        let Some(ip) = peer.address.ip_addr() else {
            return CheckResult::pass();
        };
        let ip_str = &peer.address.ip;
        let matchers = self.matchers.read();
        for matcher in matchers.iter() {
            if let Some(comment) = matcher.match_ip(ip) {
                let comment_tc = if comment.is_empty() {
                    TranslationComponent::new("MODULE_IBL_COMMENT_UNKNOWN")
                } else {
                    TranslationComponent::new(comment.to_string())
                };
                let mut sd = Map::new();
                sd.insert("ruleName".into(), Value::String(matcher.rule_name.clone()));
                return CheckResult::new(
                    CTX_IBRL,
                    PeerAction::Ban,
                    self.ban_duration,
                    TranslationComponent::new(matcher.rule_name.clone()),
                    TranslationComponent::with(
                        "MODULE_IBL_MATCH_IP_RULE",
                        vec![
                            matcher.rule_name.clone().into(),
                            ip_str.clone().into(),
                            comment_tc.into(),
                        ],
                    ),
                    sd,
                );
            }
        }
        CheckResult::pass()
    }

    fn web_data(&self, _locale: &str, _lang: &LangStore) -> Value {
        let matchers = self.matchers.read();
        let list: Vec<Value> = matchers
            .iter()
            .map(|m| {
                serde_json::json!({
                    "ruleId": m.rule_id,
                    "ruleName": m.rule_name,
                    "size": m.size(),
                })
            })
            .collect();
        serde_json::json!({ "rules": list, "checkInterval": self.check_interval })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_match() {
        let text = "# Comment for next line\n1.2.3.0/24\n// another\n5.6.7.8 inline note\n9.9.9.9\n2001:db8::/32\n";
        let m = IpMatcher::parse("r1", "R1", text);
        assert_eq!(m.size(), 4);
        assert_eq!(m.match_ip("1.2.3.99".parse().unwrap()), Some("Comment for next line"));
        assert_eq!(m.match_ip("5.6.7.8".parse().unwrap()), Some("another\ninline note"));
        assert_eq!(m.match_ip("9.9.9.9".parse().unwrap()), Some(""));
        assert!(m.match_ip("1.2.4.1".parse().unwrap()).is_none());
        assert!(m.match_ip("2001:db8::1".parse().unwrap()).is_some());
    }

    #[test]
    fn duplicate_merges_comment() {
        let text = "# a\n1.1.1.1\n# b\n1.1.1.1\n";
        let m = IpMatcher::parse("r", "r", text);
        assert_eq!(m.size(), 1);
        assert_eq!(m.match_ip("1.1.1.1".parse().unwrap()), Some("a\nb"));
    }
}
