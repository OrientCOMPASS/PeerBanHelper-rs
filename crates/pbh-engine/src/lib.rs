//! pbh-engine：BanWave 调度引擎与内存封禁表。
//!
//! 对应上游 DownloaderServerImpl + banpipeline（DigestionSession）。上游用 7 级
//! BanOrgan 流水线 + 三种线程池编排；此处按「进程外行为一致」原则以 tokio 任务直接
//! 实现同等语义（顺序：过期解封 → 逐下载器登录/取种子/取 peers → 模块检查与合并 →
//! 生成封禁 → 应用到所有下载器 → 持久化与日志）。
use anyhow::Result;
use ipnet::IpNet;
use parking_lot::RwLock;
use pbh_core::banlist::{BanList, BanListHandle};
use pbh_config::ConfigHandle;
use pbh_core::check::{CheckResult, PeerAction};
use pbh_core::ipset::CidrSet;
use pbh_core::lang::LangStore;
use pbh_core::model::{host_net, BanMetadata, DownloaderBasicInfo, Millis, Peer, Torrent};
use pbh_core::translate::TranslationComponent;
use pbh_db::{Database, HistoryRow, PeerRecordRow, SwarmRow, TrafficJournalRow};
use pbh_downloader::{Downloader, DownloaderLastStatus, DownloaderManager, StatusSnapshot};
use pbh_modules::RuleModule;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default)]
pub struct WaveStats {
    pub downloaders: u64,
    pub torrents: u64,
    pub peers: u64,
    pub banned: u64,
    pub unbanned: u64,
    pub duration_ms: u64,
}

#[derive(Default)]
pub struct EngineStats {
    pub last_wave_at: AtomicU64,
    pub last_wave_duration_ms: AtomicU64,
    pub total_banned: AtomicU64,
    pub total_unbanned: AtomicU64,
    pub waves_run: AtomicU64,
}

pub struct Engine {
    pub cfg: ConfigHandle,
    pub lang: Arc<LangStore>,
    pub db: Arc<Database>,
    pub downloaders: Arc<DownloaderManager>,
    pub modules: RwLock<Vec<Arc<dyn RuleModule>>>,
    pub rule_sub: RwLock<Option<Arc<pbh_modules::rule_sub::IpBlackRuleList>>>,
    pub geoip: Option<Arc<pbh_geoip::IpDb>>,
    pub push: Arc<pbh_push::PushManager>,
    banlist: BanListHandle,
    pub stats: EngineStats,
    pub global_paused: AtomicBool,
    started_at: Millis,
    last_save: RwLock<Option<Instant>>,
    shutdown: AtomicBool,
    /// 对应上游 scheduledBanListOperations（手动封禁/解封队列，下一轮 BanWave 处理）
    // 监视数据缓冲（对应上游 PeerRecording/SwarmTracking 的 PBHCache 定期刷写）
    peer_record_buf: RwLock<HashMap<(String, i64, String, String), PeerRecordRow>>,
    swarm_buf: RwLock<HashMap<(String, i64, String, String), SwarmRow>>,
    last_peer_flush: RwLock<Instant>,
    last_swarm_flush: RwLock<Instant>,
    last_journal: RwLock<Instant>,
    journal_baseline: RwLock<HashMap<String, (i64, i64)>>,
    last_retention_cleanup: RwLock<Instant>,
    pending_bans: RwLock<VecDeque<String>>,
    pending_unbans: RwLock<VecDeque<IpNet>>,
    /// 对应上游 needReApplyBanList
    pub need_reapply: AtomicBool,
}

pub struct EngineBuilder {
    pub cfg: ConfigHandle,
    pub lang: Arc<LangStore>,
    pub db: Arc<Database>,
    pub downloaders: Arc<DownloaderManager>,
    pub built: pbh_modules::BuiltModules,
    pub banlist: BanListHandle,
    pub geoip: Option<Arc<pbh_geoip::IpDb>>,
    pub push: Arc<pbh_push::PushManager>,
}

impl Engine {
    pub fn new(b: EngineBuilder) -> Arc<Self> {
        Arc::new(Self {
            cfg: b.cfg,
            lang: b.lang,
            db: b.db,
            downloaders: b.downloaders,
            modules: RwLock::new(b.built.list),
            rule_sub: RwLock::new(b.built.rule_sub),
            geoip: b.geoip,
            push: b.push,
            banlist: b.banlist,
            stats: EngineStats::default(),
            global_paused: AtomicBool::new(false),
            started_at: Millis::now(),
            last_save: RwLock::new(None),
            shutdown: AtomicBool::new(false),
            peer_record_buf: RwLock::new(HashMap::new()),
            swarm_buf: RwLock::new(HashMap::new()),
            last_peer_flush: RwLock::new(Instant::now()),
            last_swarm_flush: RwLock::new(Instant::now()),
            last_journal: RwLock::new(Instant::now() - std::time::Duration::from_secs(120)),
            journal_baseline: RwLock::new(HashMap::new()),
            last_retention_cleanup: RwLock::new(Instant::now()),
            pending_bans: RwLock::new(VecDeque::new()),
            pending_unbans: RwLock::new(VecDeque::new()),
            need_reapply: AtomicBool::new(false),
        })
    }

