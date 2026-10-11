//! Transmission 适配器（对应上游 downloader/impl/transmission + vendored cordelia）。
//!
//! 协议要点（对齐 PBH v9.5.1 行为）：
//! - JSON-RPC POST {endpoint}{rpc-url}，409 → 记录 X-Transmission-Session-Id 重试
//! - 版本要求 ≥ 4.1.0（login 时校验；沙盒测试可用环境变量
//!   PBH_RS_TR_BYPASS_VERSION_CHECK=1 旁路，生产请勿使用）
//! - 封禁机制：session-set 配置 blocklist-url 指向 PBH 的
//!   {server.prefix}/blocklist/p2p-plain-format，setBanList → blocklist-update RPC
//! - peers 直接来自 torrent-get 的 peers 字段（peer_id 为 base64，rate/bytes 字段
//!   可为 null → -1，与上游 TRPeer 一致；-1 会使 handshaking 判定为 true，同为上游行为）
use anyhow::{bail, Result};
use async_trait::async_trait;
use base64::Engine;
use ipnet::IpNet;
use parking_lot::RwLock;
use pbh_config::ConfigHandle;
use pbh_core::model::{BanMetadata, Peer, PeerAddress, Torrent};
use pbh_core::translate::TranslationComponent;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::{Downloader, DownloaderLoginResult, DownloaderStatistics, LoginStatus};

#[derive(Clone, Debug)]
pub struct TrConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub rpc_url: String,
    pub username: String,
    pub password: String,
    pub verify_ssl: bool,
    pub ignore_private: bool,
    pub paused: bool,
}

pub struct Transmission {
    cfg: TrConfig,
    blocklist_url: String,
    http: Client,
    session_id: RwLock<String>,
    tag: AtomicU64,
    paused: AtomicBool,
}

impl Transmission {
    fn rpc_endpoint(&self) -> String {
        format!("{}{}", self.cfg.endpoint.trim_end_matches('/'), self.cfg.rpc_url)
    }

