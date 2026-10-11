//! Aria2Next 适配器（对应上游 downloader/impl/aria2next）。
//!
//! 目标为 Aria2Next 分支（提供 aria2.getPeers / aria2.setBtPeerBlocklist 扩展，
//! 标准 aria2 无此二方法）。JSON-RPC 2.0：
//! - 登录：aria2.getVersion
//! - 种子：aria2.tellActive [keys...]（仅保留 bittorrent != null）
//! - Peers：aria2.getPeers(gid)
//! - 封禁：aria2.setBtPeerBlocklist([remapped...])（恒为全量替换）
//! - 统计：aria2.getGlobalStat → {uploadLength, downloadLength}
//! 数值在 aria2 RPC 中为字符串，解析时转换。
use anyhow::{bail, Result};
use async_trait::async_trait;
use ipnet::IpNet;
use std::sync::atomic::AtomicBool;
use pbh_config::{ConfigHandle, YValue};
use pbh_core::model::{BanMetadata, Peer, PeerAddress, Torrent};
use pbh_core::translate::TranslationComponent;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::{remap_ban_address, Downloader, DownloaderLoginResult, DownloaderStatistics, LoginStatus};

#[derive(Clone, Debug)]
pub struct Aria2Config {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub token: String,
    pub verify_ssl: bool,
    pub ignore_private: bool,
    pub paused: bool,
}

pub struct Aria2Next {
    cfg: Aria2Config,
    http: Client,
    handle: ConfigHandle,
    rpc_id: AtomicU64,
    paused: AtomicBool,
}

const TASK_KEYS: [&str; 24] = [
    "gid", "status", "totalLength", "completedLength", "uploadLength", "bitfield",
    "downloadSpeed", "uploadSpeed", "infoHash", "numSeeders", "seeder", "pieceLength",
    "numPieces", "connections", "errorCode", "errorMessage", "followedBy", "following",
    "belongsTo", "dir", "bittorrent", "verifiedLength", "verifyIntegrityPending", "files",
];

impl Aria2Next {
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
        let cfg = Aria2Config {
            id: id.to_string(),
            name: get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or(id).to_string(),
            endpoint,
            token: get("token").and_then(|v| v.as_str()).unwrap_or("").to_string(),
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
        let cfg = Aria2Config {
            id: id.to_string(),
            name: g("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or(id).to_string(),
            endpoint,
            token: g("token").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            verify_ssl: g("verifySsl").and_then(|v| v.as_bool()).unwrap_or(true),
            ignore_private: g("ignorePrivate").and_then(|v| v.as_bool()).unwrap_or(false),
            paused: g("paused").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        Self::build(cfg, handle)
    }

    fn build(cfg: Aria2Config, handle: &ConfigHandle) -> Result<Self> {
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
            rpc_id: AtomicU64::new(1),
            paused: AtomicBool::new(paused),
        })
    }

