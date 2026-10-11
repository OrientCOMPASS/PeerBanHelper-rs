//! qBittorrent / qBittorrentEE 适配器。
//! 协议行为对齐上游 `AbstractQbittorrent`（v9.5.1）：
//! - 登录：POST /api/v2/auth/login（表单，SID cookie）或 API-Key（≥5.2.0，Bearer，跳过 auth 端点）；
//!   登录状态验证：GET /app/buildInfo（libtorrent 字段非空）；版本要求 ≥4.5.0（API-Key ≥5.2.0）
//! - 首次成功后：POST /app/setPreferences 关闭「允许来自同一 IP 的多重连接」（默认开关行为）
//! - 种子：GET /torrents/info?filter=active&limit=100&offset=N 分页去重；
//!   属性回填：GET /torrents/properties?hash=（带 TTL 缓存，completed 变化时失效重取）
//! - Peers：GET /sync/torrentPeers?hash=；跳过 connection=HTTP/HTTPS/Web、空 IP、.onion/.i2p；
//!   handshaking = up_speed<=0 && dl_speed<=0
//! - 封禁：增量 POST /transfer/banPeers（peers=rawIp|rawIp…，仅 increment-ban 且无解封时）；
//!   全量 POST /app/setPreferences {"banned_IPs": "…\n…"}（含 banlist-remapping，≥5.3.0 支持范围封禁）
//! - 统计：GET /sync/maindata（alltime 全 0 视为未就绪）
use anyhow::{bail, Result};
use async_trait::async_trait;
use base64::Engine;
use ipnet::IpNet;
use parking_lot::{Mutex, RwLock};
use pbh_config::{ConfigHandle, YValue};
use pbh_core::model::{BanMetadata, Peer, PeerAddress, Torrent};
use pbh_core::translate::TranslationComponent;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use serde_yaml::Value as YamlValue;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{
    remap_ban_address, Downloader, DownloaderLoginResult, DownloaderStatistics, LoginStatus,
};

#[derive(Clone, Debug)]
pub struct QBConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub api_key: String,
    pub basic_auth_user: String,
    pub basic_auth_pass: String,
    pub increment_ban: bool,
    #[allow(dead_code)]
    pub use_shadow_ban: bool,
    pub verify_ssl: bool,
    pub ignore_private: bool,
    pub paused: bool,
    pub downloader_type: String,
}

#[derive(Deserialize)]
struct QBBuildInfo {
    #[serde(default)]
    libtorrent: Option<String>,
}

#[derive(Deserialize, Clone)]
struct QBTorrentInfo {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    dlspeed: i64,
    #[serde(default)]
    upspeed: i64,
    #[serde(default)]
    completed: i64,
    #[serde(default)]
    is_private: Option<bool>,
    #[serde(default)]
    piece_size: Option<i64>,
    #[serde(default)]
    pieces_have: Option<i64>,
}

#[derive(Clone)]
struct TorrentProps {
    is_private: bool,
    completed: i64,
    piece_size: i64,
    pieces_have: i64,
}

pub struct QBittorrent {
    cfg: QBConfig,
    api_endpoint: String,
    http: Client,
    basic_auth_header: Option<String>,
    handle: ConfigHandle,
    props_ttl: Duration,
    props_cache: Mutex<HashMap<String, (Instant, TorrentProps)>>,
    last_version: RwLock<Option<semver::Version>>,
    paused: AtomicBool,
}

