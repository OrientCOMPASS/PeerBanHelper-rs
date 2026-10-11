//! ProgressCheatBlocker（假进度检查器）——上游 module/impl/rule/ProgressCheatBlocker.java
//! 的行为等价实现。
//!
//! 检测维度（按上游执行顺序）：
//! 1. fastPcbTest：首次达到 fast-pcb-test-percentage 上传量时短暂 BAN_FOR_DISCONNECT 断连，
//!    促使对方重新汇报进度（需下载器支持 UNBAN_IP 特性）
//! 2. excessiveClient：累计下载超过种子大小（或已完成部分）× excessive-threshold
//! 3. differenceTest：真实进度（对端累计下载推算）与汇报进度差 > maximum-difference，
//!    经 max-wait-duration 延迟窗口确认后封禁
//! 4. progressRewind：进度回退超过 rewind-maximum-difference
//!
//! 持久化：pcb_address（IP:Port 维度）与 pcb_range（IP 前缀维度）双实体，
//! tracking_uploaded_increase_total 取「下载器报告 / IP 增量累计 / 段增量累计」三者最大，
//! 对抗「缓慢失忆攻击」。enable-persist=false 时仅内存。
use crate::{PcbConfig, RuleModule};
use pbh_core::check::{CheckResult, PeerAction};
use pbh_core::context;
use pbh_core::model::{Millis, Peer, Torrent};
use pbh_core::translate::TranslationComponent;
use pbh_db::{Database, PcbRow};
use pbh_downloader::Downloader;
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

pub const CTX_PCB: &str = "com.ghostchu.peerbanhelper.module.impl.rule.ProgressCheatBlocker";

#[derive(Clone)]
struct Entity {
    row: PcbRow,
    dirty: bool,
}

impl Entity {
    fn new(now: Millis, downloader: &str) -> Self {
        Entity {
            row: PcbRow {
                id: None,
                last_report_progress: 0.0,
                last_report_uploaded: 0,
                tracking_uploaded_increase_total: 0,
                rewind_counter: 0,
                progress_difference_counter: 0,
                first_time_seen: now.millis(),
                last_time_seen: now.millis(),
                downloader: downloader.to_string(),
                ban_delay_window_end_at: 0,
                fast_pcb_test_execute_at: 0,
                last_torrent_completed_size: 0,
            },
            dirty: true,
        }
    }
}

type AddrKey = (String, String, String, i64); // downloader, torrent_id, ip, port
type RangeKey = (String, String, String); // downloader, torrent_id, range

pub struct PcbStore {
    db: Arc<Database>,
    persist: bool,
    addr: Mutex<HashMap<AddrKey, Entity>>,
    range: Mutex<HashMap<RangeKey, Entity>>,
}

impl PcbStore {
    pub fn new(db: Arc<Database>, persist: bool) -> Self {
        Self {
            db,
            persist,
            addr: Mutex::new(HashMap::new()),
            range: Mutex::new(HashMap::new()),
        }
    }

    fn load_addr(&self, key: &AddrKey, now: Millis) -> Entity {
        if self.persist {
            if let Ok(Some(row)) = self.db.fetch_pcb_addr(&key.1, &key.2, key.3, &key.0) {
                return Entity { row, dirty: false };
            }
        }
        Entity::new(now, &key.0)
    }

    fn load_range(&self, key: &RangeKey, now: Millis) -> Entity {
        if self.persist {
            if let Ok(Some(row)) = self.db.fetch_pcb_range(&key.1, &key.2, &key.0) {
                return Entity { row, dirty: false };
            }
        }
        Entity::new(now, &key.0)
    }

