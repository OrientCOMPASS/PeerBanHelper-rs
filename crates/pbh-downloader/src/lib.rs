//! pbh-downloader：下载器抽象与适配器。
//!
//! 对应上游 downloader 包：Downloader 接口、DownloaderManager（支持运行期注册/注销/
//! 配置热重载/saveDownloaders 写回）、AbstractDownloader 的 banlist 重映射逻辑。
//! MVP 实现 qBittorrent / qBittorrentEE 适配器（HTTP 协议行为与上游 AbstractQbittorrent 一致）。
use anyhow::Result;
use async_trait::async_trait;
use ipnet::IpNet;
use parking_lot::RwLock;
use pbh_config::{ConfigHandle, RemapConfig, YValue};
use pbh_core::model::{BanMetadata, DownloaderBasicInfo, Peer, Torrent};
use pbh_core::translate::TranslationComponent;
use std::collections::HashMap;
use std::sync::Arc;

pub mod aria2next;
pub mod biglybt;
pub mod qbittorrent;
pub mod transmission;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum LoginStatus {
    Success,
    IncorrectCredential,
    Exception,
    RequireTakeActions,
    NetworkError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[allow(non_camel_case_types)]
pub enum DownloaderLastStatus {
    HEALTHY,
    PAUSED,
    NEED_TAKE_ACTION,
    ERROR,
    UNKNOWN,
}

#[derive(Clone, Debug)]
pub struct DownloaderLoginResult {
    pub status: LoginStatus,
    pub message: TranslationComponent,
}

impl DownloaderLoginResult {
    pub fn success() -> Self {
        Self {
            status: LoginStatus::Success,
            message: TranslationComponent::new("STATUS_TEXT_OK"),
        }
    }
    pub fn is_success(&self) -> bool {
        self.status == LoginStatus::Success
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DownloaderStatistics {
    pub total_uploaded: i64,
    pub total_downloaded: i64,
}

/// 引擎在每轮 BanWave 中更新，供 /api/downloaders/{id}/status 使用
#[derive(Clone, Debug)]
pub struct StatusSnapshot {
    pub last_status: DownloaderLastStatus,
    pub last_status_message: String,
    pub active_torrents: i64,
    pub active_peers: i64,
}

impl Default for StatusSnapshot {
    fn default() -> Self {
        Self {
            last_status: DownloaderLastStatus::UNKNOWN,
            last_status_message: String::new(),
            active_torrents: 0,
            active_peers: 0,
        }
    }
}

#[async_trait]
pub trait Downloader: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn downloader_type(&self) -> &str;
    fn endpoint(&self) -> &str;
    fn is_paused(&self) -> bool;
    fn set_paused(&self, paused: bool);

    fn basic_info(&self) -> DownloaderBasicInfo {
        DownloaderBasicInfo {
            id: self.id().to_string(),
            name: self.name().to_string(),
            downloader_type: self.downloader_type().to_string(),
        }
    }

    async fn login(&self) -> DownloaderLoginResult;
    /// 活跃 Torrent（对应 getTorrents：仅活跃任务，按配置排除私有种子）
    async fn get_torrents(&self) -> Result<Vec<Torrent>>;
    async fn get_peers(&self, torrent: &Torrent) -> Result<Vec<Peer>>;
    /// 应用封禁列表。`removed_empty` 表示本轮没有解封项（qB 增量封禁的前置条件之一）。
    async fn set_ban_list(
        &self,
        full: &[IpNet],
        added: &[BanMetadata],
        removed_empty: bool,
        apply_full: bool,
    ) -> Result<()>;
    async fn statistics(&self) -> Result<DownloaderStatistics>;
    /// 是否支持 CIDR 范围封禁（RANGE_BAN_IP 特性旗标）
    fn support_range_ban(&self) -> bool {
        false
    }
    /// 扩展特性旗标（对应上游 DownloaderFeatureFlag：UNBAN_IP / TRAFFIC_STATS /
    /// LIVE_UPDATE_BT_PROTOCOL_PORT / RANGE_BAN_IP 等）
    fn has_feature(&self, _flag: &str) -> bool {
        false
    }
    /// 对应 saveDownloader()：写回 config.yml client 节的有序键值（kebab-case）
    fn save_config(&self) -> Vec<(String, YValue)>;
    /// WebUI 编辑表单使用的 JSON 配置（camelCase，对应上游 Gson 序列化 QBittorrentConfigImpl）
    fn config_json(&self) -> serde_json::Value;
}

pub struct DownloaderManager {
    pub(crate) list: RwLock<Vec<Arc<dyn Downloader>>>,
    pub(crate) status: RwLock<HashMap<String, StatusSnapshot>>,
    handle: ConfigHandle,
}

impl DownloaderManager {
    pub fn from_config(handle: &ConfigHandle) -> Result<Arc<Self>> {
        let mgr = Arc::new(Self {
            list: RwLock::new(Vec::new()),
            status: RwLock::new(HashMap::new()),
            handle: handle.clone(),
        });
        mgr.reload_from_config();
        Ok(mgr)
    }