impl QBittorrent {
    pub fn from_yaml(
        id: &str,
        dtype: &str,
        section: &YamlValue,
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
        let cfg = QBConfig {
            id: id.to_string(),
            name: get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string(),
            endpoint: endpoint.clone(),
            username: get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            password: get("password").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            api_key: get("api-key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            basic_auth_user: get("basic-auth.user").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            basic_auth_pass: get("basic-auth.pass").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            increment_ban: get("increment-ban").and_then(|v| v.as_bool()).unwrap_or(false),
            use_shadow_ban: get("use-shadow-ban").and_then(|v| v.as_bool()).unwrap_or(false),
            verify_ssl: get("verify-ssl").and_then(|v| v.as_bool()).unwrap_or(true),
            ignore_private: get("ignore-private").and_then(|v| v.as_bool()).unwrap_or(false),
            paused: get("paused").and_then(|v| v.as_bool()).unwrap_or(false),
            downloader_type: if dtype == "qbittorrentee" {
                "qBittorrentEE".to_string()
            } else {
                "qBittorrent".to_string()
            },
        };

        let mut builder = Client::builder()
            .cookie_store(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("PeerBanHelper-RS/", env!("CARGO_PKG_VERSION")));
        if !cfg.verify_ssl {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let http = builder.build()?;

        let basic_auth_header = if !cfg.basic_auth_user.is_empty() {
            let cred = format!("{}:{}", cfg.basic_auth_user, cfg.basic_auth_pass);
            Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(cred)
            ))
        } else {
            None
        };

        let paused_initial = cfg.paused;
        let props_ttl = Duration::from_millis(handle.get().check_interval_ms() + 60_000);
        Ok(Self {
            api_endpoint: format!("{}/api/v2", cfg.endpoint),
            cfg,
            http,
            basic_auth_header,
            handle: handle.clone(),
            props_ttl,
            props_cache: Mutex::new(HashMap::new()),
            last_version: RwLock::new(None),
            paused: AtomicBool::new(paused_initial),
        })
    }

    /// 构造请求：应用 basic-auth 与 API-Key（auth/login、auth/logout 除外，对齐上游拦截器）
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.api_endpoint, path);
        let mut rb = self.http.request(method, &url);
        if let Some(h) = &self.basic_auth_header {
            rb = rb.header(reqwest::header::AUTHORIZATION, h);
        }
        if !self.cfg.api_key.is_empty()
            && !path.contains("/auth/login")
            && !path.contains("/auth/logout")
        {
            rb = rb.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.cfg.api_key),
            );
        }
        rb
    }

    async fn is_logged_in(&self) -> bool {
        let resp = self.req(reqwest::Method::GET, "/app/buildInfo").send().await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<QBBuildInfo>().await {
                Ok(info) => info
                    .libtorrent
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false),
                _ => false,
            },
            _ => false,
        }
    }

    async fn fetch_version(&self) -> Result<semver::Version> {
        let r = self
            .req(reqwest::Method::GET, "/app/version")
            .send()
            .await?;
        if !r.status().is_success() {
            bail!("Failed to get qBittorrent version: statusCode={}", r.status());
        }
        let text = r.text().await?;
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        parse_loose_semver(&normalize_version(text))
    }

    async fn update_preferences(&self) -> Result<()> {
        // 对应开关 pbh.downloader.qbittorrent.disableSameIpMultiConnection（默认 true）
        let json = serde_json::json!({ "enable_multi_connections_from_same_ip": false }).to_string();
        let r = self
            .req(reqwest::Method::POST, "/app/setPreferences")
            .form(&[("json", json.as_str())])
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            bail!("Save qBittorrent preferences error: statusCode={} body={}", status, body);
        }
        Ok(())
    }

    async fn update_and_apply_metadata_if_required(&self) {
        let need = self.last_version.read().is_none();
        if need {
            if let Err(e) = self.update_preferences().await {
                tracing::error!(
                    "[{}] Failed to disable same-IP multi connection: {}",
                    self.cfg.name,
                    e
                );
            }
            match self.fetch_version().await {
                Ok(v) => {
                    tracing::info!("[{}] qBittorrent version: {}", self.cfg.name, v);
                    *self.last_version.write() = Some(v);
                }
                Err(e) => tracing::warn!("[{}] Failed to fetch version: {}", self.cfg.name, e),
            }
        }
    }

    async fn get_torrent_properties(&self, hash: &str, completed: i64) -> Option<TorrentProps> {
        if let Some((at, props)) = self.props_cache.lock().get(hash).cloned() {
            if at.elapsed() < self.props_ttl && props.completed == completed {
                return Some(props);
            }
        }
        let r = self
            .req(
                reqwest::Method::GET,
                &format!("/torrents/properties?hash={}", hash),
            )
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        let detail: QBTorrentInfo = r.json().await.ok()?;
        let props = TorrentProps {
            is_private: detail.is_private.unwrap_or(false),
            completed: detail.completed,
            piece_size: detail.piece_size.unwrap_or(0),
            pieces_have: detail.pieces_have.unwrap_or(0),
        };
        self.props_cache
            .lock()
            .insert(hash.to_string(), (Instant::now(), props.clone()));
        Some(props)
    }
}

