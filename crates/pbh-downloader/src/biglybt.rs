//! BiglyBT 适配器（对应上游 downloader/impl/biglybt，依赖 PBH-Adapter-BiglyBT 插件 ≥1.3.0）。
//!
//! 协议（HTTP JSON，Bearer token）：
//! - GET /metadata → {pluginVersion,...}（版本 ≥1.3.0）；POST /setconnector（尽力而为）
//! - GET /downloads?filter=4&filter=5&filter=8（下载中/做种/错误）→ [DownloadRecord]
//!   活跃过滤：rtDownloadSpeed>0 || rtUploadSpeed>0（上游 getTorrents 语义）
//! - GET /download/{infoHash}/peers → {peers:[PeerRecord]}（404 → 空表）
//! - 封禁：增量 POST /bans {"ips":[...]}；全量 PUT /bans {"replaceWith":[...],"includeNonPBHEntries":false}
//!   （两种路径均先做 banlist-remapping，对应上游 remapBanListAddress(…, true)）
//! - GET /statistics → {overallDataBytesSent, overallDataBytesReceived}
use anyhow::{bail, Result};
use async_trait::async_trait;
use ipnet::IpNet;
use parking_lot::RwLock;
use std::sync::atomic::AtomicBool;
use pbh_config::{ConfigHandle, YValue};
use pbh_core::model::{BanMetadata, Peer, PeerAddress, Torrent};
use pbh_core::translate::TranslationComponent;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::{remap_ban_address, Downloader, DownloaderLoginResult, DownloaderStatistics, LoginStatus};

#[derive(Clone, Debug)]
pub struct BiglyBTConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub token: String,
    pub increment_ban: bool,
    pub verify_ssl: bool,
    pub ignore_private: bool,
    pub paused: bool,
}

#[derive(Deserialize)]
struct MetadataBean {
    #[serde(default, rename = "pluginVersion")]
    plugin_version: String,
}

#[derive(Deserialize)]
struct TorrentStats {
    #[serde(default, rename = "remainingBytes")]
    remaining_bytes: i64,
    #[serde(default, rename = "completedInThousandNotation")]
    completed_in_thousand_notation: i64,
    #[serde(default, rename = "rtUploadSpeed")]
    rt_upload_speed: i64,
    #[serde(default, rename = "rtDownloadSpeed")]
    rt_download_speed: i64,
}

#[derive(Deserialize)]
struct TorrentRecord {
    #[serde(default, rename = "infoHash")]
    info_hash: String,
    #[serde(default)]
    size: i64,
    #[serde(default, rename = "privateTorrent")]
    private_torrent: bool,
}

#[derive(Deserialize)]
struct DownloadRecord {
    #[serde(default)]
    name: String,
    #[serde(default)]
    torrent: Option<TorrentRecord>,
    #[serde(default)]
    stats: Option<TorrentStats>,
}

#[derive(Deserialize)]
struct PeerStats {
    #[serde(default, rename = "rtDownloadSpeed")]
    rt_download_speed: i64,
    #[serde(default, rename = "totalReceived")]
    total_received: i64,
    #[serde(default, rename = "rtUploadSpeed")]
    rt_upload_speed: i64,
    #[serde(default, rename = "totalSent")]
    total_sent: i64,
}

#[derive(Deserialize)]
struct PeerRecord {
    #[serde(default)]
    ip: String,
    #[serde(default)]
    port: u16,
    #[serde(default, rename = "peerId")]
    peer_id: String,
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    stats: Option<PeerStats>,
    #[serde(default, rename = "percentDoneInThousandNotation")]
    percent_done_in_thousand_notation: i64,
    #[serde(default)]
    state: i64,
    #[serde(default)]
    incoming: bool,
}

#[derive(Deserialize)]
struct PeerManagerRecord {
    #[serde(default)]
    peers: Vec<PeerRecord>,
}

#[derive(Deserialize)]
struct StatisticsRecord {
    #[serde(default, rename = "overallDataBytesSent")]
    overall_sent: i64,
    #[serde(default, rename = "overallDataBytesReceived")]
    overall_received: i64,
}

