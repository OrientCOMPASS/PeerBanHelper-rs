//! 领域模型。JSON 序列化形状与上游 Java 版（Gson，null 省略，时间为 epoch 毫秒）保持一致，
//! 以便：1) 直接接管上游 sqlite 库中 `banlist.metadata` 的 JSON；2) WebUI 零改动兼容。
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv6Addr};

use crate::translate::TranslationComponent;
use ipnet::IpNet;

/// epoch 毫秒时间（对应 Java OffsetDateTime + OffsetDateTimeTypeAdapter）
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub DateTime<Utc>);

impl Millis {
    pub fn now() -> Self {
        Millis(Utc::now())
    }
    pub fn from_millis(ms: i64) -> Self {
        Millis(
            Utc.timestamp_millis_opt(ms)
                .single()
                .unwrap_or_else(|| Utc.timestamp_millis_opt(0).unwrap()),
        )
    }
    pub fn millis(&self) -> i64 {
        self.0.timestamp_millis()
    }
    pub fn add_ms(&self, ms: i64) -> Self {
        Millis::from_millis(self.millis().saturating_add(ms))
    }
}

impl Serialize for Millis {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(self.millis())
    }
}

impl<'de> Deserialize<'de> for Millis {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let ms = i64::deserialize(d)?;
        Ok(Millis::from_millis(ms))
    }
}

/// 对应 Java `wrapper.PeerAddress`（Gson 输出字段名一致；可空 String 省略，int 恒输出）
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerAddress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloader_raw_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloader_raw_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teredo_client_ip: Option<String>,
    #[serde(default)]
    pub teredo_client_udp_port: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub natted_client_ip: Option<String>,
    #[serde(default)]
    pub natted_client_port: i64,
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub nat_translated: bool,
    #[serde(default)]
    pub teredo_translated: bool,
}

impl PeerAddress {
    pub fn new(ip: impl Into<String>, port: u16, raw_ip: impl Into<String>) -> Self {
        let ip = ip.into();
        Self {
            downloader_raw_ip: Some(raw_ip.into()),
            downloader_raw_port: Some(port),
            teredo_client_ip: None,
            teredo_client_udp_port: 0,
            natted_client_ip: None,
            natted_client_port: 0,
            port,
            nat_translated: false,
            teredo_translated: false,
            ip,
        }
    }

    pub fn ip_addr(&self) -> Option<IpAddr> {
        self.ip.parse::<IpAddr>().ok()
    }

    /// 主机前缀块（/32 或 /128），对应 IPAddress.toPrefixBlock()
    pub fn host_net(&self) -> Option<IpNet> {
        self.ip_addr().map(host_net)
    }
}

pub fn host_net(ip: IpAddr) -> IpNet {
    match ip {
        IpAddr::V4(v4) => IpNet::V4(ipnet::Ipv4Net::new(v4, 32).unwrap()),
        IpAddr::V6(v6) => IpNet::V6(ipnet::Ipv6Net::new(v6, 128).unwrap()),
    }
}

/// 运行期 Peer（不直接序列化到 API；API 使用 PeerWrapper）
#[derive(Clone, Debug)]
pub struct Peer {
    pub address: PeerAddress,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub download_speed: i64,
    pub downloaded: i64,
    pub upload_speed: i64,
    pub uploaded: i64,
    pub progress: f64,
    /// libtorrent 标准 flags 字符串
    pub flags: Option<String>,
    pub handshaking: bool,
}

impl Peer {
    pub fn wrapper(&self) -> PeerWrapper {
        PeerWrapper {
            address: self.address.clone(),
            raw_ip: self.address.downloader_raw_ip.clone(),
            id: self.peer_id.clone(),
            client_name: self.client_name.clone(),
            downloaded: self.downloaded,
            download_speed: self.download_speed,
            uploaded: self.uploaded,
            upload_speed: self.upload_speed,
            progress: self.progress,
            flags: self.flags.clone(),
        }
    }