impl QBittorrent {
    /// WebUI/OOBE 提交的 JSON 配置（camelCase，对应 Gson 反序列化 QBittorrentConfigImpl）
    pub fn from_json(
        id: &str,
        dtype: &str,
        config: &Value,
        handle: &ConfigHandle,
    ) -> Result<Self> {
        // 转换为 YAML 风格的 kebab-case 键后复用 from_yaml 逻辑
        let mut map = serde_yaml::Mapping::new();
        let put = |map: &mut serde_yaml::Mapping, k: &str, v: Option<Value>| {
            if let Some(v) = v {
                if let Ok(yv) = serde_yaml::to_value(v) {
                    map.insert(serde_yaml::Value::String(k.to_string()), yv);
                }
            }
        };
        put(&mut map, "type", Some(Value::String(dtype.to_string())));
        for (jk, yk) in [
            ("name", "name"),
            ("endpoint", "endpoint"),
            ("username", "username"),
            ("password", "password"),
            ("apiKey", "api-key"),
            ("incrementBan", "increment-ban"),
            ("useShadowBan", "use-shadow-ban"),
            ("verifySsl", "verify-ssl"),
            ("ignorePrivate", "ignore-private"),
            ("paused", "paused"),
        ] {
            put(&mut map, yk, config.get(jk).cloned());
        }
        if let Some(ba) = config.get("basicAuth") {
            let mut bam = serde_yaml::Mapping::new();
            put(&mut bam, "user", ba.get("user").cloned());
            put(&mut bam, "pass", ba.get("pass").cloned());
            map.insert(
                serde_yaml::Value::String("basic-auth".into()),
                serde_yaml::Value::Mapping(bam),
            );
        }
        Self::from_yaml(id, dtype, &serde_yaml::Value::Mapping(map), handle)
    }
}

impl QBittorrent {
    fn save_config_impl(&self) -> Vec<(String, YValue)> {
        vec![
            ("type".into(), YValue::Str(if self.cfg.downloader_type == "qBittorrentEE" { "qbittorrentee".into() } else { "qbittorrent".into() })),
            ("name".into(), YValue::Str(self.cfg.name.clone())),
            ("endpoint".into(), YValue::Str(self.cfg.endpoint.clone())),
            ("username".into(), YValue::Str(self.cfg.username.clone())),
            ("password".into(), YValue::Str(self.cfg.password.clone())),
            ("api-key".into(), YValue::Str(self.cfg.api_key.clone())),
            ("basic-auth".into(), YValue::Map(vec![
                ("user".into(), YValue::Str(self.cfg.basic_auth_user.clone())),
                ("pass".into(), YValue::Str(self.cfg.basic_auth_pass.clone())),
            ])),
            ("increment-ban".into(), YValue::Bool(self.cfg.increment_ban)),
            ("use-shadow-ban".into(), YValue::Bool(self.cfg.use_shadow_ban)),
            ("verify-ssl".into(), YValue::Bool(self.cfg.verify_ssl)),
            ("ignore-private".into(), YValue::Bool(self.cfg.ignore_private)),
            ("paused".into(), YValue::Bool(self.is_paused())),
        ]
    }

    fn config_json_impl(&self) -> Value {
        serde_json::json!({
            "type": if self.cfg.downloader_type == "qBittorrentEE" { "qbittorrentee" } else { "qbittorrent" },
            "name": self.cfg.name,
            "endpoint": self.cfg.endpoint,
            "username": self.cfg.username,
            "password": self.cfg.password,
            "apiKey": self.cfg.api_key,
            "basicAuth": { "user": self.cfg.basic_auth_user, "pass": self.cfg.basic_auth_pass },
            "incrementBan": self.cfg.increment_ban,
            "useShadowBan": self.cfg.use_shadow_ban,
            "verifySsl": self.cfg.verify_ssl,
            "ignorePrivate": self.cfg.ignore_private,
            "paused": self.is_paused(),
        })
    }
}