    /// 对应 loadDownloaders()：从 config.yml client 节重建下载器列表
    pub fn reload_from_config(&self) {
        let cfg = self.handle.get();
        let mut new_list: Vec<Arc<dyn Downloader>> = Vec::new();
        for id in cfg.main.section_keys("client") {
            let section_path = format!("client.{}", id);
            let Some(section) = cfg.main.get(&section_path) else {
                continue;
            };
            let dtype = pbh_config::yaml_get(section, "type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            match create_downloader_from_yaml(&id, &dtype, section, &self.handle) {
                Some(dl) => {
                    tracing::info!(
                        "Discovered new client: {} ({}) @ {}",
                        dl.downloader_type(),
                        format!("{}({})", dl.name(), id),
                        dl.endpoint()
                    );
                    new_list.push(dl);
                }
                None => tracing::error!(
                    "Downloader type `{}` (id={}) is not supported by pbh-rs yet, skipping",
                    dtype,
                    id
                ),
            }
        }
        *self.list.write() = new_list;
    }

    pub fn iter(&self) -> Vec<Arc<dyn Downloader>> {
        self.list.read().clone()
    }

    pub fn find(&self, id: &str) -> Option<Arc<dyn Downloader>> {
        self.list.read().iter().find(|d| d.id() == id).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.list.read().is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.read().len()
    }

    /// 对应 registerDownloader：id 或 name 重复时拒绝
    pub fn register(&self, dl: Arc<dyn Downloader>) -> bool {
        let mut list = self.list.write();
        if list.iter().any(|d| d.id() == dl.id() || d.name() == dl.name()) {
            return false;
        }
        list.push(dl);
        true
    }

    pub fn unregister(&self, id: &str) -> bool {
        let mut list = self.list.write();
        let before = list.len();
        list.retain(|d| d.id() != id);
        list.len() != before
    }

    /// 对应 saveDownloaders()：以注册顺序重建 config.yml 的 client 子树
    pub fn save_downloaders(&self) -> Result<()> {
        use pbh_config::yaml_file::{YamlFile, YValue as YV};
        let mut file = YamlFile::load(self.handle.main_file())?;
        let list = self.list.read();
        let mut entries: Vec<(String, YV)> = Vec::new();
        for dl in list.iter() {
            entries.push((dl.id().to_string(), YV::Map(dl.save_config())));
        }
        file.set("client", &YV::Map(entries))?;
        file.save()?;
        Ok(())
    }

    pub fn set_status(&self, id: &str, snap: StatusSnapshot) {
        self.status.write().insert(id.to_string(), snap);
    }