    pub fn uptime_ms(&self) -> i64 {
        Millis::now().millis() - self.started_at.millis()
    }

    pub fn banlist_size(&self) -> usize {
        self.banlist.read().len()
    }

    pub fn is_banned(&self, ip: std::net::IpAddr) -> bool {
        self.banlist.read().contains_addr(ip)
    }

    pub fn snapshot_bans(&self) -> Vec<(IpNet, BanMetadata)> {
        self.banlist.read().snapshot()
    }

    /// POST /api/bans → scheduleBanPeerNoAssign(PeerAddress)
    pub fn schedule_ban_peer_no_assign(&self, ip: &str) {
        self.pending_bans.write().push_back(ip.to_string());
    }

    /// DELETE /api/bans → scheduleUnBanPeer：立即从内存封禁表移除（unbanPeers 语义），
    /// 并标记下一轮全量重应用。
    /// 注：上游对单个 IP 解封不设置 needReApplyBanList（下载器过滤器会延迟到下一次
    /// 有变更的 BanWave 才收敛，疑似上游缺陷）；pbh-rs 主动置位以保证解封及时生效，
    /// 封禁决策本身不受影响。
    pub fn unban_addresses(&self, nets: &[IpNet]) -> usize {
        let mut removed_metas: Vec<BanMetadata> = Vec::new();
        {
            let mut list = self.banlist.write();
            for net in nets {
                removed_metas.extend(list.remove_contained(net));
            }
        }
        let removed = removed_metas.len();
        if removed > 0 {
            self.notify_unban(&removed_metas);
            self.need_reapply.store(true, Ordering::Relaxed);
        }
        removed
    }

    fn notify_unban(&self, metas: &[BanMetadata]) {
        let modules = self.modules.read();
        for meta in metas {
            let ip = pbh_core::model::compress_ip(
                meta.peer
                    .address
                    .ip_addr()
                    .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
            );
            for m in modules.iter() {
                m.on_peer_unban(&meta.torrent.id, &ip);
            }
        }
    }

    /// "*" 全量解封：清空封禁表并强制下一轮全量重应用（对应 handleBanDelete 的 "*" 分支）
    pub fn unban_all(&self) -> usize {
        let mut list = self.banlist.write();
        let n = list.len();
        *list = BanList::new();
        drop(list);
        self.need_reapply.store(true, Ordering::Relaxed);
        n
    }

    /// 上游手动封禁的 mock PeerAddress Lombok toString（参数文案逐字节对齐）
    fn lombok_peer_address_string(ip: &str) -> String {
        format!(
            "PeerAddress(downloaderRawIp={}, downloaderRawPort=0, teredoClientIp=null, teredoClientUdpPort=0, nattedClientIp=null, nattedClientPort=0, ip={}, address=null, port=0, natTranslated=false, teredoTranslated=false)",
            ip, ip
        )
    }

    /// 构造上游 scheduleBanPeerNoAssign(PeerAddress) 的 mock 元数据
    fn build_manual_ban(&self, ip: &str) -> Option<(IpNet, BanMetadata)> {
        let parsed = pbh_core::ipset::parse_cidr(ip)?;
        let net = pbh_core::model::host_net(parsed.addr());
        let addr = pbh_core::model::PeerAddress::new(ip, 0, ip);
        let mock_torrent = Torrent {
            id: "00000000000000000000".to_string(),
            name: "User Operation".to_string(),
            hash: "00000000000000000000".to_string(),
            size: 0,
            completed_size: 0,
            progress: 0.0,
            rt_upload_speed: 0,
            rt_download_speed: 0,
            private_torrent: false,
            piece_size: 0,
            pieces_have: 0,
            attached_peers: Vec::new(),
        };
        let mock_peer = Peer {
            address: addr.clone(),
            peer_id: Some("-USROPS-".to_string()),
            client_name: Some("User Operation".to_string()),
            download_speed: 0,
            downloaded: 0,
            upload_speed: 0,
            uploaded: 0,
            progress: 0.0,
            flags: None,
            handshaking: false,
        };
        let year_plus_100 = 100i64 * 365 * 24 * 60 * 60 * 1000;
        let now = Millis::now();
        let mut sd = serde_json::Map::new();
        sd.insert("type".into(), serde_json::Value::String("manually".into()));
        sd.insert(
            "ip".into(),
            serde_json::to_value(&addr).unwrap_or(serde_json::Value::Null),
        );
        let meta = BanMetadata {
            downloader: self
                .downloaders
                .iter()
                .first()
                .map(|d| d.basic_info())
                .unwrap_or(DownloaderBasicInfo {
                    id: String::new(),
                    name: String::new(),
                    downloader_type: String::new(),
                }),
            torrent: mock_torrent.wrapper(),
            peer: mock_peer.wrapper(),
            reverse_lookup: "N/A".to_string(),
            context: "com.ghostchu.peerbanhelper.DownloaderServerImpl".to_string(),
            random_id: uuid::Uuid::new_v4().simple().to_string(),
            ban_at: now,
            unban_at: now.add_ms(year_plus_100),
            ban_for_disconnect: false,
            exclude_from_report: false,
            exclude_from_display: false,
            rule: TranslationComponent::new("PEER_BAN_USER_OPERATE_TITLE"),
            description: TranslationComponent::strs(
                "PEER_BAN_USER_OPERATE_DESCRIPTION",
                vec![Self::lombok_peer_address_string(ip)],
            ),
            structured_data: sd,
        };
        Some((net, meta))
    }