    pub fn from_yaml(
        id: &str,
        section: &serde_yaml::Value,
        handle: &ConfigHandle,
    ) -> Result<Self> {
        let get = |p: &str| pbh_config::yaml_get(section, p);
        let endpoint = get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        if endpoint.is_empty() {
            bail!("downloader {}: missing endpoint", id);
        }
        let cfg = TrConfig {
            id: id.to_string(),
            name: get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string(),
            endpoint,
            rpc_url: get("rpc-url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("/transmission/rpc")
                .to_string(),
            username: get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            password: get("password").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            verify_ssl: get("verify-ssl").and_then(|v| v.as_bool()).unwrap_or(true),
            ignore_private: get("ignore-private").and_then(|v| v.as_bool()).unwrap_or(false),
            paused: get("paused").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        Self::build(cfg, handle)
    }

    pub fn from_json(id: &str, config: &Value, handle: &ConfigHandle) -> Result<Self> {
        let g = |k: &str| config.get(k);
        let endpoint = g("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        if endpoint.is_empty() {
            bail!("downloader {}: missing endpoint", id);
        }
        let cfg = TrConfig {
            id: id.to_string(),
            name: g("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string(),
            endpoint,
            rpc_url: g("rpcUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("/transmission/rpc")
                .to_string(),
            username: g("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            password: g("password").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            verify_ssl: g("verifySsl").and_then(|v| v.as_bool()).unwrap_or(true),
            ignore_private: g("ignorePrivate").and_then(|v| v.as_bool()).unwrap_or(false),
            paused: g("paused").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        Self::build(cfg, handle)
    }

    fn build(cfg: TrConfig, handle: &ConfigHandle) -> Result<Self> {
        let prefix = handle.get().main.get_str("server.prefix", "http://127.0.0.1:9898");
        let blocklist_url = format!(
            "{}/blocklist/p2p-plain-format",
            prefix.trim_end_matches('/')
        );
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
            blocklist_url,
            http: builder.build()?,
            session_id: RwLock::new(String::new()),
            tag: AtomicU64::new(1),
            paused: AtomicBool::new(paused),
        })
    }

    async fn rpc(&self, method: &str, args: Value) -> Result<Value> {
        let url = self.rpc_endpoint();
        let tag = self.tag.fetch_add(1, Ordering::Relaxed);
        let mut last_err = "unknown".to_string();
        for attempt in 0..3u32 {
            let mut rb = self
                .http
                .post(&url)
                .json(&json!({ "method": method, "arguments": args, "tag": tag }));
            if !self.cfg.username.is_empty() {
                rb = rb.basic_auth(&self.cfg.username, Some(&self.cfg.password));
            }
            let sid = self.session_id.read().clone();
            if !sid.is_empty() {
                rb = rb.header("X-Transmission-Session-Id", &sid);
            }
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("{}: {}", e_type_name(&e), e);
                    continue;
                }
            };
            if resp.status().as_u16() == 409 {
                if let Some(new_sid) = resp
                    .headers()
                    .get("X-Transmission-Session-Id")
                    .and_then(|v| v.to_str().ok())
                {
                    *self.session_id.write() = new_sid.to_string();
                }
                if attempt == 0 {
                    continue;
                }
            }
            let body: Value = resp.json().await.map_err(|e| anyhow::anyhow!(e))?;
            let result = body.get("result").and_then(|r| r.as_str()).unwrap_or("");
            if result != "success" {
                bail!("transmission rpc error: {}", result);
            }
            return Ok(body.get("arguments").cloned().unwrap_or(Value::Null));
        }
        bail!("transmission rpc failed: {}", last_err)
    }

    async fn set_blocklist_url(&self, url: &str) -> bool {
        self.rpc(
            "session-set",
            json!({ "blocklist-url": url, "blocklist-enabled": true }),
        )
        .await
        .is_ok()
    }

    async fn update_blocklist(&self) -> bool {
        self.rpc("blocklist-update", json!({})).await.is_ok()
    }

    fn version_ok(version: &str) -> bool {
        if std::env::var("PBH_RS_TR_BYPASS_VERSION_CHECK").is_ok() {
            return true;
        }
        // 上游：截取前 5 字符后按 loose semver 解析，要求 ≥ 4.1.0
        let v = if version.len() > 5 { &version[..5] } else { version };
        let parts: Vec<&str> = v.split('.').collect();
        let major: i64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: i64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        major > 4 || (major == 4 && minor >= 1)
    }
}

fn e_type_name(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "TimeoutException"
    } else if e.is_connect() {
        "ConnectException"
    } else {
        "IOException"
    }
}

#[async_trait]
impl Downloader for Transmission {
    fn id(&self) -> &str {
        &self.cfg.id
    }
    fn name(&self) -> &str {
        &self.cfg.name
    }
    fn downloader_type(&self) -> &str {
        "Transmission"
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
        let args = match self.rpc("session-get", json!({})).await {
            Ok(a) => a,
            Err(e) => {
                return DownloaderLoginResult {
                    status: LoginStatus::Exception,
                    message: TranslationComponent::strs(
                        "DOWNLOADER_LOGIN_IO_EXCEPTION",
                        vec![e.to_string()],
                    ),
                }
            }
        };
        let version = args.get("version").and_then(|v| v.as_str()).unwrap_or("0.0.0");
        if !Self::version_ok(version) {
            return DownloaderLoginResult {
                status: LoginStatus::Exception,
                message: TranslationComponent::strs(
                    "DOWNLOADER_TR_KNOWN_INCOMPATIBILITY",
                    vec![
                        "Transmission version must be 4.1.0 or higher (got: ".to_string()
                            + version
                            + ")",
                    ],
                ),
            };
        }
        let bl_enabled = args
            .get("blocklist-enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let bl_url = args
            .get("blocklist-url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !bl_enabled || !bl_url.starts_with(&self.blocklist_url) {
            let with_ts = format!("{}?t={}", self.blocklist_url, pbh_core::model::Millis::now().millis());
            if !self.set_blocklist_url(&with_ts).await {
                return DownloaderLoginResult {
                    status: LoginStatus::Exception,
                    message: TranslationComponent::strs(
                        "DOWNLOADER_TR_INCORRECT_BANLIST_API_RESP",
                        vec!["Unable to set Transmission blocklist".to_string()],
                    ),
                };
            }
            if !self.update_blocklist().await {
                let _ = self
                    .set_blocklist_url(
                        "http://peerbanhelper-blocklist-update-failed.com/check-peerbanhelper-webui-prefix-settings",
                    )
                    .await;
                return DownloaderLoginResult {
                    status: LoginStatus::RequireTakeActions,
                    message: TranslationComponent::strs(
                        "DOWNLOADER_TRANSMISSION_BLOCKLIST_UPDATE_FAILED",
                        vec![self.cfg.name.clone()],
                    ),
                };
            }
        }
        DownloaderLoginResult::success()
    }

    async fn get_torrents(&self) -> Result<Vec<Torrent>> {
        let fields = [
            "id",
            "hashString",
            "name",
            "peersConnected",
            "status",
            "totalSize",
            "peers",
            "rateDownload",
            "rateUpload",
            "peer-limit",
            "percentDone",
            "sizeWhenDone",
            "trackerList",
            "trackerStats",
            "isPrivate",
        ];
        let args = self
            .rpc("torrent-get", json!({ "fields": fields }))
            .await?;
        let torrents = args.get("torrents").and_then(|t| t.as_array());
        let Some(list) = torrents else {
            return Ok(Vec::new());
        };
        let include_private = !self.cfg.ignore_private;
        let mut out = Vec::new();
        for t in list {
            let rate_dl = t.get("rateDownload").and_then(|v| v.as_i64()).unwrap_or(0);
            let rate_ul = t.get("rateUpload").and_then(|v| v.as_i64()).unwrap_or(0);
            let peers_connected = t.get("peersConnected").and_then(|v| v.as_i64()).unwrap_or(0);
            // 上游 active 过滤：有传输或有连接
            if rate_dl <= 0 && rate_ul <= 0 && peers_connected <= 0 {
                continue;
            }
            let is_private = t.get("isPrivate").and_then(|v| v.as_bool()).unwrap_or(false);
            if !include_private && is_private {
                continue;
            }
            let percent_done = t.get("percentDone").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let size_when_done = t.get("sizeWhenDone").and_then(|v| v.as_i64()).unwrap_or(0);
            let id_num = t.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let peers = parse_tr_peers(t.get("peers"));
            out.push(Torrent {
                id: id_num.to_string(),
                name: t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                hash: t
                    .get("hashString")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                size: t.get("totalSize").and_then(|v| v.as_i64()).unwrap_or(0),
                completed_size: (size_when_done as f64 * percent_done) as i64,
                progress: percent_done,
                rt_upload_speed: rate_ul,
                rt_download_speed: rate_dl,
                private_torrent: is_private,
                piece_size: 0,
                pieces_have: 0,
                attached_peers: peers,
            });
        }
        Ok(out)
    }

    async fn get_peers(&self, torrent: &Torrent) -> Result<Vec<Peer>> {
        // 上游 TRTorrent.getPeers：直接使用 torrent-get 附带的 peers
        Ok(torrent.attached_peers.clone())
    }

    async fn set_ban_list(
        &self,
        _full: &[IpNet],
        _added: &[BanMetadata],
        _removed_empty: bool,
        _apply_full: bool,
    ) -> Result<()> {
        // 上游：仅触发 blocklist-update（列表内容由 /blocklist/p2p-plain-format 提供）
        let resp = self.rpc("blocklist-update", json!({})).await?;
        let size = resp.get("blocklist-size").and_then(|v| v.as_i64()).unwrap_or(0);
        tracing::info!("[{}] Transmission blocklist updated, size: {}", self.cfg.name, size);
        Ok(())
    }

    async fn statistics(&self) -> Result<DownloaderStatistics> {
        let args = self.rpc("session-stats", json!({})).await?;
        let cum = args.get("cumulative-stats").cloned().unwrap_or(Value::Null);
        Ok(DownloaderStatistics {
            total_uploaded: cum.get("uploadedBytes").and_then(|v| v.as_i64()).unwrap_or(0),
            total_downloaded: cum.get("downloadedBytes").and_then(|v| v.as_i64()).unwrap_or(0),
        })
    }

    fn has_feature(&self, flag: &str) -> bool {
        matches!(flag, "UNBAN_IP" | "TRAFFIC_STATS" | "LIVE_UPDATE_BT_PROTOCOL_PORT")
    }

    fn save_config(&self) -> Vec<(String, pbh_config::YValue)> {
        use pbh_config::YValue;
        vec![
            ("type".into(), YValue::Str("transmission".into())),
            ("name".into(), YValue::Str(self.cfg.name.clone())),
            ("endpoint".into(), YValue::Str(self.cfg.endpoint.clone())),
            ("rpc-url".into(), YValue::Str(self.cfg.rpc_url.clone())),
            ("username".into(), YValue::Str(self.cfg.username.clone())),
            ("password".into(), YValue::Str(self.cfg.password.clone())),
            ("verify-ssl".into(), YValue::Bool(self.cfg.verify_ssl)),
            ("ignore-private".into(), YValue::Bool(self.cfg.ignore_private)),
            ("paused".into(), YValue::Bool(self.is_paused())),
        ]
    }

    fn config_json(&self) -> Value {
        json!({
            "type": "transmission",
            "name": self.cfg.name,
            "endpoint": self.cfg.endpoint,
            "rpcUrl": self.cfg.rpc_url,
            "username": self.cfg.username,
            "password": self.cfg.password,
            "verifySsl": self.cfg.verify_ssl,
            "ignorePrivate": self.cfg.ignore_private,
            "paused": self.is_paused(),
        })
    }
}

/// 上游 TRPeer 字段映射（bytes/rate 可为 null → -1；peer_id 为 base64 → ISO-8859-1）
fn parse_tr_peers(v: Option<&Value>) -> Vec<Peer> {
    let Some(arr) = v.and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for p in arr {
        let address = p.get("address").and_then(|a| a.as_str()).unwrap_or("");
        if address.is_empty() {
            continue;
        }
        let port = p.get("port").and_then(|a| a.as_u64()).unwrap_or(0) as u16;
        let rate_to_client = p.get("rateToClient").and_then(|a| a.as_i64()).unwrap_or(-1);
        let rate_to_peer = p.get("rateToPeer").and_then(|a| a.as_i64()).unwrap_or(-1);
        let downloaded = p.get("bytes_to_client").and_then(|a| a.as_i64()).unwrap_or(-1);
        let uploaded = p.get("bytes_to_peer").and_then(|a| a.as_i64()).unwrap_or(-1);
        let peer_id = p
            .get("peer_id")
            .and_then(|a| a.as_str())
            .map(|b64| {
                String::from_utf8_lossy(
                    &base64::engine::general_purpose::STANDARD.decode(b64).unwrap_or_default(),
                )
                .to_string()
            })
            .unwrap_or_default();
        out.push(Peer {
            address: PeerAddress::new(address, port, address),
            peer_id: Some(peer_id),
            client_name: p
                .get("clientName")
                .and_then(|a| a.as_str())
                .map(|s| s.to_string()),
            download_speed: rate_to_client,
            downloaded,
            upload_speed: rate_to_peer,
            uploaded,
            progress: p.get("progress").and_then(|a| a.as_f64()).unwrap_or(0.0),
            flags: p.get("flagStr").and_then(|a| a.as_str()).map(|s| s.to_string()),
            // 上游：dl<=0 && up<=0（-1 缺省也视为 handshaking，保持一致）
            handshaking: rate_to_client <= 0 && rate_to_peer <= 0,
        });
    }
    out
}