    /// 对应 PBHCache 的批量回写（此处每轮 BanWave 结束时同步 flush）
    pub fn flush(&self) {
        if !self.persist {
            return;
        }
        {
            let mut cache = self.addr.lock();
            cache.retain(|key, ent| {
                if ent.dirty {
                    match self.db.upsert_pcb_addr((&key.1, &key.2, key.3), &ent.row) {
                        Ok(id) => {
                            ent.row.id = Some(id);
                            ent.dirty = false;
                        }
                        Err(e) => tracing::warn!("PCB addr flush failed: {}", e),
                    }
                }
                true
            });
        }
        {
            let mut cache = self.range.lock();
            cache.retain(|key, ent| {
                if ent.dirty {
                    match self.db.upsert_pcb_range((&key.1, &key.2), &ent.row) {
                        Ok(id) => {
                            ent.row.id = Some(id);
                            ent.dirty = false;
                        }
                        Err(e) => tracing::warn!("PCB range flush failed: {}", e),
                    }
                }
                true
            });
        }
    }

    pub fn cleanup_expired(&self, persist_duration_ms: i64) {
        if !self.persist {
            return;
        }
        let cutoff = Millis::now().millis() - persist_duration_ms;
        match self.db.cleanup_pcb(cutoff) {
            Ok(n) if n > 0 => {
                tracing::info!("[PCB] Deleted {} expired data records", n);
                // 同步清缓存
                let mut a = self.addr.lock();
                a.retain(|_, e| e.row.last_time_seen >= cutoff);
                let mut r = self.range.lock();
                r.retain(|_, e| e.row.last_time_seen >= cutoff);
            }
            Ok(_) => {}
            Err(e) => tracing::error!("[PCB] cleanup failed: {}", e),
        }
    }

    pub fn on_peer_unban(&self, torrent_id: &str, ip: &str) {
        {
            let mut a = self.addr.lock();
            a.retain(|k, _| !(k.1 == torrent_id && k.2 == ip));
        }
        if self.persist {
            if let Err(e) = self.db.delete_pcb_addr(torrent_id, ip) {
                tracing::warn!("[PCB] delete addr entry failed: {}", e);
            }
        }
    }
}

pub struct ProgressCheatBlocker {
    cfg: PcbConfig,
    store: Arc<PcbStore>,
}

impl ProgressCheatBlocker {
    pub fn new(cfg: PcbConfig, db: Arc<Database>) -> Arc<Self> {
        let store = Arc::new(PcbStore::new(db, cfg.enable_persist));
        let me = Arc::new(Self { cfg, store });
        // 启动时清理一次过期数据（对应上游 scheduleWithFixedDelay(0, 8h)）
        {
            let store2 = me.store.clone();
            let pd = me.cfg.persist_duration;
            std::thread::spawn(move || store2.cleanup_expired(pd));
        }
        me
    }

    pub fn store(&self) -> &Arc<PcbStore> {
        &self.store
    }

    fn prefix_string(&self, ip: IpAddr) -> Option<String> {
        let prefix = match ip {
            IpAddr::V4(_) => self.cfg.ipv4_prefix_length,
            IpAddr::V6(_) => self.cfg.ipv6_prefix_length,
        };
        let net = ipnet::IpNet::new(ip, prefix).ok()?;
        Some(net.trunc().to_string())
    }
}

fn percent(d: f64) -> String {
    // DecimalFormat("0.00%")，HALF_EVEN；此处用 HALF_UP 近似（差异仅在第 4 位小数的舍入）
    format!("{:.2}%", d * 100.0)
}

fn is_uploading_to_peer(peer: &Peer) -> bool {
    peer.upload_speed > 0 || peer.uploaded > 0
}

