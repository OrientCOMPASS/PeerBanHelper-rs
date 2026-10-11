//! pbh-push：推送通知（对应上游 util/push 的 9 个 Provider）。
//!
//! 各 Provider 的配置键与上游 loadFromYaml 一致（config.yml push-notification 节）。
//! 推送由 AlertManager 语义触发（publishAlert(push=true) 时群发），与上游一致。
//! 注意：对外部推送服务的 payload 采用各服务的标准格式，与上游字段级完全一致为目标，
//! 个别装饰性字段（如图标 URL）保持上游取值。
use anyhow::{bail, Result};
use parking_lot::RwLock;
use pbh_config::{AppConfig, ConfigHandle};
use serde_json::{json, Value};
use std::sync::Arc;

const ICON_URL: &str = "https://raw.githubusercontent.com/PBH-BTN/PeerBanHelper/refs/heads/master/src/main/resources/assets/icon.png";

#[derive(Clone, Debug)]
pub struct PushProvider {
    pub name: String,
    pub ptype: String,
    pub config: Value,
}

impl PushProvider {
    pub async fn send(&self, title: &str, content: &str) -> Result<()> {
        {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?;
            let cfg = &self.config;
            let s = |k: &str, d: &str| {
                cfg.get(k)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(d)
                    .to_string()
            };
            match self.ptype.as_str() {
                "telegram" => {
                    let token = s("token", "");
                    let chatid = s("chatid", "");
                    if token.is_empty() || chatid.is_empty() {
                        bail!("telegram: token/chatid required");
                    }
                    let url = format!("https://api.telegram.org/bot{}/sendPhoto", token);
                    let resp = client
                        .post(&url)
                        .json(&json!({
                            "chat_id": chatid,
                            "photo": ICON_URL,
                            "caption": format!("<b>{}</b>\n{}", title, content),
                            "parse_mode": "HTML",
                        }))
                        .send()
                        .await?;
                    check_status(resp).await
                }
                "webhook" => {
                    let url = s("url", "");
                    if url.is_empty() {
                        bail!("webhook: url required");
                    }
                    let method = s("method", "POST").to_uppercase();
                    let content_type = s("content-type", "application/json");
                    let m = match method.as_str() {
                        "GET" => reqwest::Method::GET,
                        "PUT" => reqwest::Method::PUT,
                        "PATCH" => reqwest::Method::PATCH,
                        _ => reqwest::Method::POST,
                    };
                    let resp = client
                        .request(m, &url)
                        .header(reqwest::header::CONTENT_TYPE, content_type)
                        .body(json!({ "title": title, "content": content }).to_string())
                        .send()
                        .await?;
                    check_status(resp).await
                }
                "bark" => {
                    let backend = s("backend_url", "https://api.day.app/push");
                    let key = s("device_key", "");
                    if key.is_empty() {
                        bail!("bark: device_key required");
                    }
                    let mut body = json!({
                        "device_key": key,
                        "title": title,
                        "body": content,
                        "icon": ICON_URL,
                    });
                    let group = s("message_group", "");
                    if !group.is_empty() {
                        body["group"] = json!(group);
                    }
                    let resp = client.post(&backend).json(&body).send().await?;
                    check_status(resp).await
                }
                "gotify" => {
                    let endpoint = s("endpoint", "");
                    if endpoint.is_empty() {
                        bail!("gotify: endpoint required");
                    }
                    let priority = cfg.get("priority").and_then(|v| v.as_i64()).unwrap_or(5);
                    let resp = client
                        .post(&endpoint)
                        .form(&[
                            ("title", title.to_string()),
                            ("message", content.to_string()),
                            ("priority", priority.to_string()),
                        ])
                        .send()
                        .await?;
                    check_status(resp).await
                }
                "ntfy" => {
                    let server = s("server_url", "https://ntfy.sh").trim_end_matches('/').to_string();
                    let topic = s("topic", "");
                    if topic.is_empty() {
                        bail!("ntfy: topic required");
                    }
                    let priority = cfg.get("priority").and_then(|v| v.as_i64()).unwrap_or(3);
                    let tags = s("tags", "");
                    let token = s("token", "");
                    let mut rb = client
                        .post(format!("{}/{}", server, topic))
                        .header("Title", title)
                        .header("Priority", priority.to_string())
                        .header("Icon", ICON_URL)
                        .body(content.to_string());
                    if !tags.is_empty() {
                        rb = rb.header("Tags", tags);
                    }
                    if !token.is_empty() {
                        rb = rb.bearer_auth(token);
                    }
                    let resp = rb.send().await?;
                    check_status(resp).await
                }
                "pushdeer" => {
                    let endpoint = s("endpoint", "https://api2.pushdeer.com/message/push");
                    let pushkey = s("pushkey", "");
                    if pushkey.is_empty() {
                        bail!("pushdeer: pushkey required");
                    }
                    let resp = client
                        .post(&endpoint)
                        .json(&json!({
                            "pushkey": pushkey,
                            "text": title,
                            "desp": content,
                            "type": "markdown",
                        }))
                        .send()
                        .await?;
                    check_status(resp).await
                }
                "pushplus" => {
                    let token = s("token", "");
                    if token.is_empty() {
                        bail!("pushplus: token required");
                    }
                    let mut body = json!({
                        "token": token,
                        "title": title,
                        "content": content,
                        "template": "html",
                    });
                    let topic = s("topic", "");
                    if !topic.is_empty() {
                        body["topic"] = json!(topic);
                    }
                    let channel = s("channel", "");
                    if !channel.is_empty() {
                        body["channel"] = json!(channel);
                    }
                    let resp = client
                        .post("https://www.pushplus.plus/send")
                        .json(&body)
                        .send()
                        .await?;
                    check_status(resp).await
                }
                "serverchan" => {
                    let sendkey = s("sendkey", "");
                    if sendkey.is_empty() {
                        bail!("serverchan: sendkey required");
                    }
                    let mut body = json!({ "title": title, "desp": content });
                    let channel = s("channel", "");
                    if !channel.is_empty() {
                        body["channel"] = json!(channel);
                    }
                    let openid = s("openid", "");
                    if !openid.is_empty() {
                        body["openid"] = json!(openid);
                    }
                    let resp = client
                        .post(format!("https://sctapi.ftqq.com/{}.send", sendkey))
                        .json(&body)
                        .send()
                        .await?;
                    check_status(resp).await
                }
                "smtp" => self.send_smtp(title, content).await,
                other => bail!("unknown push provider type: {}", other),
            }
        }
    }