pub struct BiglyBT {
    cfg: BiglyBTConfig,
    http: Client,
    handle: ConfigHandle,
    paused: AtomicBool,
    _last_meta: RwLock<Option<String>>,
}

impl BiglyBT {
    pub fn from_yaml(id: &str, section: &serde_yaml::Value, handle: &ConfigHandle) -> Result<Self> {
        let get = |p: &str| pbh_config::yaml_get(section, p);
        let endpoint = get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        if endpoint.is_empty() {
            bail!("downloader {}: missing endpoint", id);
        }
        let cfg = BiglyBTConfig {
            id: id.to_string(),
            name: get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or(id).to_string(),
            endpoint,
            token: get("token").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            increment_ban: get("increment-ban").and_then(|v| v.as_bool()).unwrap_or(true),
            verify_ssl: get("verify-ssl").and_then(|v| v.as_bool()).unwrap_or(true),
            ignore_private: get("ignore-private").and_then(|v| v.as_bool()).unwrap_or(false),
            paused: get("paused").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        Self::build(cfg, handle)
    }

    pub fn from_json(id: &str, config: &Value, handle: &ConfigHandle) -> Result<Self> {
        let g = |k: &str| config.get(k);
        let endpoint = g("endpoint").and_then(|v| v.as_str()).unwrap_or("").trim_end_matches('/').to_string();
        if endpoint.is_empty() {
            bail!("downloader {}: missing endpoint", id);
        }
        let cfg = BiglyBTConfig {
            id: id.to_string(),
            name: g("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or(id).to_string(),
            endpoint,
            token: g("token").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            increment_ban: g("incrementBan").and_then(|v| v.as_bool()).unwrap_or(true),
            verify_ssl: g("verifySsl").and_then(|v| v.as_bool()).unwrap_or(true),
            ignore_private: g("ignorePrivate").and_then(|v| v.as_bool()).unwrap_or(false),
            paused: g("paused").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        Self::build(cfg, handle)
    }

    fn build(cfg: BiglyBTConfig, handle: &ConfigHandle) -> Result<Self> {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("PeerBanHelper-RS/", env!("CARGO_PKG_VERSION")));
        if !cfg.verify_ssl {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let paused = cfg.paused;
        Ok(Self {
            cfg,
            http: builder.build()?,
            handle: handle.clone(),
            paused: AtomicBool::new(paused),
            _last_meta: RwLock::new(None),
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.cfg.endpoint, path))
            .header("Content-Type", "application/json")
            .bearer_auth(&self.cfg.token)
    }

    /// hex 字符串 → ISO-8859-1 字符串（对应 ByteUtil.hexToByteArray + new String）
    fn hex_to_latin1(hex: &str) -> String {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();
        bytes.iter().map(|b| *b as char).collect()
    }
}

#[async_trait]
impl Downloader for BiglyBT {
    fn id(&self) -> &str {
        &self.cfg.id
    }
    fn name(&self) -> &str {
        &self.cfg.name
    }
    fn downloader_type(&self) -> &str {
        "BiglyBT"
    }
    fn endpoint(&self) -> &str {
        &self.cfg.endpoint
    }
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    async fn login(&self) -> DownloaderLoginResult {
        let resp = match self.req(reqwest::Method::GET, "/metadata").send().await {
            Ok(r) => r,
            Err(e) => {
                return DownloaderLoginResult {
                    status: LoginStatus::NetworkError,
                    message: TranslationComponent::strs(
                        "DOWNLOADER_LOGIN_IO_EXCEPTION",
                        vec![format!("IOException: {}", e)],
                    ),
                }
            }
        };
        let status = resp.status();
        if status.is_success() {
            let meta: MetadataBean = match resp.json().await {
                Ok(m) => m,
                Err(e) => {
                    return DownloaderLoginResult {
                        status: LoginStatus::Exception,
                        message: TranslationComponent::strs(
                            "DOWNLOADER_LOGIN_EXCEPTION",
                            vec![e.to_string()],
                        ),
                    }
                }
            };
            let ok = meta
                .plugin_version
                .split('.')
                .take(2)
                .collect::<Vec<_>>()
                .join(".")
                .parse::<f64>()
                .map(|v| v >= 1.3)
                .unwrap_or(false)
                || meta.plugin_version.starts_with("1.3")
                || meta
                    .plugin_version
                    .split('.')
                    .next()
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|major| major > 1)
                    .unwrap_or(false);
            if !ok {
                return DownloaderLoginResult {
                    status: LoginStatus::RequireTakeActions,
                    message: TranslationComponent::strs(
                        "DOWNLOADER_BIGLYBT_INCORRECT_ADAPTER_VERSION",
                        vec!["1.3.0".to_string()],
                    ),
                };
            }
            {
                *self._last_meta.write() = Some(meta.plugin_version);
            }
            // setconnector：尽力而为（失败不影响登录）
            let payload = json!({
                "name": "PeerBanHelper-RS",
                "version": env!("CARGO_PKG_VERSION"),
            });
            let _ = self
                .req(reqwest::Method::POST, "/setconnector")
                .json(&payload)
                .send()
                .await;
            return DownloaderLoginResult::success();
        }
        if status.as_u16() == 403 {
            return DownloaderLoginResult {
                status: LoginStatus::IncorrectCredential,
                message: TranslationComponent::new("DOWNLOADER_LOGIN_INCORRECT_CRED"),
            };
        }
        DownloaderLoginResult {
            status: LoginStatus::Exception,
            message: TranslationComponent::strs(
                "DOWNLOADER_LOGIN_EXCEPTION",
                vec![format!("statusCode={}", status.as_u16())],
            ),
        }
    }

    async fn get_torrents(&self) -> Result<Vec<Torrent>> {
        // 4=DOWNLOADING 5=SEEDING 8=ERROR
        let resp = self
            .req(reqwest::Method::GET, "/downloads?filter=4&filter=5&filter=8")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "BiglyBT incorrect response: statusCode={} body={}",
                status.as_u16(),
                body
            );
        }
        let records: Vec<DownloadRecord> = resp.json().await?;
        let include_private = !self.cfg.ignore_private;
        let mut out = Vec::new();
        for r in records {
            let (Some(t), Some(s)) = (r.torrent, r.stats) else {
                continue;
            };
            if s.rt_download_speed <= 0 && s.rt_upload_speed <= 0 {
                continue; // 上游活跃过滤
            }
            if !include_private && t.private_torrent {
                continue;
            }
            out.push(Torrent {
                id: t.info_hash.clone(),
                name: r.name,
                hash: t.info_hash,
                size: t.size,
                completed_size: t.size - s.remaining_bytes,
                progress: s.completed_in_thousand_notation as f64 / 1000.0,
                rt_upload_speed: s.rt_upload_speed,
                rt_download_speed: s.rt_download_speed,
                private_torrent: t.private_torrent,
                piece_size: 0,
                pieces_have: 0,
                attached_peers: Vec::new(),
            });
        }
        Ok(out)
    }