/// 对应上游 normalizeVersion：数字与前导点保留，后缀转 prerelease
fn normalize_version(unprocessed: &str) -> String {
    let unprocessed = unprocessed.trim();
    let mut sb = String::new();
    let mut suffix = String::new();
    let mut reach_suffix = false;
    for c in unprocessed.chars() {
        if !reach_suffix && (c.is_ascii_digit() || c == '.') {
            sb.push(c);
        } else {
            reach_suffix = true;
            suffix.push(c);
        }
    }
    if suffix.is_empty() {
        return sb;
    }
    if suffix.starts_with('-') || suffix.starts_with('+') {
        format!("{}{}", sb, suffix)
    } else {
        format!("{}-{}", sb, suffix)
    }
}

fn parse_loose_semver(s: &str) -> Result<semver::Version> {
    // 拆分主版本部分与 prerelease
    let (core, pre) = match s.find(|c| c == '-' || c == '+') {
        Some(idx) => (&s[..idx], Some(&s[idx..])),
        None => (s, None),
    };
    let mut parts: Vec<&str> = core.split('.').collect();
    while parts.len() < 3 {
        parts.push("0");
    }
    let core3 = format!("{}.{}.{}", parts[0], parts[1], parts[2]);
    let normalized = match pre {
        Some(p) => format!("{}{}", core3, p),
        None => core3,
    };
    Ok(semver::Version::parse(&normalized)?)
}