    /// 对应上游 AlertManager.publishAlert：同 identifier 的未读告警存在时跳过；
    /// title/content 以 TranslationComponent JSON 存储，读取时按请求 locale 烘焙。
    pub fn publish_alert(
        &self,
        push: bool,
        level: &str,
        identifier: &str,
        title: &TranslationComponent,
        content: &TranslationComponent,
    ) {
        if push {
            let locale = self.lang.default_locale.clone();
            self.push.send_all(
                self.lang.tlc(&locale, title),
                self.lang.tlc(&locale, content),
            );
        }
        if self.db.unread_alert_exists(identifier) {
            return;
        }
        let title_json = serde_json::to_string(title).unwrap_or_default();
        let content_json = serde_json::to_string(content).unwrap_or_default();
        if let Err(e) = self.db.insert_alert(
            Millis::now().millis(),
            level,
            identifier,
            &title_json,
            &content_json,
        ) {
            tracing::error!("Failed to publish alert {}: {}", identifier, e);
        }
    }

    /// 对应 loadBanListToMemory（persist.banlist 开启时）
    pub fn load_banlist_from_db(&self) {
        if !self.cfg.get().persist_banlist() {
            return;
        }
        match self.db.read_banlist() {
            Ok(rows) => {
                let mut list = self.banlist.write();
                let mut ok = 0usize;
                let mut bad = 0usize;
                for (address, metadata) in rows {
                    match (parse_address_key(&address), serde_json::from_str::<BanMetadata>(&metadata)) {
                        (Some(net), Ok(meta)) => {
                            list.insert(net, meta);
                            ok += 1;
                        }
                        _ => {
                            bad += 1;
                            tracing::warn!("Corrupted banlist entry dropped: {}", address);
                        }
                    }
                }
                if bad > 0 {
                    tracing::warn!("{} corrupted banlist entries dropped", bad);
                }
                tracing::info!(
                    "{}",
                    self.lang.tl(
                        &self.lang.default_locale,
                        "LOAD_BANLIST_FROM_FILE",
                        &[ok.to_string().into()]
                    )
                );
            }
            Err(e) => tracing::error!("Failed to load banlist from database: {}", e),
        }
    }

    /// 对应 saveBanList
    pub fn save_banlist(&self) -> Result<usize> {
        if !self.cfg.get().persist_banlist() {
            return Ok(0);
        }
        let snapshot = self.banlist.read().snapshot();
        let mut entries = Vec::with_capacity(snapshot.len());
        for (net, meta) in snapshot {
            let address = match net {
                IpNet::V4(n) => n.addr().to_string(),
                IpNet::V6(n) => n.addr().to_string(),
            };
            entries.push((address, serde_json::to_string(&meta)?));
        }
        let count = self.db.save_banlist(&entries)?;
        *self.last_save.write() = Some(Instant::now());
        tracing::info!(
            "{}",
            self.lang.tl(
                &self.lang.default_locale,
                "SAVED_BANLIST",
                &[count.to_string().into()]
            )
        );
        Ok(count)
    }

    fn maybe_periodic_save(&self) {
        let need = match *self.last_save.read() {
            Some(at) => at.elapsed().as_secs() >= 3600, // 上游 BANLIST_SAVE_INTERVAL = 1h
            None => true,
        };
        if need {
            if let Err(e) = self.save_banlist() {
                tracing::error!("Failed to save banlist: {}", e);
            }
        }
    }