    async fn get_peers(&self, torrent: &Torrent) -> Result<Vec<Peer>> {
        let resp = self
            .req(
                reqwest::Method::GET,
                &format!("/download/{}/peers", torrent.id),
            )
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(Vec::new()); // 种子被删除/错误
        }
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("BiglyBT peers request failed: {} {}", status, body);
        }
        let mgr: PeerManagerRecord = resp.json().await?;
        let mut out = Vec::with_capacity(mgr.peers.len());
        for p in mgr.peers {
            if p.ip.is_empty() {
                continue;
            }
            let ip = p.ip.strip_prefix('/').unwrap_or(&p.ip).to_string();
            let (dl, up, downloaded, uploaded) = match &p.stats {
                Some(s) => (
                    s.rt_download_speed,
                    s.rt_upload_speed,
                    s.total_received,
                    s.total_sent,
                ),
                None => (0, 0, 0, 0),
            };
            let peer_id = if p.peer_id.is_empty() {
                None
            } else {
                Some(Self::hex_to_latin1(&p.peer_id))
            };
            let mut flags = String::new();
            if p.incoming {
                flags.push_str("I ");
            }
            out.push(Peer {
                address: PeerAddress::new(ip.clone(), p.port, ip),
                peer_id,
                client_name: p.client.clone(),
                download_speed: dl,
                downloaded,
                upload_speed: up,
                uploaded,
                progress: p.percent_done_in_thousand_notation as f64 / 1000.0,
                flags: if flags.is_empty() { None } else { Some(flags.trim().to_string()) },
                // 上游：state != 30 && state != 40 视为握手中
                handshaking: p.state != 30 && p.state != 40,
            });
            let _ = &p.incoming;
        }
        Ok(out)
    }

    async fn set_ban_list(
        &self,
        full: &[IpNet],
        added: &[BanMetadata],
        removed_empty: bool,
        apply_full: bool,
    ) -> Result<()> {
        let remap_cfg = self.handle.get().remap_config();
        if removed_empty && !added.is_empty() && self.cfg.increment_ban && !apply_full {
            let ips: Vec<String> = {
                let mut seen = HashSet::new();
                added
                    .iter()
                    .filter_map(|b| b.peer.address.ip_addr())
                    .flat_map(|ip| {
                        remap_ban_address(pbh_core::model::host_net(ip), true, &remap_cfg)
                    })
                    .map(|n| n.addr().to_string())
                    .filter(|s| seen.insert(s.clone()))
                    .collect()
            };
            let resp = self
                .req(reqwest::Method::POST, "/bans")
                .json(&json!({ "ips": ips }))
                .send()
                .await?;
            if !resp.status().is_success() {
                bail!("Save BiglyBT banlist error: statusCode={}", resp.status());
            }
            return Ok(());
        }
        let replace_with: Vec<String> = {
            let mut seen = HashSet::new();
            full.iter()
                .flat_map(|n| remap_ban_address(*n, true, &remap_cfg))
                .map(|n| {
                    if n.prefix_len()
                        == match n {
                            IpNet::V4(_) => 32,
                            IpNet::V6(_) => 128,
                        }
                    {
                        n.addr().to_string()
                    } else {
                        n.to_string()
                    }
                })
                .filter(|s| seen.insert(s.clone()))
                .collect()
        };
        let resp = self
            .req(reqwest::Method::PUT, "/bans")
            .json(&json!({ "replaceWith": replace_with, "includeNonPBHEntries": false }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("Save BiglyBT banlist error: statusCode={}", resp.status());
        }
        Ok(())
    }

    async fn statistics(&self) -> Result<DownloaderStatistics> {
        let resp = self.req(reqwest::Method::GET, "/statistics").send().await?;
        if !resp.status().is_success() {
            bail!("Failed to request statistics: {}", resp.status());
        }
        let s: StatisticsRecord = resp.json().await?;
        Ok(DownloaderStatistics {
            total_uploaded: s.overall_sent,
            total_downloaded: s.overall_received,
        })
    }

    fn support_range_ban(&self) -> bool {
        true
    }

    fn has_feature(&self, flag: &str) -> bool {
        matches!(flag, "UNBAN_IP" | "TRAFFIC_STATS" | "RANGE_BAN_IP")
    }

    fn save_config(&self) -> Vec<(String, YValue)> {
        vec![
            ("type".into(), YValue::Str("biglybt".into())),
            ("name".into(), YValue::Str(self.cfg.name.clone())),
            ("endpoint".into(), YValue::Str(self.cfg.endpoint.clone())),
            ("token".into(), YValue::Str(self.cfg.token.clone())),
            ("increment-ban".into(), YValue::Bool(self.cfg.increment_ban)),
            ("verify-ssl".into(), YValue::Bool(self.cfg.verify_ssl)),
            ("ignore-private".into(), YValue::Bool(self.cfg.ignore_private)),
            ("paused".into(), YValue::Bool(self.is_paused())),
        ]
    }

    fn config_json(&self) -> Value {
        json!({
            "type": "biglybt",
            "name": self.cfg.name,
            "endpoint": self.cfg.endpoint,
            "token": self.cfg.token,
            "incrementBan": self.cfg.increment_ban,
            "verifySsl": self.cfg.verify_ssl,
            "ignorePrivate": self.cfg.ignore_private,
            "paused": self.is_paused(),
        })
    }
}