    async fn send_smtp(&self, title: &str, content: &str) -> Result<()> {
        use lettre::message::header::ContentType;
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
        let cfg = &self.config;
        let s = |k: &str, d: &str| {
            cfg.get(k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(d)
                .to_string()
        };
        let host = s("host", "");
        if host.is_empty() {
            bail!("smtp: host required");
        }
        let port: u16 = cfg
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(465) as u16;
        let username = s("username", "");
        let password = s("password", "");
        let from = s("from", &username);
        let to = s("to", "");
        if to.is_empty() {
            bail!("smtp: to required");
        }
        let email = Message::builder()
            .from(from.parse()?)
            .to(to.parse()?)
            .subject(title)
            .header(ContentType::TEXT_PLAIN)
            .body(content.to_string())?;
        let transport = if port == 465 || s("ssl", "true") == "true" {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?
                .port(port)
                .credentials(Credentials::new(username, password))
                .build()
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
                .port(port)
                .credentials(Credentials::new(username, password))
                .build()
        };
        transport.send(email).await?;
        Ok(())
    }
}

async fn check_status(resp: reqwest::Response) -> Result<()> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("push failed: HTTP {} {}", status, &body.chars().take(200).collect::<String>());
    }
    Ok(())
}

/// 推送管理器：从 config.yml push-notification 节加载全部 Provider
#[derive(Default)]
pub struct PushManager {
    providers: RwLock<Vec<PushProvider>>,
}

impl PushManager {
    pub fn from_config(cfg: &AppConfig) -> Arc<Self> {
        let me = Arc::new(Self::default());
        me.reload(cfg);
        me
    }

    pub fn reload(&self, cfg: &AppConfig) {
        let mut providers = Vec::new();
        if let Some(value_section) = cfg.main.get("push-notification") {
            if let serde_yaml::Value::Mapping(map) = value_section {
                for (k, v) in map {
                    let name = match k {
                        serde_yaml::Value::String(s) => s.clone(),
                        _ => continue,
                    };
                    let json = pbh_config::yaml_file::yaml_to_json(v);
                    let ptype = json
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    providers.push(PushProvider {
                        name,
                        ptype,
                        config: json,
                    });
                }
            }
        }
        *self.providers.write() = providers;
    }

    pub fn list(&self) -> Vec<PushProvider> {
        self.providers.read().clone()
    }

    pub fn find(&self, name: &str) -> Option<PushProvider> {
        self.providers.read().iter().find(|p| p.name == name).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.read().is_empty()
    }

    /// 群发（后台任务，逐个发送并记录结果）
    pub fn send_all(&self, title: String, content: String) {
        let providers = self.list();
        if providers.is_empty() {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for p in providers {
                    match p.send(&title, &content).await {
                        Ok(_) => tracing::debug!("[Push:{}] sent", p.name),
                        Err(e) => tracing::warn!("[Push:{}] failed: {}", p.name, e),
                    }
                }
            });
        }
    }
}

/// 将 PushProvider 列表写回 config.yml push-notification 节（保留注释）
pub fn save_provider_to_config(
    handle: &ConfigHandle,
    name: &str,
    ptype: &str,
    config: &Value,
) -> Result<()> {
    use pbh_config::yaml_file::{YamlFile, YValue};
    let mut file = YamlFile::load(handle.main_file())?;
    let mut obj = config.as_object().cloned().unwrap_or_default();
    obj.insert("type".to_string(), json!(ptype));
    let path = format!("push-notification.{}", name);
    file.set(&path, &YValue::from_json(&Value::Object(obj)))?;
    file.save()?;
    Ok(())
}

pub fn remove_provider_from_config(handle: &ConfigHandle, name: &str) -> Result<()> {
    use pbh_config::yaml_file::YamlFile;
    let mut file = YamlFile::load(handle.main_file())?;
    file.remove(&format!("push-notification.{}", name));
    file.save()?;
    Ok(())
}