    async fn rpc(&self, method: &str, mut params: Vec<Value>) -> Result<Value> {
        if !self.cfg.token.is_empty() {
            params.insert(0, Value::String(format!("token:{}", self.cfg.token)));
        }
        let id = self.rpc_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": format!("pbh-{}", id),
            "method": method,
            "params": params,
        });
        let resp = self.http.post(&self.cfg.endpoint).json(&body).send().await?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| anyhow::anyhow!("aria2 rpc decode: {} (http {})", e, status))?;
        if let Some(err) = v.get("error") {
            bail!(
                "aria2 rpc error: {} {}",
                err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                err.get("message").and_then(|m| m.as_str()).unwrap_or("")
            );
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn parse_i64(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn parse_f64(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}

fn hex_to_latin1(hex: &str) -> String {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect();
    bytes.iter().map(|b| *b as char).collect()
}

#[async_trait]
impl Downloader for Aria2Next {
    fn id(&self) -> &str {
        &self.cfg.id
    }
    fn name(&self) -> &str {
        &self.cfg.name
    }
    fn downloader_type(&self) -> &str {
        "Aria2Next"
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
        match self.rpc("aria2.getVersion", vec![]).await {
            Ok(v) => {
                let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("0");
                tracing::debug!("[{}] aria2 version: {}", self.cfg.name, version);
                DownloaderLoginResult::success()
            }
            Err(e) => {
                let msg = e.to_string();
                let is_auth = msg.contains("unauthorized") || msg.contains("secret");
                DownloaderLoginResult {
                    status: if is_auth {
                        LoginStatus::IncorrectCredential
                    } else {
                        LoginStatus::Exception
                    },
                    message: TranslationComponent::strs("DOWNLOADER_LOGIN_IO_EXCEPTION", vec![msg]),
                }
            }
        }
    }

    async fn get_torrents(&self) -> Result<Vec<Torrent>> {
        let keys: Vec<Value> = TASK_KEYS.iter().map(|k| json!(k)).collect();
        let result = self.rpc("aria2.tellActive", vec![Value::Array(keys)]).await?;
        let Some(arr) = result.as_array() else {
            return Ok(Vec::new());
        };
        let include_private = !self.cfg.ignore_private;
        let mut out = Vec::new();
        for t in arr {
            let Some(bt) = t.get("bittorrent") else {
                continue; // 非 BT 任务
            };
            if !bt.is_null() {
                // bittorrent.info.private 存在且为 true 时视为私有种子
                let is_private = bt
                    .pointer("/info/private")
                    .and_then(|p| p.as_bool())
                    .unwrap_or(false);
                if !include_private && is_private {
                    continue;
                }
            }
            let total = parse_i64(t.get("totalLength").unwrap_or(&Value::Null));
            let completed = parse_i64(t.get("completedLength").unwrap_or(&Value::Null));
            let gid = t.get("gid").and_then(|g| g.as_str()).unwrap_or("").to_string();
            let name = bt
                .pointer("/info/name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let hash = t
                .get("infoHash")
                .and_then(|h| h.as_str())
                .unwrap_or("")
                .to_string();
            out.push(Torrent {
                id: gid.clone(),
                name,
                hash: if hash.is_empty() { gid.clone() } else { hash.clone() },
                size: total,
                completed_size: completed,
                progress: if total > 0 { completed as f64 / total as f64 } else { 0.0 },
                rt_upload_speed: parse_i64(t.get("uploadSpeed").unwrap_or(&Value::Null)),
                rt_download_speed: parse_i64(t.get("downloadSpeed").unwrap_or(&Value::Null)),
                private_torrent: bt
                    .pointer("/info/private")
                    .and_then(|p| p.as_bool())
                    .unwrap_or(false),
                piece_size: parse_i64(t.get("pieceLength").unwrap_or(&Value::Null)),
                pieces_have: 0,
                attached_peers: Vec::new(),
            });
            let _ = hash;
        }
        Ok(out)
    }

    async fn get_peers(&self, torrent: &Torrent) -> Result<Vec<Peer>> {
        let result = self.rpc("aria2.getPeers", vec![json!(torrent.id)]).await?;
        let Some(arr) = result.as_array() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(arr.len());
        for p in arr {
            let ip = p.get("ip").and_then(|v| v.as_str()).unwrap_or("");
            if ip.is_empty() {
                continue;
            }
            let port = parse_i64(p.get("port").unwrap_or(&Value::Null)) as u16;
            let dl = parse_i64(p.get("downloadSpeed").unwrap_or(&Value::Null));
            let ul = parse_i64(p.get("uploadSpeed").unwrap_or(&Value::Null));
            let peer_id_hex = p.get("peerId").and_then(|v| v.as_str()).unwrap_or("");
            let peer_id = if peer_id_hex.is_empty() {
                None
            } else {
                Some(hex_to_latin1(peer_id_hex))
            };
            let client_name = p
                .get("clientName")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| peer_id.clone());
            out.push(Peer {
                address: PeerAddress::new(ip, port, ip.clone()),
                peer_id,
                client_name,
                download_speed: dl,
                downloaded: parse_i64(p.get("downloaded").unwrap_or(&Value::Null)),
                upload_speed: ul,
                uploaded: parse_i64(p.get("uploaded").unwrap_or(&Value::Null)),
                progress: parse_f64(p.get("progress").unwrap_or(&Value::Null)),
                flags: None,
                handshaking: dl <= 0 && ul <= 0,
            });
        }
        Ok(out)
    }

    async fn set_ban_list(
        &self,
        full: &[IpNet],
        _added: &[BanMetadata],
        _removed_empty: bool,
        _apply_full: bool,
    ) -> Result<()> {
        let remap_cfg = self.handle.get().remap_config();
        let mut seen = HashSet::new();
        let list: Vec<String> = full
            .iter()
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
            .collect();
        self.rpc("aria2.setBtPeerBlocklist", vec![json!(list)]).await?;
        Ok(())
    }

    async fn statistics(&self) -> Result<DownloaderStatistics> {
        let v = self.rpc("aria2.getGlobalStat", vec![]).await?;
        Ok(DownloaderStatistics {
            total_uploaded: parse_i64(v.get("uploadLength").unwrap_or(&Value::Null)),
            total_downloaded: parse_i64(v.get("downloadLength").unwrap_or(&Value::Null)),
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
            ("type".into(), YValue::Str("aria2next".into())),
            ("name".into(), YValue::Str(self.cfg.name.clone())),
            ("endpoint".into(), YValue::Str(self.cfg.endpoint.clone())),
            ("token".into(), YValue::Str(self.cfg.token.clone())),
            ("verify-ssl".into(), YValue::Bool(self.cfg.verify_ssl)),
            ("ignore-private".into(), YValue::Bool(self.cfg.ignore_private)),
            ("paused".into(), YValue::Bool(self.is_paused())),
        ]
    }

    fn config_json(&self) -> Value {
        json!({
            "type": "aria2next",
            "name": self.cfg.name,
            "endpoint": self.cfg.endpoint,
            "token": self.cfg.token,
            "verifySsl": self.cfg.verify_ssl,
            "ignorePrivate": self.cfg.ignore_private,
            "paused": self.is_paused(),
        })
    }
}