    pub fn get_status(&self, id: &str) -> StatusSnapshot {
        self.status
            .read()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn handle(&self) -> &ConfigHandle {
        &self.handle
    }
}

pub fn create_downloader_from_yaml(
    id: &str,
    dtype: &str,
    section: &serde_yaml::Value,
    handle: &ConfigHandle,
) -> Option<Arc<dyn Downloader>> {
    match dtype {
        "qbittorrent" | "qbittorrentee" => {
            match qbittorrent::QBittorrent::from_yaml(id, dtype, section, handle) {
                Ok(qb) => Some(Arc::new(qb)),
                Err(e) => {
                    tracing::error!("Failed to create downloader {}: {}", id, e);
                    None
                }
            }
        }
        "transmission" => match transmission::Transmission::from_yaml(id, section, handle) {
            Ok(tr) => Some(Arc::new(tr)),
            Err(e) => {
                tracing::error!("Failed to create downloader {}: {}", id, e);
                None
            }
        },
        "biglybt" => match biglybt::BiglyBT::from_yaml(id, section, handle) {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                tracing::error!("Failed to create downloader {}: {}", id, e);
                None
            }
        },
        "aria2next" => match aria2next::Aria2Next::from_yaml(id, section, handle) {
            Ok(a) => Some(Arc::new(a)),
            Err(e) => {
                tracing::error!("Failed to create downloader {}: {}", id, e);
                None
            }
        },
        _ => None,
    }
}

/// WebUI/OOBE 提交的 JSON 配置（camelCase）创建下载器；type 在 config.type 或独立参数
pub fn create_downloader_from_json(
    id: &str,
    config: &serde_json::Value,
    handle: &ConfigHandle,
) -> Option<Arc<dyn Downloader>> {
    let dtype = config
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    match dtype.as_str() {
        "qbittorrent" | "qbittorrentee" => {
            match qbittorrent::QBittorrent::from_json(id, &dtype, config, handle) {
                Ok(qb) => Some(Arc::new(qb)),
                Err(e) => {
                    tracing::error!("Failed to create downloader {}: {}", id, e);
                    None
                }
            }
        }
        "transmission" => match transmission::Transmission::from_json(id, config, handle) {
            Ok(tr) => Some(Arc::new(tr)),
            Err(e) => {
                tracing::error!("Failed to create downloader {}: {}", id, e);
                None
            }
        },
        "biglybt" => match biglybt::BiglyBT::from_json(id, config, handle) {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                tracing::error!("Failed to create downloader {}: {}", id, e);
                None
            }
        },
        "aria2next" => match aria2next::Aria2Next::from_json(id, config, handle) {
            Ok(a) => Some(Arc::new(a)),
            Err(e) => {
                tracing::error!("Failed to create downloader {}: {}", id, e);
                None
            }
        },
        _ => None,
    }
}

/// 对应上游 IPAddressUtil.remapBanListAddress：
/// 结果集合 = {原地址} ∪ {按配置重映射的更大范围块}（supportRangeBan 且相应开关启用时）。
pub fn remap_ban_address(addr: IpNet, support_range_ban: bool, cfg: &RemapConfig) -> Vec<IpNet> {
    let mut out = vec![addr];
    if !support_range_ban {
        return out;
    }
    match addr {
        IpNet::V4(_) => {
            if cfg.ipv4_enabled {
                if let Some(block) = trunc_net(addr, cfg.ipv4_range) {
                    out.push(block);
                }
            }
        }
        IpNet::V6(_) => {
            if cfg.ipv6_enabled {
                if let Some(block) = trunc_net(addr, cfg.ipv6_range) {
                    out.push(block);
                }
            }
        }
    }
    out
}

fn trunc_net(addr: IpNet, prefix: u8) -> Option<IpNet> {
    if addr.prefix_len() <= prefix {
        Some(addr)
    } else {
        IpNet::new(addr.addr(), prefix).ok().map(|n| n.trunc())
    }
}