    /// 一轮 BanWave。对应上游 DownloaderServerImpl.banWave + DigestionSession.runBanWave
    pub async fn ban_wave(&self) {
        if self.global_paused.load(Ordering::Relaxed) {
            return;
        }
        let start = Instant::now();
        let now = Millis::now();
        let locale = self.lang.default_locale.clone();
        let cfg = self.cfg.get();
        let (ignore, ignore_errors) =
            CidrSet::from_strs(cfg.ignore_peers_from_addresses().iter().map(|s| s.as_str()));
        for e in ignore_errors {
            tracing::warn!("Invalid entry in ignore-peers-from-addresses: {}", e);
        }
        let modules = self.modules.read().clone();
        let peer_recording = cfg.module_enabled("peer-analyse-service.peer-recording", true);
        let swarm_tracking = cfg.module_enabled("peer-analyse-service.swarm-tracking", true);
        let active_monitoring = cfg.module_enabled("active-monitoring", true);

        // 1. 过期解封（对应 removeExpiredBans；汇总日志仅统计非 banForDisconnect 项）
        let removed: Vec<(IpNet, BanMetadata)> = {
            let mut list = self.banlist.write();
            list.remove_expired(now)
        };
        let normal_unban_count = removed
            .iter()
            .filter(|(_, m)| !m.ban_for_disconnect)
            .count();
        // 对应上游 PeerUnbanEvent → PCB onPeerUnBan（删除追踪记录）
        {
            let modules = self.modules.read();
            for (_, meta) in removed.iter() {
                let ip = pbh_core::model::compress_ip(
                    meta.peer.address.ip_addr().unwrap_or_else(|| {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
                    }),
                );
                for m in modules.iter() {
                    m.on_peer_unban(&meta.torrent.id, &ip);
                }
            }
        }
        if normal_unban_count > 0 {
            tracing::info!(
                "{}",
                self.lang.tl(
                    &locale,
                    "PEER_UNBAN_WAVE",
                    &[normal_unban_count.to_string().into()]
                )
            );
        }

        // 1.5 处理计划操作（对应上游 scheduledBanListOperations：手动封禁在此落地）
        let mut stats = WaveStats::default();
        let mut added_global: Vec<BanMetadata> = Vec::new();
        {
            let bans: Vec<String> = self.pending_bans.write().drain(..).collect();
            for ip in bans {
                match self.build_manual_ban(&ip) {
                    Some((net, meta)) => {
                        let mut list = self.banlist.write();
                        if !list.contains(&net) {
                            list.insert(net, meta.clone());
                            drop(list);
                            self.record_manual_ban(&net, &meta);
                            added_global.push(meta);
                        }
                    }
                    None => tracing::warn!("Manual ban skipped, invalid IP: {}", ip),
                }
            }
            // 计划解封（通常已由 unban_addresses 即时移除；保留队列以对齐上游结构）
            let unbans: Vec<IpNet> = self.pending_unbans.write().drain(..).collect();
            if !unbans.is_empty() {
                let mut list = self.banlist.write();
                for net in unbans {
                    list.remove(&net);
                }
            }
        }

        // 2. 逐下载器检查
        let global_ban_duration = cfg.ban_duration_ms();

        for dl in self.downloaders.iter() {
            if dl.is_paused() {
                tracing::debug!("Downloader {} is paused, skipping", dl.name());
                self.downloaders.set_status(
                    dl.id(),
                    StatusSnapshot {
                        last_status: DownloaderLastStatus::PAUSED,
                        last_status_message: self.lang.tl(&locale, "STATUS_TEXT_OK", &[]),
                        ..Default::default()
                    },
                );
                continue;
            }
            stats.downloaders += 1;

            let login = dl.login().await;
            if !login.is_success() {
                let msg = self.lang.tlc(&locale, &login.message);
                tracing::warn!(
                    "{}",
                    self.lang.tl(
                        &locale,
                        "ERR_CLIENT_LOGIN_FAILURE_SKIP",
                        &[dl.name().to_string().into(), msg.clone().into()]
                    )
                );
                self.downloaders.set_status(
                    dl.id(),
                    StatusSnapshot {
                        last_status: match login.status {
                            pbh_downloader::LoginStatus::RequireTakeActions => {
                                DownloaderLastStatus::NEED_TAKE_ACTION
                            }
                            _ => DownloaderLastStatus::ERROR,
                        },
                        last_status_message: msg,
                        ..Default::default()
                    },
                );
                // 注：上游对登录失败仅记录日志与状态（不发布 Alert），保持一致
                continue;
            }

            let torrents = match dl.get_torrents().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("[{}] Failed to fetch torrents: {}", dl.name(), e);
                    self.downloaders.set_status(
                        dl.id(),
                        StatusSnapshot {
                            last_status: DownloaderLastStatus::ERROR,
                            last_status_message: e.to_string(),
                            ..Default::default()
                        },
                    );
                    continue;
                }
            };
            stats.torrents += torrents.len() as u64;
            let mut dl_peers = 0i64;