#[async_trait]
impl Downloader for QBittorrent {
    fn id(&self) -> &str {
        &self.cfg.id
    }
    fn name(&self) -> &str {
        &self.cfg.name
    }
    fn downloader_type(&self) -> &str {
        &self.cfg.downloader_type
    }
    fn endpoint(&self) -> &str {
        &self.api_endpoint
    }
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    async fn login(&self) -> DownloaderLoginResult {
        // API-Key 认证路径（qB ≥ 5.2.0）
        if !self.cfg.api_key.is_empty() {
            if self.is_logged_in().await {
                self.update_and_apply_metadata_if_required().await;
                let v = self.last_version.read().clone();
                let ok = v
                    .as_ref()
                    .map(|v| v >= &semver::Version::new(5, 2, 0))
                    .unwrap_or(false);
                return if ok {
                    DownloaderLoginResult::success()
                } else {
                    DownloaderLoginResult {
                        status: LoginStatus::RequireTakeActions,
                        message: TranslationComponent::strs(
                            "DOWNLOADER_VERSION_INCOMPATIBLE",
                            vec![
                                v.map(|v| v.to_string()).unwrap_or_else(|| "unknown".into()),
                                ">= 5.2.0".to_string(),
                            ],
                        ),
                    }
                };
            }
            return DownloaderLoginResult {
                status: LoginStatus::IncorrectCredential,
                message: TranslationComponent::strs(
                    "DOWNLOADER_LOGIN_EXCEPTION",
                    vec!["API Key authentication failed".to_string()],
                ),
            };
        }

        // 已登录会话复用
        if self.is_logged_in().await {
            self.update_and_apply_metadata_if_required().await;
            if let Some(v) = self.last_version.read().clone() {
                if v >= semver::Version::new(4, 5, 0) {
                    return DownloaderLoginResult::success();
                }
                return DownloaderLoginResult {
                    status: LoginStatus::RequireTakeActions,
                    message: TranslationComponent::strs(
                        "DOWNLOADER_VERSION_INCOMPATIBLE",
                        vec![v.to_string(), ">= 4.5.0".to_string()],
                    ),
                };
            }
        }

        let resp = self
            .req(reqwest::Method::POST, "/auth/login")
            .form(&[
                ("username", self.cfg.username.as_str()),
                ("password", self.cfg.password.as_str()),
            ])
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                if status.is_success() && self.is_logged_in().await {
                    self.update_and_apply_metadata_if_required().await;
                    let v = self.last_version.read().clone();
                    let ok = v
                        .as_ref()
                        .map(|v| v >= &semver::Version::new(4, 5, 0))
                        .unwrap_or(true); // 版本获取失败时不阻塞（对齐 bypass 行为从宽）
                    if ok {
                        DownloaderLoginResult::success()
                    } else {
                        DownloaderLoginResult {
                            status: LoginStatus::RequireTakeActions,
                            message: TranslationComponent::strs(
                                "DOWNLOADER_VERSION_INCOMPATIBLE",
                                vec![
                                    v.map(|v| v.to_string()).unwrap_or_else(|| "unknown".into()),
                                    ">= 4.5.0".to_string(),
                                ],
                            ),
                        }
                    }
                } else {
                    DownloaderLoginResult {
                        status: LoginStatus::IncorrectCredential,
                        message: TranslationComponent::strs(
                            "DOWNLOADER_LOGIN_EXCEPTION",
                            vec![format!("{}: {}", status.as_u16(), body)],
                        ),
                    }
                }
            }
            Err(e) => DownloaderLoginResult {
                status: LoginStatus::Exception,
                message: TranslationComponent::strs(
                    "DOWNLOADER_LOGIN_IO_EXCEPTION",
                    vec![format!("{}: {}", e_type_name(&e), e)],
                ),
            },
        }
    }

    async fn get_torrents(&self) -> Result<Vec<Torrent>> {
        let mut all: Vec<QBTorrentInfo> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let page_size = 100usize;
        let mut offset = 0usize;
        loop {
            let url = format!(
                "/torrents/info?filter=active&limit={}&offset={}",
                page_size, offset
            );
            let r = self.req(reqwest::Method::GET, &url).send().await?;
            let status = r.status();
            if !status.is_success() {
                let body = r.text().await.unwrap_or_default();
                bail!(
                    "Failed to request torrent list: statusCode={} body={}",
                    status.as_u16(),
                    body
                );
            }
            let page: Vec<QBTorrentInfo> = r.json().await?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            let mut has_new = false;
            for t in page {
                if seen.insert(t.hash.clone()) {
                    all.push(t);
                    has_new = true;
                }
            }
            // 对齐上游：全部重复 或 小于分页大小 → 结束
            if !has_new || page_len < page_size {
                break;
            }
            offset += page_size;
        }

        // 属性回填（is_private / piece 字段）。MVP 顺序请求（带缓存），
        // 后续里程碑引入受限并发（对应上游 parallelService）。
        for t in all.iter_mut() {
            if let Some(props) = self.get_torrent_properties(&t.hash, t.completed).await {
                if t.is_private.is_none() {
                    t.is_private = Some(props.is_private);
                }
                if t.piece_size.unwrap_or(0) <= 0 || t.pieces_have.unwrap_or(0) <= 0 {
                    t.piece_size = Some(props.piece_size);
                    t.pieces_have = Some(props.pieces_have);
                }
            } else {
                tracing::warn!("Failed to retrieve properties for torrent: {}", t.hash);
            }
        }

        let include_private = !self.cfg.ignore_private;
        Ok(all
            .into_iter()
            .map(|t| Torrent {
                id: t.hash.clone(),
                name: t.name,
                hash: t.hash,
                size: t.size,
                completed_size: t.completed,
                progress: t.progress,
                rt_upload_speed: t.upspeed,
                rt_download_speed: t.dlspeed,
                private_torrent: t.is_private.unwrap_or(false),
                piece_size: t.piece_size.unwrap_or(0),
                pieces_have: t.pieces_have.unwrap_or(0),
                attached_peers: Vec::new(),
            })
            .filter(|t| include_private || !t.private_torrent)
            .collect())
    }

    async fn get_peers(&self, torrent: &Torrent) -> Result<Vec<Peer>> {
        let url = format!("/sync/torrentPeers?hash={}", torrent.id);
        let r = self.req(reqwest::Method::GET, &url).send().await?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            bail!(
                "Failed to request peers list of torrent {}: statusCode={} body={}",
                torrent.hash,
                status.as_u16(),
                body
            );
        }
        let body: Value = r.json().await?;
        let Some(peers) = body.get("peers").and_then(|p| p.as_object()) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(peers.len());
        for (key, v) in peers {
            let connection = v.get("connection").and_then(|c| c.as_str()).unwrap_or("");
            if connection.eq_ignore_ascii_case("HTTP")
                || connection.eq_ignore_ascii_case("HTTPS")
                || connection.eq_ignore_ascii_case("Web")
            {
                continue;
            }
            let ip = v.get("ip").and_then(|c| c.as_str()).unwrap_or("");
            if ip.is_empty() {
                continue;
            }
            if key.contains(".onion") || key.contains(".i2p") {
                continue;
            }
            let port = v.get("port").and_then(|p| p.as_u64()).unwrap_or(0) as u16;
            let dl_speed = v.get("dl_speed").and_then(|p| p.as_i64()).unwrap_or(0);
            let up_speed = v.get("up_speed").and_then(|p| p.as_i64()).unwrap_or(0);
            let peer_id = v
                .get("peer_id_client")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            let client_name = v
                .get("client")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            let flags = v
                .get("flags")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            out.push(Peer {
                address: PeerAddress::new(ip, port, key.clone()),
                peer_id,
                client_name,
                download_speed: dl_speed,
                downloaded: v.get("downloaded").and_then(|p| p.as_i64()).unwrap_or(0),
                upload_speed: up_speed,
                uploaded: v.get("uploaded").and_then(|p| p.as_i64()).unwrap_or(0),
                progress: v.get("progress").and_then(|p| p.as_f64()).unwrap_or(0.0),
                flags,
                handshaking: up_speed <= 0 && dl_speed <= 0,
            });
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
        if removed_empty && !added.is_empty() && self.cfg.increment_ban && !apply_full {
            let peers: Vec<String> = added
                .iter()
                .map(|m| {
                    m.peer
                        .raw_ip
                        .clone()
                        .unwrap_or_else(|| format!("{}:{}", m.peer.address.ip, m.peer.address.port))
                })
                .collect();
            let joined = peers.join("|");
            let r = self
                .req(reqwest::Method::POST, "/transfer/banPeers")
                .form(&[("peers", joined.as_str())])
                .send()
                .await?;
            let status = r.status();
            if !status.is_success() {
                let body = r.text().await.unwrap_or_default();
                bail!("Save qBittorrent increment banlist error: statusCode={} body={}", status, body);
            }
            return Ok(());
        }

        let support_range = self.support_range_ban();
        let remap_cfg = self.handle.get().remap_config();
        let mut seen: HashSet<String> = HashSet::new();
        let mut lines: Vec<String> = Vec::new();
        for addr in full {
            for remapped in remap_ban_address(*addr, support_range, &remap_cfg) {
                let s = match remapped {
                    IpNet::V4(n) => {
                        if n.prefix_len() == 32 {
                            n.addr().to_string()
                        } else {
                            n.to_string()
                        }
                    }
                    IpNet::V6(n) => {
                        if n.prefix_len() == 128 {
                            n.addr().to_string()
                        } else {
                            n.to_string()
                        }
                    }
                };
                if seen.insert(s.clone()) {
                    lines.push(s);
                }
            }
        }
        let ban_str = lines.join("\n");
        let json = serde_json::json!({ "banned_IPs": ban_str }).to_string();
        let r = self
            .req(reqwest::Method::POST, "/app/setPreferences")
            .form(&[("json", json.as_str())])
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            bail!("Save qBittorrent banlist error: statusCode={} body={}", status, body);
        }
        Ok(())
    }

    async fn statistics(&self) -> Result<DownloaderStatistics> {
        let r = self
            .req(reqwest::Method::GET, "/sync/maindata")
            .send()
            .await?;
        if !r.status().is_success() {
            bail!("Failed to request statistics: statusCode={}", r.status());
        }
        let body: Value = r.json().await?;
        let ul = body
            .pointer("/server_state/alltime_ul")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let dl = body
            .pointer("/server_state/alltime_dl")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if ul == 0 && dl == 0 {
            bail!("not ready (alltimeUl=0, alltimeDl=0)");
        }
        Ok(DownloaderStatistics {
            total_uploaded: ul,
            total_downloaded: dl,
        })
    }

    fn support_range_ban(&self) -> bool {
        // RANGE_BAN_IP：qB ≥ 5.3.0（对应上游特性旗标判定；忽略 alpha/beta 特例）
        self.last_version
            .read()
            .as_ref()
            .map(|v| v >= &semver::Version::new(5, 3, 0))
            .unwrap_or(false)
    }

    fn has_feature(&self, flag: &str) -> bool {
        match flag {
            "UNBAN_IP" | "TRAFFIC_STATS" | "LIVE_UPDATE_BT_PROTOCOL_PORT" => true,
            "RANGE_BAN_IP" => self.support_range_ban(),
            _ => false,
        }
    }

    fn save_config(&self) -> Vec<(String, YValue)> {
        self.save_config_impl()
    }

    fn config_json(&self) -> Value {
        self.config_json_impl()
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

#[allow(dead_code)]
fn status_code_u16(s: StatusCode) -> u16 {
    s.as_u16()
}