    pub fn cache_key(&self) -> String {
        format!("{}:{}", self.address.ip, self.address.port)
    }
}

/// 对应 Java `wrapper.PeerWrapper`（字段顺序与 Gson 一致）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerWrapper {
    pub address: PeerAddress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    pub downloaded: i64,
    pub download_speed: i64,
    pub uploaded: i64,
    pub upload_speed: i64,
    pub progress: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<String>,
}

impl PeerWrapper {
    pub fn to_peer_address(&self) -> PeerAddress {
        PeerAddress::new(
            self.address.ip.clone(),
            self.address.port,
            self.raw_ip.clone().unwrap_or_else(|| self.address.ip.clone()),
        )
    }
}

/// 运行期 Torrent
#[derive(Clone, Debug)]
pub struct Torrent {
    pub id: String,
    pub name: String,
    pub hash: String,
    pub size: i64,
    pub completed_size: i64,
    pub progress: f64,
    pub rt_upload_speed: i64,
    pub rt_download_speed: i64,
    pub private_torrent: bool,
    pub piece_size: i64,
    pub pieces_have: i64,
    /// 部分下载器（如 Transmission）在获取种子时一并返回 peers（对应上游 TRTorrent）
    pub attached_peers: Vec<Peer>,
}

impl Torrent {
    pub fn wrapper(&self) -> TorrentWrapper {
        TorrentWrapper {
            id: self.id.clone(),
            size: self.size,
            completed_size: self.completed_size,
            name: self.name.clone(),
            hash: self.hash.clone(),
            private_torrent: self.private_torrent,
            progress: self.progress,
            rt_upload_speed: self.rt_upload_speed,
            rt_download_speed: self.rt_download_speed,
        }
    }
}

/// 对应 Java `wrapper.TorrentWrapper`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentWrapper {
    pub id: String,
    pub size: i64,
    pub completed_size: i64,
    pub name: String,
    pub hash: String,
    pub private_torrent: bool,
    pub progress: f64,
    pub rt_upload_speed: i64,
    pub rt_download_speed: i64,
}

/// 对应 Java `downloader.DownloaderBasicInfo` record
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloaderBasicInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub downloader_type: String,
}

/// 对应 Java `wrapper.BanMetadata`（含父类 PeerMetadata 字段，顺序与 Gson 输出一致：
/// 父类字段在前）。用于 banlist 持久化 JSON 与内部传递。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BanMetadata {
    pub downloader: DownloaderBasicInfo,
    pub torrent: TorrentWrapper,
    pub peer: PeerWrapper,
    #[serde(default = "default_reverse_lookup")]
    pub reverse_lookup: String,
    pub context: String,
    #[serde(default)]
    pub random_id: String,
    pub ban_at: Millis,
    pub unban_at: Millis,
    #[serde(default)]
    pub ban_for_disconnect: bool,
    #[serde(default)]
    pub exclude_from_report: bool,
    #[serde(default)]
    pub exclude_from_display: bool,
    pub rule: TranslationComponent,
    pub description: TranslationComponent,
    #[serde(default)]
    pub structured_data: serde_json::Map<String, serde_json::Value>,
}

fn default_reverse_lookup() -> String {
    "N/A".to_string()
}

impl BanMetadata {
    /// banlist 表的地址键（与 Java 侧 InetAddress 字符串互通：主机地址不带前缀）
    pub fn address_key(ip: IpAddr) -> String {
        match ip {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => compress_v6(v6),
        }
    }
}

/// IPv6 压缩表示（对应 IPAddress.toCompressedString：去掉前导零、小写）
pub fn compress_v6(v6: Ipv6Addr) -> String {
    v6.to_string()
}

pub fn compress_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => compress_v6(v6),
    }
}

/// IPv4-mapped IPv6 转 IPv4（对应 IPAddress.isIPv4Convertible/toIPv4 的常见情形）
pub fn try_v4(ip: IpAddr) -> Option<IpAddr> {
    match ip {
        IpAddr::V4(_) => Some(ip),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4),
    }
}