            for torrent in &torrents {
                let peers = match dl.get_peers(torrent).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(
                            "[{}] Failed to fetch peers of torrent {}: {}",
                            dl.name(),
                            torrent.hash,
                            e
                        );
                        continue;
                    }
                };
                stats.peers += peers.len() as u64;
                dl_peers += peers.len() as i64;

                // 监视数据收集（peer-recording / swarm-tracking）
                if peer_recording || swarm_tracking {
                    let torrent_db_id = self.db
                        .create_torrent_if_not_exists(
                            &torrent.hash,
                            &torrent.name,
                            torrent.size,
                            torrent.private_torrent,
                        )
                        .unwrap_or(0);
                    let now_ms = Millis::now().millis();
                    for peer in &peers {
                        if peer_recording {
                            if let Some(_ip) = peer.address.ip_addr() {
                                let row = PeerRecordRow {
                                    address: peer.address.ip.clone(),
                                    port: peer.address.port as i64,
                                    torrent_id: torrent_db_id,
                                    downloader: dl.id().to_string(),
                                    peer_id: peer.peer_id.clone(),
                                    client_name: peer.client_name.clone(),
                                    uploaded: peer.uploaded.max(0),
                                    upload_speed: peer.upload_speed.max(0),
                                    downloaded: peer.downloaded.max(0),
                                    download_speed: peer.download_speed.max(0),
                                    last_flags: peer.flags.clone(),
                                    last_time_seen: now_ms,
                                    peer_geoip: None,
                                };
                                let key = (
                                    row.address.clone(),
                                    row.port,
                                    torrent.hash.clone(),
                                    row.downloader.clone(),
                                );
                                self.peer_record_buf.write().insert(key, row);
                            }
                        }
                        if swarm_tracking {
                            let row = SwarmRow {
                                ip: peer.address.ip.clone(),
                                port: peer.address.port as i64,
                                info_hash: torrent.hash.clone(),
                                private: torrent.private_torrent,
                                torrent_size: torrent.size,
                                downloader: dl.id().to_string(),
                                downloader_progress: torrent.progress,
                                peer_id: peer.peer_id.clone(),
                                client_name: peer.client_name.clone(),
                                peer_progress: format!("{:.3}", peer.progress),
                                uploaded: peer.uploaded.max(0),
                                upload_speed: peer.upload_speed.max(0),
                                downloaded: peer.downloaded.max(0),
                                download_speed: peer.download_speed.max(0),
                                last_flags: peer.flags.clone(),
                                last_time_seen: now_ms,
                            };
                            let key = (
                                row.ip.clone(),
                                row.port,
                                row.info_hash.clone(),
                                row.downloader.clone(),
                            );
                            self.swarm_buf.write().insert(key, row);
                        }
                    }
                }

                for peer in &peers {
                    let Some(ip) = peer.address.ip_addr() else {
                        continue;
                    };
                    let net = host_net(ip);
                    // 已封禁的 Peer 不再重复检查（对应上游 banlistClone 去重语义）
                    if self.banlist.read().contains(&net) {
                        continue;
                    }
                    let result = pbh_modules::check_peer(&modules, torrent, peer, dl.as_ref(), &ignore);
                    match result.action {
                        PeerAction::Ban | PeerAction::BanForDisconnect => {
                            match self.apply_ban(dl.as_ref(), torrent, peer, &result, global_ban_duration) {
                                Some(meta) => added_global.push(meta),
                                None => {}
                            }
                        }
                        _ => {}
                    }
                }
            }

            self.downloaders.set_status(
                dl.id(),
                StatusSnapshot {
                    last_status: DownloaderLastStatus::HEALTHY,
                    last_status_message: self.lang.tl(&locale, "STATUS_TEXT_OK", &[]),
                    active_torrents: torrents.len() as i64,
                    active_peers: dl_peers,
                },
            );
        }

        stats.banned = added_global.len() as u64;
        stats.unbanned = removed.len() as u64;

        // 3. 应用封禁列表到所有下载器（封禁表是全局的，跨下载器生效）
        let need_reapply = self.need_reapply.swap(false, Ordering::Relaxed);
        let changed = !added_global.is_empty() || !removed.is_empty() || need_reapply;
        if changed {
            let full = self.banlist.read().keys();
            let mut any_failed = false;
            for dl in self.downloaders.iter() {
                // need_reapply → applyFullList=true（对应上游 APPLYING_FULL_BANLIST_TO_DOWNLOADER）
                if let Err(e) = dl
                    .set_ban_list(&full, &added_global, removed.is_empty(), need_reapply)
                    .await
                {
                    tracing::error!("[{}] Failed to apply banlist: {}", dl.name(), e);
                    any_failed = true;
                }
            }
            if any_failed {
                // 比上游更稳健：应用失败时下一轮自动全量重试
                self.need_reapply.store(true, Ordering::Relaxed);
            }
            if need_reapply {
                tracing::info!(
                    "{}",
                    self.lang
                        .tl(&locale, "APPLYING_FULL_BANLIST_TO_DOWNLOADER", &[])
                );
            }
        }

        // 4. 持久化（对应上游每小时 + 关闭时保存；此处每小时兜底 + 变更节流保存）
        if cfg.persist_banlist() && changed {
            let should = match *self.last_save.read() {
                Some(at) => at.elapsed().as_secs() >= 60,
                None => true,
            };
            if should {
                if let Err(e) = self.save_banlist() {
                    tracing::error!("Failed to save banlist: {}", e);
                }
            }
        }
        self.maybe_periodic_save();

        // 5. 统计与完成日志
        let duration_ms = start.elapsed().as_millis() as u64;
        stats.duration_ms = duration_ms;
        self.stats.last_wave_at.store(Millis::now().millis() as u64, Ordering::Relaxed);
        self.stats
            .last_wave_duration_ms
            .store(duration_ms, Ordering::Relaxed);
        self.stats.total_banned.fetch_add(stats.banned, Ordering::Relaxed);
        self.stats
            .total_unbanned
            .fetch_add(stats.unbanned, Ordering::Relaxed);
        self.stats.waves_run.fetch_add(1, Ordering::Relaxed);

        for m in modules.iter() {
            m.on_wave_complete();
        }

        // 监视数据刷写（data-flush-interval 节流）
        if peer_recording {
            let interval = cfg
                .profile
                .get_i64("module.peer-analyse-service.peer-recording.data-flush-interval", 900000);
            let due = self
                .last_peer_flush
                .read()
                .elapsed()
                .as_millis() as i64
                >= interval.min(60000).max(5000);
            if due {
                let rows: Vec<PeerRecordRow> = self.peer_record_buf.write().drain().map(|(_, v)| v).collect();
                if !rows.is_empty() {
                    if let Err(err) = self.db.batch_upsert_peer_records(&rows) {
                        tracing::warn!("peer_records flush failed: {}", err);
                    }
                }
                *self.last_peer_flush.write() = Instant::now();
            }
        }
        if swarm_tracking {
            let interval = cfg
                .profile
                .get_i64("module.peer-analyse-service.swarm-tracking.data-flush-interval", 3600000);
            let due = self
                .last_swarm_flush
                .read()
                .elapsed()
                .as_millis() as i64
                >= interval.min(60000).max(5000);
            if due {
                let rows: Vec<SwarmRow> = self.swarm_buf.write().drain().map(|(_, v)| v).collect();
                if !rows.is_empty() {
                    if let Err(err) = self.db.batch_upsert_tracked_swarm(&rows) {
                        tracing::warn!("tracked_swarm flush failed: {}", err);
                    }
                }
                *self.last_swarm_flush.write() = Instant::now();
            }
        }
        // 流量日志（ActiveMonitoring：60s 节流，对应上游 journaling）
        if active_monitoring
            && self.last_journal.read().elapsed().as_secs() >= 60
        {
            let ts = (Millis::now().millis() / 60000) * 60000;
            for dl in self.downloaders.iter() {
                if let Ok(stat) = dl.statistics().await {
                    let (start_ul, start_dl) = self
                        .journal_baseline
                        .read()
                        .get(dl.id())
                        .copied()
                        .unwrap_or((stat.total_uploaded, stat.total_downloaded));
                    let row = TrafficJournalRow {
                        timestamp: ts,
                        downloader: dl.id().to_string(),
                        data_uploaded_at_start: start_ul,
                        data_uploaded: stat.total_uploaded,
                        data_downloaded_at_start: start_dl,
                        data_downloaded: stat.total_downloaded,
                        protocol_uploaded_at_start: 0,
                        protocol_uploaded: 0,
                        protocol_downloaded_at_start: 0,
                        protocol_downloaded: 0,
                    };
                    if let Err(err) = self.db.insert_traffic_journal(&row) {
                        tracing::warn!("traffic journal failed: {}", err);
                    }
                    self.journal_baseline
                        .write()
                        .insert(dl.id().to_string(), (stat.total_uploaded, stat.total_downloaded));
                }
            }
            *self.last_journal.write() = Instant::now();
        }
        // 数据保留清理（每小时检查一次）
        if self.last_retention_cleanup.read().elapsed().as_secs() >= 3600 {
            let retention = cfg
                .profile
                .get_i64("module.peer-analyse-service.peer-recording.data-retention-time", 5184000000);
            let cutoff = Millis::now().millis() - retention;
            match self.db.cleanup_peer_records(cutoff) {
                Ok(n) if n > 0 => tracing::info!("[PeerRecording] Cleaned {} expired records", n),
                Ok(_) => {}
                Err(err) => tracing::warn!("peer_records cleanup failed: {}", err),
            }
            *self.last_retention_cleanup.write() = Instant::now();
        }

        if !cfg.hide_finish_log() && !self.downloaders.is_empty() {
            tracing::info!(
                "{}",
                self.lang.tl(
                    &locale,
                    "BAN_WAVE_CHECK_COMPLETED",
                    &[
                        stats.downloaders.to_string().into(),
                        stats.torrents.to_string().into(),
                        stats.peers.to_string().into(),
                        stats.banned.to_string().into(),
                        stats.unbanned.to_string().into(),
                        stats.duration_ms.to_string().into(),
                    ]
                )
            );
        }
    }

    /// 生成 BanMetadata、写入封禁表/日志/数据库（对应上游 banWave 内的封禁落地 + PersistMetrics.recordPeerBan）
    fn apply_ban(
        &self,
        dl: &dyn Downloader,
        torrent: &Torrent,
        peer: &Peer,
        result: &CheckResult,
        global_ban_duration: i64,
    ) -> Option<BanMetadata> {
        let Some(ip) = peer.address.ip_addr() else {
            return None;
        };
        let net = host_net(ip);
        let actual_duration = if result.duration > 0 {
            result.duration
        } else {
            global_ban_duration
        };
        let ban_at = Millis::now();
        let unban_at = ban_at.add_ms(actual_duration);
        let ban_for_disconnect = result.action == PeerAction::BanForDisconnect;

        let metadata = BanMetadata {
            downloader: dl.basic_info(),
            torrent: torrent.wrapper(),
            peer: peer.wrapper(),
            reverse_lookup: "N/A".to_string(),
            context: result.module_context.clone(),
            random_id: uuid::Uuid::new_v4().simple().to_string(),
            ban_at,
            unban_at,
            ban_for_disconnect,
            exclude_from_report: result.action.exclude_from_report(),
            exclude_from_display: result.action.exclude_from_display(),
            rule: result.rule.clone(),
            description: result.reason.clone(),
            structured_data: result.structured_data.clone(),
        };

        // 竞态保护：同轮内重复命中只封一次
        {
            let mut list = self.banlist.write();
            if list.contains(&net) {
                return None;
            }
            list.insert(net, metadata.clone());
        }

        let locale = self.lang.default_locale.clone();
        if !ban_for_disconnect {
            // 对应上游 BAN_PEER 控制台日志
            tracing::info!(
                "{}",
                self.lang.tl(
                    &locale,
                    "BAN_PEER",
                    &[
                        format!("{}:{}", peer.address.ip, peer.address.port).into(),
                        peer.peer_id.clone().unwrap_or_default().into(),
                        peer.client_name.clone().unwrap_or_default().into(),
                        format!("{:.2}", peer.progress).into(),
                        peer.uploaded.to_string().into(),
                        peer.downloaded.to_string().into(),
                        torrent.name.clone().into(),
                        self.lang.tlc(&locale, &result.reason).into(),
                    ]
                )
            );

            // PersistMetrics.recordPeerBan：banForDisconnect 不落库
            if let Err(e) = (|| -> Result<()> {
                let torrent_id = self.db.create_torrent_if_not_exists(
                    &torrent.hash,
                    &torrent.name,
                    torrent.size,
                    torrent.private_torrent,
                )?;
                let row = HistoryRow {
                    ban_at: ban_at.millis(),
                    unban_at: unban_at.millis(),
                    ip: pbh_core::model::compress_ip(ip),
                    port: peer.address.port as i64,
                    peer_id: peer.peer_id.clone(),
                    peer_client_name: peer.client_name.clone(),
                    peer_uploaded: peer.uploaded,
                    peer_downloaded: peer.downloaded,
                    peer_progress: peer.progress,
                    downloader_progress: torrent.progress,
                    torrent_id,
                    module_name: result.module_context.clone(),
                    rule_name: serde_json::to_string(&result.rule)?,
                    description: serde_json::to_string(&result.reason)?,
                    flags: peer.flags.clone(),
                    downloader: dl.id().to_string(),
                    structured_data: Some(structured_to_string(&result.structured_data)),
                    peer_geoip: self.query_geoip_json(ip),
                };
                self.db.insert_history(&row)
            })() {
                tracing::error!("Failed to record ban history: {}", e);
            }
        }

        Some(metadata)
    }

    fn record_manual_ban(&self, net: &IpNet, meta: &BanMetadata) {
        let locale = self.lang.default_locale.clone();
        tracing::info!(
            "{}",
            self.lang.tl(
                &locale,
                "BAN_PEER",
                &[
                    format!("{}:{}", meta.peer.address.ip, meta.peer.address.port).into(),
                    meta.peer.id.clone().unwrap_or_default().into(),
                    meta.peer.client_name.clone().unwrap_or_default().into(),
                    format!("{:.2}", meta.peer.progress).into(),
                    meta.peer.uploaded.to_string().into(),
                    meta.peer.downloaded.to_string().into(),
                    meta.torrent.name.clone().into(),
                    self.lang.tlc(&locale, &meta.description).into(),
                ]
            )
        );
        let ip = match net.addr() {
            a => a,
        };
        if let Err(e) = (|| -> anyhow::Result<()> {
            let torrent_id = self.db.create_torrent_if_not_exists(
                &meta.torrent.hash,
                &meta.torrent.name,
                meta.torrent.size,
                meta.torrent.private_torrent,
            )?;
            let row = HistoryRow {
                ban_at: meta.ban_at.millis(),
                unban_at: meta.unban_at.millis(),
                ip: pbh_core::model::compress_ip(ip),
                port: meta.peer.address.port as i64,
                peer_id: meta.peer.id.clone(),
                peer_client_name: meta.peer.client_name.clone(),
                peer_uploaded: meta.peer.uploaded,
                peer_downloaded: meta.peer.downloaded,
                peer_progress: meta.peer.progress,
                downloader_progress: meta.torrent.progress,
                torrent_id,
                module_name: meta.context.clone(),
                rule_name: serde_json::to_string(&meta.rule)?,
                description: serde_json::to_string(&meta.description)?,
                flags: meta.peer.flags.clone(),
                downloader: meta.downloader.id.clone(),
                structured_data: Some(structured_to_string(&meta.structured_data)),
                peer_geoip: meta.peer.address.ip_addr().and_then(|a| self.query_geoip_json(a)),
            };
            self.db.insert_history(&row)
        })() {
            tracing::error!("Failed to record manual ban history: {}", e);
        }
    }

    /// 主循环：scheduleWithFixedDelay 语义（一轮结束后再等待 interval）
    pub async fn run_loop(self: &Arc<Self>) {
        tracing::info!(
            "BanWave scheduler started (interval={}ms, {} modules, {} downloaders)",
            self.cfg.get().check_interval_ms(),
            self.modules.read().len(),
            self.downloaders.len()
        );
        while !self.shutdown.load(Ordering::Relaxed) {
            self.ban_wave().await;
            let interval_ms = self.cfg.get().check_interval_ms();
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    }

    /// 对应上游 ReloadManager.reload()：重读配置文件、重建模块与下载器
    pub fn reload(&self) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        match self.cfg.reload() {
            Ok(cfg) => {
                entries.push(("pbh_config::AppConfig".to_string(), "SUCCESS".to_string()));
                let built = pbh_modules::build_modules(&cfg, &self.lang, self.db.clone(), self.banlist.clone(), &self.cfg, self.geoip.clone());
                tracing::info!(
                    "Reloaded {} rule modules: [{}]",
                    built.list.len(),
                    built.list.iter().map(|m| m.name()).collect::<Vec<_>>().join(", ")
                );
                *self.rule_sub.write() = built.rule_sub;
                *self.modules.write() = built.list;
                entries.push(("pbh_modules".to_string(), "SUCCESS".to_string()));
                self.downloaders.reload_from_config();
                entries.push(("pbh_downloader::DownloaderManager".to_string(), "SUCCESS".to_string()));
                self.push.reload(&cfg);
                entries.push(("pbh_push::PushManager".to_string(), "SUCCESS".to_string()));
            }
            Err(e) => {
                tracing::error!("Config reload failed: {}", e);
                entries.push(("pbh_config::AppConfig".to_string(), "EXCEPTION".to_string()));
            }
        }
        entries
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Engine {
    /// history.peer_geoip：封禁时刻的 GeoIP 快照 JSON（对应 PersistMetrics 的 queryIPDB）
    pub fn query_geoip_json(&self, ip: std::net::IpAddr) -> Option<String> {
        let geoip = self.geoip.as_ref()?;
        let data = geoip.query(ip)?;
        serde_json::to_string(&data).ok()
    }
}

fn structured_to_string(v: &serde_json::Map<String, serde_json::Value>) -> String {
    serde_json::to_string(&serde_json::Value::Object(v.clone())).unwrap_or_default()
}

fn parse_address_key(s: &str) -> Option<IpNet> {
    pbh_core::ipset::parse_cidr(s)
}