impl ProgressCheatBlocker {
    pub fn check(&self, torrent: &Torrent, peer: &Peer, downloader: &dyn Downloader) -> CheckResult {
        if peer.handshaking {
            return CheckResult::handshaking();
        }
        let Some(ip) = peer.address.ip_addr() else {
            return CheckResult::pass();
        };
        let Some(prefix) = self.prefix_string(ip) else {
            return CheckResult::pass();
        };
        let ip_str = pbh_core::model::compress_ip(ip);
        let now = Millis::now();

        let addr_key: AddrKey = (
            downloader.id().to_string(),
            torrent.id.clone(),
            ip_str.clone(),
            peer.address.port as i64,
        );
        let range_key: RangeKey = (
            downloader.id().to_string(),
            torrent.id.clone(),
            prefix,
        );

        let mut addr = {
            let mut cache = self.store.addr.lock();
            cache
                .entry(addr_key.clone())
                .or_insert_with(|| self.store.load_addr(&addr_key, now))
                .clone()
        };
        let mut range = {
            let mut cache = self.store.range.lock();
            cache
                .entry(range_key.clone())
                .or_insert_with(|| self.store.load_range(&range_key, now))
                .clone()
        };

        let result = self.check_inner(torrent, peer, downloader, &mut addr, &mut range);

        // finally 块：无论如何更新缓存实体（对齐上游）
        addr.row.last_report_uploaded = peer.uploaded;
        if peer.progress != 0.0 {
            addr.row.last_report_progress = peer.progress;
            range.row.last_report_progress = peer.progress;
        }
        addr.row.last_torrent_completed_size =
            std::cmp::max(torrent.completed_size, addr.row.last_torrent_completed_size);
        addr.row.last_time_seen = now.millis();
        range.row.last_report_uploaded = peer.uploaded;
        range.row.last_torrent_completed_size =
            std::cmp::max(torrent.completed_size, range.row.last_torrent_completed_size);
        range.row.last_time_seen = now.millis();
        addr.dirty = true;
        range.dirty = true;
        {
            self.store.addr.lock().insert(addr_key, addr);
            self.store.range.lock().insert(range_key, range);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn check_inner(
        &self,
        torrent: &Torrent,
        peer: &Peer,
        downloader: &dyn Downloader,
        addr: &mut Entity,
        range: &mut Entity,
    ) -> CheckResult {
        let ae = &mut addr.row;
        let re = &mut range.row;

        let computed_uploaded_incremental = if peer.uploaded < ae.last_report_uploaded {
            peer.uploaded
        } else {
            peer.uploaded - ae.last_report_uploaded
        };
        ae.tracking_uploaded_increase_total += computed_uploaded_incremental;
        re.tracking_uploaded_increase_total += computed_uploaded_incremental;
        let computed_uploaded = std::cmp::max(
            peer.uploaded,
            std::cmp::max(ae.tracking_uploaded_increase_total, re.tracking_uploaded_increase_total),
        );

        let torrent_size = torrent.size;
        let completed_size = torrent.completed_size;
        let computed_completed_size = std::cmp::max(
            completed_size,
            std::cmp::max(re.last_torrent_completed_size, ae.last_torrent_completed_size),
        );

        if torrent_size <= 0 {
            return CheckResult::pass();
        }
        if !is_uploading_to_peer(peer) {
            return CheckResult::pass();
        }

        let mut sd = Map::new();
        let add = |sd: &mut Map<String, Value>, k: &str, v: Value| {
            sd.insert(k.to_string(), v);
        };
        add(&mut sd, "torrentSize", json_n(torrent_size));
        add(&mut sd, "completedSize", json_n(completed_size));
        add(&mut sd, "computedCompletedSize", json_n(computed_completed_size));
        add(&mut sd, "peerReportUploaded", json_n(peer.uploaded));
        add(&mut sd, "peerLastReportUploaded", json_n(ae.last_report_uploaded));
        add(&mut sd, "prefixLastReportUploaded", json_n(re.last_report_uploaded));
        add(&mut sd, "peerTrackingUploadedIncreaseTotal", json_n(ae.tracking_uploaded_increase_total));
        add(&mut sd, "prefixTrackingUploadedIncreaseTotal", json_n(re.tracking_uploaded_increase_total));
        add(&mut sd, "actualUploaded", json_n(computed_uploaded));
        add(&mut sd, "uploadedIncremental", json_n(computed_uploaded_incremental));
        add(&mut sd, "fileTooSmall", Value::Bool(self.file_too_small(torrent_size)));
        add(&mut sd, "fastPcbTestPercentage", json_f(self.cfg.fast_test_percentage));

        // 1. 快速 PCB 测试
        if let Some(r) = self.fast_pcb_test(addr, range, computed_uploaded, torrent_size, &mut sd, downloader) {
            return r;
        }

        let computed_progress = computed_uploaded as f64 / torrent_size as f64;
        let client_reported_progress = peer.progress;
        add(&mut sd, "computedProgress", json_f(computed_progress));
        add(&mut sd, "clientReportedProgress", json_f(client_reported_progress));

        // 2. 过量下载
        if let Some(r) = self.excessive_client(
            computed_uploaded,
            torrent_size,
            &mut sd,
            range,
            addr,
            completed_size,
            computed_completed_size,
        ) {
            return r;
        }

        if computed_progress <= client_reported_progress {
            return CheckResult::pass();
        }

        // 3. 进度差异
        if let Some(r) = self.difference_test(
            range,
            addr,
            client_reported_progress,
            computed_progress,
            &mut sd,
            torrent_size,
            peer,
        ) {
            return r;
        }

        // 4. 进度回退
        if let Some(r) = self.progress_rewind(
            peer,
            &mut sd,
            range,
            addr,
            client_reported_progress,
            computed_progress,
            torrent_size,
        ) {
            return r;
        }

        CheckResult::pass()
    }

    fn file_too_small(&self, torrent_size: i64) -> bool {
        torrent_size < self.cfg.minimum_size
    }

    fn fast_pcb_test(
        &self,
        addr: &mut Entity,
        range: &mut Entity,
        computed_uploaded: i64,
        torrent_size: i64,
        sd: &mut Map<String, Value>,
        downloader: &dyn Downloader,
    ) -> Option<CheckResult> {
        if self.cfg.fast_test_percentage > 0.0
            && !self.file_too_small(torrent_size)
            && downloader.has_feature("UNBAN_IP")
        {
            if addr.row.fast_pcb_test_execute_at == 0 || range.row.fast_pcb_test_execute_at == 0 {
                if (computed_uploaded as f64) >= self.cfg.fast_test_percentage * torrent_size as f64 {
                    let now = Millis::now().millis();
                    addr.row.fast_pcb_test_execute_at = now;
                    range.row.fast_pcb_test_execute_at = now;
                    sd.insert("type".into(), Value::String("fastPcbTest".into()));
                    return Some(CheckResult::new(
                        CTX_PCB,
                        PeerAction::BanForDisconnect,
                        self.cfg.fast_test_block_duration,
                        TranslationComponent::new("PCB_RULE_PEER_PROGRESS_CHEAT_TESTING"),
                        TranslationComponent::new("PCB_DESCRIPTION_PEER_PROGRESS_CHEAT_TESTING"),
                        sd.clone(),
                    ));
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn excessive_client(
        &self,
        computed_uploaded: i64,
        torrent_size: i64,
        sd: &mut Map<String, Value>,
        range: &mut Entity,
        addr: &mut Entity,
        completed_size: i64,
        computed_completed_size: i64,
    ) -> Option<CheckResult> {
        if computed_uploaded != -1 && self.cfg.block_excessive {
            if computed_uploaded > torrent_size {
                let max_allowed =
                    (std::cmp::max(torrent_size, self.cfg.minimum_size) as f64 * self.cfg.excessive_threshold)
                        as i64;
                sd.insert("maxAllowedExcessiveThreshold".into(), json_n(max_allowed));
                if computed_uploaded > max_allowed {
                    reset_ban_delay_window(range, addr);
                    sd.insert("type".into(), Value::String("excessiveMaxDownloadThreshold".into()));
                    return Some(CheckResult::new(
                        CTX_PCB,
                        PeerAction::Ban,
                        self.cfg.ban_duration,
                        TranslationComponent::new("PCB_RULE_REACHED_MAX_ALLOWED_EXCESSIVE_THRESHOLD"),
                        TranslationComponent::strs(
                            "MODULE_PCB_EXCESSIVE_DOWNLOAD",
                            vec![
                                torrent_size.to_string(),
                                computed_uploaded.to_string(),
                                max_allowed.to_string(),
                            ],
                        ),
                        sd.clone(),
                    ));
                }
            } else if completed_size > 0 && computed_uploaded > completed_size {
                let max_allowed = (std::cmp::max(computed_completed_size, self.cfg.minimum_size) as f64
                    * self.cfg.excessive_threshold) as i64;
                sd.insert("maxAllowedExcessiveThreshold".into(), json_n(max_allowed));
                if computed_uploaded > max_allowed {
                    reset_ban_delay_window(range, addr);
                    sd.insert(
                        "type".into(),
                        Value::String("excessiveMaxDownloadThresholdForIncompleteTask".into()),
                    );
                    return Some(CheckResult::new(
                        CTX_PCB,
                        PeerAction::Ban,
                        self.cfg.ban_duration,
                        TranslationComponent::new("PCB_RULE_REACHED_MAX_ALLOWED_EXCESSIVE_THRESHOLD"),
                        TranslationComponent::strs(
                            "MODULE_PCB_EXCESSIVE_DOWNLOAD_INCOMPLETE",
                            vec![
                                torrent_size.to_string(),
                                completed_size.to_string(),
                                computed_uploaded.to_string(),
                                max_allowed.to_string(),
                            ],
                        ),
                        sd.clone(),
                    ));
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn difference_test(
        &self,
        range: &mut Entity,
        addr: &mut Entity,
        client_reported_progress: f64,
        computed_progress: f64,
        sd: &mut Map<String, Value>,
        torrent_size: i64,
        peer: &Peer,
    ) -> Option<CheckResult> {
        let difference = (computed_progress - client_reported_progress).abs();
        sd.insert("difference".into(), json_f(difference));
        if difference > self.cfg.max_difference
            && !self.file_too_small(torrent_size)
            && is_uploading_to_peer(peer)
        {
            if !is_ban_delay_window_scheduled(range, addr) {
                schedule_ban_delay_window(range, addr, self.cfg.max_wait_duration);
                return None;
            }
            if is_ban_delay_window_expired(range, addr) {
                range.row.progress_difference_counter += 1;
                addr.row.progress_difference_counter += 1;
                reset_ban_delay_window(range, addr);
                sd.insert("type".into(), Value::String("deSyncDifference".into()));
                return Some(CheckResult::new(
                    CTX_PCB,
                    PeerAction::Ban,
                    self.cfg.ban_duration,
                    TranslationComponent::new("PCB_RULE_REACHED_MAX_DIFFERENCE"),
                    TranslationComponent::strs(
                        "MODULE_PCB_PEER_BAN_INCORRECT_PROGRESS",
                        vec![
                            percent(client_reported_progress),
                            percent(computed_progress),
                            percent(difference),
                        ],
                    ),
                    sd.clone(),
                ));
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn progress_rewind(
        &self,
        peer: &Peer,
        sd: &mut Map<String, Value>,
        range: &mut Entity,
        addr: &mut Entity,
        client_reported_progress: f64,
        computed_progress: f64,
        torrent_size: i64,
    ) -> Option<CheckResult> {
        if self.cfg.rewind_max_difference > 0.0 && !self.file_too_small(torrent_size) {
            let last_report_progress = addr.row.last_report_progress.max(range.row.last_report_progress);
            let rewind = last_report_progress - peer.progress;
            sd.insert("peerLastReportProgress".into(), json_f(addr.row.last_report_progress));
            sd.insert("rangeLastRecordProgress".into(), json_f(range.row.last_report_progress));
            sd.insert("lastReportProgress".into(), json_f(last_report_progress));
            sd.insert("rewind".into(), json_f(rewind));
            sd.insert("rewindMaximumDifference".into(), json_f(self.cfg.rewind_max_difference));

            if rewind > self.cfg.rewind_max_difference && is_uploading_to_peer(peer) {
                if peer.progress > 0.0 || is_ban_delay_window_expired(range, addr) {
                    addr.row.rewind_counter += 1;
                    range.row.rewind_counter += 1;
                    reset_ban_delay_window(range, addr);
                    sd.insert("type".into(), Value::String("rewindProgress".into()));
                    return Some(CheckResult::new(
                        CTX_PCB,
                        PeerAction::Ban,
                        self.cfg.ban_duration,
                        TranslationComponent::new("PCB_RULE_PROGRESS_REWIND"),
                        TranslationComponent::strs(
                            "MODULE_PCB_PEER_BAN_REWIND",
                            vec![
                                percent(client_reported_progress),
                                percent(computed_progress),
                                percent(last_report_progress),
                                percent(rewind),
                                percent(self.cfg.rewind_max_difference),
                            ],
                        ),
                        sd.clone(),
                    ));
                } else if !is_ban_delay_window_scheduled(range, addr) {
                    schedule_ban_delay_window(range, addr, self.cfg.max_wait_duration);
                }
            }
        }
        None
    }
}

fn schedule_ban_delay_window(range: &mut Entity, addr: &mut Entity, max_wait: i64) {
    let end = Millis::now().add_ms(max_wait).millis();
    if range.row.ban_delay_window_end_at <= 0 {
        range.row.ban_delay_window_end_at = end;
    }
    if addr.row.ban_delay_window_end_at <= 0 {
        addr.row.ban_delay_window_end_at = end;
    }
}

fn is_ban_delay_window_scheduled(range: &Entity, addr: &Entity) -> bool {
    range.row.ban_delay_window_end_at > 0 || addr.row.ban_delay_window_end_at > 0
}

fn is_ban_delay_window_expired(range: &Entity, addr: &Entity) -> bool {
    let now = Millis::now().millis();
    (range.row.ban_delay_window_end_at > 0 && range.row.ban_delay_window_end_at < now)
        || (addr.row.ban_delay_window_end_at > 0 && addr.row.ban_delay_window_end_at < now)
}

fn reset_ban_delay_window(range: &mut Entity, addr: &mut Entity) {
    if range.row.ban_delay_window_end_at > 0 {
        range.row.ban_delay_window_end_at = 0;
    }
    if addr.row.ban_delay_window_end_at > 0 {
        addr.row.ban_delay_window_end_at = 0;
    }
}

fn json_n(v: i64) -> Value {
    Value::Number(v.into())
}

fn json_f(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

impl RuleModule for ProgressCheatBlocker {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &'static str {
        "Progress Cheat Blocker"
    }
    fn config_name(&self) -> &'static str {
        "progress-cheat-blocker"
    }

    fn should_ban_peer(&self, torrent: &Torrent, peer: &Peer, downloader: &dyn Downloader) -> CheckResult {
        self.check(torrent, peer, downloader)
    }

    fn on_wave_complete(&self) {
        self.store.flush();
    }

    fn on_peer_unban(&self, torrent_id: &str, ip: &str) {
        self.store.on_peer_unban(torrent_id, ip);
    }

    fn web_data(&self, _locale: &str, _lang: &pbh_core::lang::LangStore) -> Value {
        // 对应上游 handleConfig：返回当前生效配置
        serde_json::json!({
            "minimumSize": self.cfg.minimum_size,
            "maximumDifference": self.cfg.max_difference,
            "rewindMaximumDifference": self.cfg.rewind_max_difference,
            "blockExcessiveClients": self.cfg.block_excessive,
            "excessiveThreshold": self.cfg.excessive_threshold,
            "ipv4PrefixLength": self.cfg.ipv4_prefix_length,
            "ipv6PrefixLength": self.cfg.ipv6_prefix_length,
            "banDuration": self.cfg.ban_duration,
            "enablePersist": self.cfg.enable_persist,
            "persistDuration": self.cfg.persist_duration,
            "maxWaitDuration": self.cfg.max_wait_duration,
            "fastPcbTestPercentage": self.cfg.fast_test_percentage,
            "fastPcbTestBlockDuration": self.cfg.fast_test_block_duration,
        })
    }
}

// 使模块上下文常量可被外部引用（history.module_name 校验）
#[allow(dead_code)]
pub fn module_context() -> &'static str {
    context::ABSTRACT_RULE
}
