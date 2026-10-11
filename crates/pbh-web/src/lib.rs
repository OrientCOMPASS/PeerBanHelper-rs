//! pbh-web：HTTP API 层（对应上游 JavalinWebContainer + webapi 控制器）。
//!
//! 行为对齐要点：
//! - Gson 兼容序列化：StdResp{success,message?,data?}（null 省略）、epoch 毫秒、
//!   TranslationComponent {key,params}、PBHPage{page,size,total,results}
//! - 认证：`Authorization: Bearer`/`?token=`/Cookie PBH-Session；token 未设置 → 303 /init；
//!   未登录 → 403；登录失败 → 401
//! - 配置读写：GET/PUT /api/general/{config|profile}（JSON 视图，键 '-'↔'_'，规则字符串
//!   列表 ↔ 对象列表，push-notification 字典 ↔ 列表；PUT 为保留注释的合并写回 + 热重载）
//! - OOBE 与下载器 CRUD 对齐上游 handleOOBERequest/handleDownloaderPut/Patch/Delete/Test
use axum::{
    body::{Body, Bytes},
    extract::{Path as AxPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ipnet::IpNet;
use parking_lot::RwLock;
use pbh_config::ConfigHandle;
use pbh_core::lang::LangStore;
use pbh_core::model::BanMetadata;
use pbh_downloader::{create_downloader_from_json, DownloaderManager};
use pbh_engine::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

pub mod extra;
pub mod loghub;
pub use loghub::LogHub;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub cfg: ConfigHandle,
    pub lang: Arc<LangStore>,
    pub token: Arc<RwLock<String>>,
    pub version: String,
    pub static_dir: PathBuf,
    pub log_hub: Arc<LogHub>,
}

// ---------------------------------------------------------------------------
// Gson 兼容响应包装
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct StdResp {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl StdResp {
    pub fn ok(data: Value) -> Response {
        json_resp(
            StatusCode::OK,
            StdResp {
                success: true,
                message: None,
                data: Some(data),
            },
        )
    }
    pub fn ok_msg(locale_msg: String, data: Option<Value>) -> Response {
        json_resp(
            StatusCode::OK,
            StdResp {
                success: true,
                message: Some(locale_msg),
                data,
            },
        )
    }
    pub fn created(locale_msg: String, data: Option<Value>) -> Response {
        json_resp(
            StatusCode::CREATED,
            StdResp {
                success: true,
                message: Some(locale_msg),
                data,
            },
        )
    }
    pub fn err(status: StatusCode, locale_msg: String, data: Option<Value>) -> Response {
        json_resp(
            status,
            StdResp {
                success: false,
                message: Some(locale_msg),
                data,
            },
        )
    }
}

fn json_resp(status: StatusCode, resp: StdResp) -> Response {
    let body = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(body),
    )
        .into_response()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PBHPage<T: Serialize> {
    pub page: i64,
    pub size: i64,
    pub total: i64,
    pub results: Vec<T>,
}

// ---------------------------------------------------------------------------
// 请求上下文 / 认证
// ---------------------------------------------------------------------------

pub struct ReqCtx {
    pub locale: String,
}

/// extra.rs 使用的公开别名
pub type ReqCtxPub = ReqCtxHelper;

pub struct ReqCtxHelper;

impl ReqCtxHelper {
    pub fn from_headers(headers: &HeaderMap) -> ReqCtx {
        req_ctx(headers)
    }
}

fn req_ctx(headers: &HeaderMap) -> ReqCtx {
    let accept = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok());
    ReqCtx {
        locale: LangStore::normalize_locale(accept),
    }
}

/// BanLogDTO 组装（bans/logs 与 peer banHistory 共用）
pub fn ban_log_dto(ctx: &ReqCtx, state: &AppState, r: &pbh_db::BanLogRow) -> Value {
    let bake = |tc_json: &str| -> String {
        match serde_json::from_str::<pbh_core::translate::TranslationComponent>(tc_json) {
            Ok(tc) => state.lang.tlc(&ctx.locale, &tc),
            Err(_) => tc_json.to_string(),
        }
    };
    let downloader = state
        .engine
        .downloaders
        .find(&r.downloader)
        .map(|d| serde_json::to_value(d.basic_info()).unwrap());
    let mut m = Map::new();
    m.insert("banAt".into(), json!(r.ban_at));
    m.insert("unbanAt".into(), json!(r.unban_at));
    m.insert("peerIp".into(), json!(r.ip));
    m.insert("peerPort".into(), json!(r.port));
    if let Some(v) = &r.peer_id {
        m.insert("peerId".into(), json!(v));
    }
    if let Some(v) = &r.peer_client_name {
        m.insert("peerClientName".into(), json!(v));
    }
    m.insert("peerUploaded".into(), json!(r.peer_uploaded));
    m.insert("peerDownloaded".into(), json!(r.peer_downloaded));
    m.insert("peerProgress".into(), json!(r.peer_progress));
    m.insert("torrentInfoHash".into(), r.torrent_info_hash.clone().map(|v| json!(v)).unwrap_or(Value::Null));
    m.insert("torrentName".into(), r.torrent_name.clone().map(|v| json!(v)).unwrap_or(Value::Null));
    m.insert("torrentSize".into(), json!(r.torrent_size));
    m.insert("module".into(), json!(r.module_name));
    m.insert("rule".into(), json!(bake(&r.rule_name)));
    m.insert("description".into(), json!(bake(&r.description)));
    if let Some(d) = downloader {
        m.insert("downloader".into(), d);
    }
    Value::Object(m)
}

fn provided_token(headers: &HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(rest) = v.strip_prefix("Bearer ") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    if let Some(t) = query_token {
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn query_param(uri: &axum::http::Uri, key: &str) -> Option<String> {
    uri.query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{}=", key)))
            .map(|v| urlencoding_decode(v))
    })
}

fn urlencoding_decode(s: &str) -> String {
    // 最小实现：%XX 与 '+'
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn need_init_response(state: &AppState, ctx: &ReqCtx) -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/init".to_string()),
            (header::CONTENT_TYPE, "application/json".to_string()),
        ],
        Body::from(
            serde_json::to_vec(&StdResp {
                success: false,
                message: Some(state.lang.tl(&ctx.locale, "WEBAPI_NEED_INIT", &[])),
                data: Some(json!({ "location": "/init" })),
            })
            .unwrap(),
        ),
    )
        .into_response()
}

const ANYONE_PATHS: &[&str] = &[
    "/api/auth/login",
    "/api/auth/logout",
    "/api/oobe/init",
    "/api/oobe/testDownloader",
    "/api/oobe/scanDownloader",
    "/api/oobe/testDatabaseConfig",
    "/api/metadata/manifest",
];

async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api") || ANYONE_PATHS.contains(&path.as_str()) {
        return next.run(req).await;
    }
    let headers = req.headers().clone();
    let ctx = req_ctx(&headers);
    let token = state.token.read().clone();
    if token.is_empty() {
        return need_init_response(&state, &ctx);
    }
    let query_token = query_param(req.uri(), "token");
    match provided_token(&headers, query_token.as_deref()) {
        Some(t) if t == token => next.run(req).await,
        _ => StdResp::err(
            StatusCode::FORBIDDEN,
            state.lang.tl(&ctx.locale, "WEBAPI_NOT_LOGGED", &[]),
            None,
        ),
    }
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginBody {
    #[serde(default)]
    token: String,
}

async fn handle_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    let ctx = req_ctx(&headers);
    let token = state.token.read().clone();
    if token.is_empty() {
        return need_init_response(&state, &ctx);
    }
    if body.token.is_empty() || body.token != token {
        return StdResp::err(
            StatusCode::UNAUTHORIZED,
            state.lang.tl(&ctx.locale, "WEBAPI_AUTH_INVALID_TOKEN", &[]),
            None,
        );
    }
    let mut resp = StdResp::ok_msg(state.lang.tl(&ctx.locale, "WEBAPI_AUTH_OK", &[]), None);
    resp.headers_mut().insert(
        header::SET_COOKIE,
        format!("PBH-Session={}; Path=/; HttpOnly; SameSite=Lax", token)
            .parse()
            .unwrap(),
    );
    resp
}

async fn handle_logout() -> Response {
    let mut resp = StdResp::ok_msg("success".to_string(), None);
    resp.headers_mut().insert(
        header::SET_COOKIE,
        "PBH-Session=; Path=/; Max-Age=0".parse().unwrap(),
    );
    resp
}

// ---------------------------------------------------------------------------
// bans
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BansQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
    #[serde(default = "default_true")]
    ignore_ban_for_disconnect: bool,
    #[serde(default)]
    search: Option<String>,
}

fn default_page() -> i64 {
    1
}
fn default_page_size() -> i64 {
    10
}
fn default_true() -> bool {
    true
}

fn bake(locale: &str, lang: &LangStore, meta: &BanMetadata) -> Value {
    let mut m = Map::new();
    m.insert("downloader".into(), serde_json::to_value(&meta.downloader).unwrap());
    m.insert("torrent".into(), serde_json::to_value(&meta.torrent).unwrap());
    m.insert("peer".into(), serde_json::to_value(&meta.peer).unwrap());
    m.insert("reverseLookup".into(), Value::String(meta.reverse_lookup.clone()));
    m.insert("context".into(), Value::String(meta.context.clone()));
    m.insert("banAt".into(), Value::Number(meta.ban_at.millis().into()));
    m.insert("unbanAt".into(), Value::Number(meta.unban_at.millis().into()));
    m.insert("rule".into(), Value::String(lang.tlc(locale, &meta.rule)));
    m.insert(
        "description".into(),
        Value::String(lang.tlc(locale, &meta.description)),
    );
    Value::Object(m)
}

async fn handle_bans(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<BansQuery>,
) -> Response {
    let ctx = req_ctx(&headers);
    let search = q.search.as_deref().map(|s| s.to_lowercase());
    let mut with_time: Vec<(i64, Value)> = Vec::new();

    for (net, meta) in state.engine.snapshot_bans() {
        if meta.exclude_from_display {
            continue;
        }
        if q.ignore_ban_for_disconnect && meta.ban_for_disconnect {
            continue;
        }
        let address = net.addr().to_string();
        if let Some(s) = &search {
            let address_hit = address.to_lowercase().contains(s);
            let meta_str = format!(
                "{} {} {}",
                meta.peer.id.clone().unwrap_or_default(),
                meta.peer.client_name.clone().unwrap_or_default(),
                meta.context
            )
            .to_lowercase();
            if !address_hit && !meta_str.contains(s) {
                continue;
            }
        }
        let mut dto = Map::new();
        dto.insert("address".into(), Value::String(address));
        dto.insert("banMetadata".into(), bake(&ctx.locale, &state.lang, &meta));
        if let Some(geoip) = state.engine.geoip.as_ref() {
            if let Some(ip) = meta.peer.address.ip_addr() {
                if let Some(g) = geoip.query(ip) {
                    if let Ok(v) = serde_json::to_value(&g) {
                        dto.insert("ipGeoData".into(), v);
                    }
                }
            }
        }
        with_time.push((meta.ban_at.millis(), Value::Object(dto)));
    }
    with_time.sort_by(|a, b| b.0.cmp(&a.0));
    let total = with_time.len() as i64;
    let size = if q.page_size > 0 { q.page_size } else { 10 };
    let page = if q.page > 0 { q.page } else { 1 };
    let skip = ((page - 1) * size).max(0) as usize;
    let results: Vec<Value> = with_time
        .into_iter()
        .skip(skip)
        .take(size as usize)
        .map(|(_, v)| v)
        .collect();

    StdResp::ok(
        serde_json::to_value(PBHPage { page, size, total, results }).unwrap(),
    )
}

// ---------------------------------------------------------------------------
// bans/logs + ranks + alerts
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogsQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
    #[serde(default)]
    order_by: Vec<String>,
}

fn parse_order(order_by: &[String]) -> Vec<(String, bool)> {
    let remap = |dto: &str| -> Option<&'static str> {
        Some(match dto {
            "banAt" | "ban_at" => "ban_at",
            "unbanAt" | "unban_at" => "unban_at",
            "peerIp" | "ip" => "ip",
            "peerPort" | "port" => "port",
            "peerId" | "peer_id" => "peer_id",
            "peerClientName" | "peer_client_name" => "peer_client_name",
            "peerUploaded" | "peer_uploaded" => "peer_uploaded",
            "peerDownloaded" | "peer_downloaded" => "peer_downloaded",
            "peerProgress" | "peer_progress" => "peer_progress",
            "module" | "module_name" => "module_name",
            "rule" | "rule_name" => "rule_name",
            "description" => "description",
            _ => return None,
        })
    };
    let mut out = Vec::new();
    for item in order_by {
        let mut sp = item.split('|');
        let field = sp.next().unwrap_or("");
        let dir = sp.next().unwrap_or("asc");
        if let Some(col) = remap(field) {
            out.push((col.to_string(), !dir.eq_ignore_ascii_case("desc") && !dir.eq_ignore_ascii_case("descend")));
        }
    }
    out
}

fn bake_tc(locale: &str, lang: &LangStore, tc_json: &str) -> String {
    match serde_json::from_str::<pbh_core::translate::TranslationComponent>(tc_json) {
        Ok(tc) => lang.tlc(locale, &tc),
        Err(_) => tc_json.to_string(),
    }
}

async fn handle_ban_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LogsQuery>,
) -> Response {
    let ctx = req_ctx(&headers);
    let size = if q.page_size > 0 { q.page_size } else { 10 };
    let page = if q.page > 0 { q.page } else { 1 };
    let order = parse_order(&q.order_by);
    match state.engine.db.get_ban_logs(page, size, &order) {
        Ok((total, rows)) => {
            let results: Vec<Value> = rows.iter().map(|r| ban_log_dto(&ctx, &state, r)).collect();
            StdResp::ok(
                serde_json::to_value(PBHPage { page, size, total, results }).unwrap(),
            )
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

#[derive(Deserialize)]
struct RanksQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
    #[serde(default)]
    filter: Option<String>,
}

async fn handle_ban_ranks(
    State(state): State<AppState>,
    Query(q): Query<RanksQuery>,
) -> Response {
    let size = if q.page_size > 0 { q.page_size } else { 10 };
    let page = if q.page > 0 { q.page } else { 1 };
    match state.engine.db.get_banned_ips(page, size, q.filter.as_deref()) {
        Ok((total, rows)) => {
            let results: Vec<Value> = rows
                .into_iter()
                .map(|(ip, count)| json!({ "key": ip, "value": count }))
                .collect();
            StdResp::ok(
                serde_json::to_value(PBHPage { page, size, total, results }).unwrap(),
            )
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_alerts(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ctx = req_ctx(&headers);
    match state.engine.db.get_unread_alerts() {
        Ok(rows) => {
            let list: Vec<Value> = rows
                .iter()
                .map(|a| {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(a.id));
                    m.insert("createAt".into(), json!(a.create_at));
                    if let Some(r) = a.read_at {
                        m.insert("readAt".into(), json!(r));
                    }
                    m.insert("level".into(), json!(a.level));
                    m.insert("identifier".into(), json!(a.identifier));
                    m.insert("title".into(), json!(bake_tc(&ctx.locale, &state.lang, &a.title)));
                    m.insert("content".into(), json!(bake_tc(&ctx.locale, &state.lang, &a.content)));
                    Value::Object(m)
                })
                .collect();
            StdResp::ok(Value::Array(list))
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_alert_dismiss(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Response {
    match state
        .engine
        .db
        .dismiss_alert(id, pbh_core::model::Millis::now().millis())
    {
        Ok(true) => StdResp::ok_msg("OK".to_string(), None),
        Ok(false) => StdResp::err(StatusCode::OK, "Alert not exists or expired".to_string(), None),
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_alert_dismiss_all(State(state): State<AppState>) -> Response {
    let _ = state
        .engine
        .db
        .dismiss_all_alerts(pbh_core::model::Millis::now().millis());
    StdResp::ok_msg("OK!".to_string(), None)
}

async fn handle_alert_delete(State(state): State<AppState>, AxPath(id): AxPath<i64>) -> Response {
    let _ = state.engine.db.delete_alert(id);
    StdResp::ok_msg("OK".to_string(), None)
}

async fn handle_ban_add(State(state): State<AppState>, body: Bytes) -> Response {
    let items: Vec<String> = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return StdResp::err(StatusCode::BAD_REQUEST, "Invalid request body".to_string(), None)
        }
    };
    let mut count = 0usize;
    for s in &items {
        state.engine.schedule_ban_peer_no_assign(s);
        count += 1;
    }
    StdResp::ok(json!({ "count": count }))
}

async fn handle_ban_delete(State(state): State<AppState>, body: Bytes) -> Response {
    let items: Vec<String> = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return StdResp::err(StatusCode::BAD_REQUEST, "Invalid request body".to_string(), None)
        }
    };
    if items.iter().any(|s| s == "*") {
        let n = state.engine.unban_all();
        return StdResp::ok(json!({ "count": n }));
    }
    let nets: Vec<IpNet> = items
        .iter()
        .filter_map(|s| pbh_core::ipset::parse_cidr(s))
        .collect();
    let removed = state.engine.unban_addresses(&nets);
    StdResp::ok(json!({ "count": removed }))
}

// ---------------------------------------------------------------------------
// downloaders
// ---------------------------------------------------------------------------

async fn handle_downloaders(State(state): State<AppState>) -> Response {
    let list: Vec<Value> = state
        .engine
        .downloaders
        .iter()
        .iter()
        .map(|d| {
            json!({
                "id": d.id(),
                "name": d.name(),
                "endpoint": d.endpoint(),
                "type": d.downloader_type(),
                "paused": d.is_paused(),
            })
        })
        .collect();
    StdResp::ok(Value::Array(list))
}

async fn handle_downloader_get(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    match state.engine.downloaders.find(&id) {
        Some(d) => {
            let mut dto = Map::new();
            dto.insert("id".into(), Value::String(d.id().to_string()));
            dto.insert("name".into(), Value::String(d.name().to_string()));
            dto.insert("endpoint".into(), Value::String(d.endpoint().to_string()));
            dto.insert("type".into(), Value::String(d.downloader_type().to_string()));
            dto.insert("paused".into(), Value::Bool(d.is_paused()));
            dto.insert("config".into(), d.config_json());
            StdResp::ok(Value::Object(dto))
        }
        None => StdResp::err(StatusCode::NOT_FOUND, "Downloader not found".to_string(), None),
    }
}

async fn handle_downloader_status(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Response {
    match state.engine.downloaders.find(&id) {
        Some(d) => {
            let snap = state.engine.downloaders.get_status(&id);
            StdResp::ok(json!({
                "lastStatus": snap.last_status,
                "lastStatusMessage": snap.last_status_message,
                "activeTorrents": snap.active_torrents,
                "activePeers": snap.active_peers,
                "config": d.config_json(),
                "paused": d.is_paused(),
            }))
        }
        None => StdResp::err(StatusCode::NOT_FOUND, "Downloader not found".to_string(), None),
    }
}

async fn handle_downloader_torrents(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Response {
    let Some(d) = state.engine.downloaders.find(&id) else {
        return StdResp::err(StatusCode::NOT_FOUND, "Downloader not found".to_string(), None);
    };
    match d.get_torrents().await {
        Ok(list) => {
            let arr: Vec<Value> = list
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id, "name": t.name, "hash": t.hash, "size": t.size,
                        "completedSize": t.completed_size, "progress": t.progress,
                        "rtUploadSpeed": t.rt_upload_speed, "rtDownloadSpeed": t.rt_download_speed,
                        "privateTorrent": t.private_torrent,
                    })
                })
                .collect();
            StdResp::ok(Value::Array(arr))
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_downloader_torrent_peers(
    State(state): State<AppState>,
    AxPath((id, tid)): AxPath<(String, String)>,
) -> Response {
    let Some(d) = state.engine.downloaders.find(&id) else {
        return StdResp::err(StatusCode::NOT_FOUND, "Downloader not found".to_string(), None);
    };
    let torrent = pbh_core::model::Torrent {
        id: tid.clone(),
        name: String::new(),
        hash: tid.clone(),
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
    match d.get_peers(&torrent).await {
        Ok(peers) => {
            let arr: Vec<Value> = peers
                .iter()
                .map(|p| {
                    json!({
                        "address": p.address,
                        "id": p.peer_id,
                        "clientName": p.client_name,
                        "downloaded": p.downloaded,
                        "downloadSpeed": p.download_speed,
                        "uploaded": p.uploaded,
                        "uploadSpeed": p.upload_speed,
                        "progress": p.progress,
                        "flags": p.flags,
                    })
                })
                .collect();
            StdResp::ok(Value::Array(arr))
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

#[derive(Deserialize)]
struct DraftDownloader {
    #[serde(default)]
    id: Option<String>,
    config: Value,
}

async fn test_downloader_common(state: &AppState, ctx: &ReqCtx, id: &str, config: &Value) -> Response {
    let Some(dl) = create_downloader_from_json(id, config, &state.cfg) else {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state.lang.tl(&ctx.locale, "DOWNLOADER_API_ADD_FAILURE", &[]),
            None,
        );
    };
    if dl.is_paused() {
        return StdResp::ok_msg(
            state.lang.tl(&ctx.locale, "DOWNLOADER_API_TEST_BYPASS_PAUSED", &[]),
            None,
        );
    }
    let result = dl.login().await;
    if result.is_success() {
        StdResp::ok_msg(state.lang.tl(&ctx.locale, "DOWNLOADER_API_TEST_OK", &[]), None)
    } else {
        StdResp::err(
            StatusCode::OK,
            state.lang.tlc(&ctx.locale, &result.message),
            None,
        )
    }
}

async fn handle_downloader_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(draft): Json<DraftDownloader>,
) -> Response {
    let ctx = req_ctx(&headers);
    let id = draft.id.clone().unwrap_or_else(|| "test".to_string());
    test_downloader_common(&state, &ctx, &id, &draft.config).await
}

async fn handle_downloader_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(draft): Json<DraftDownloader>,
) -> Response {
    let ctx = req_ctx(&headers);
    let Some(id) = draft.id.clone() else {
        return StdResp::err(StatusCode::BAD_REQUEST, "Missing id".to_string(), None);
    };
    let Some(dl) = create_downloader_from_json(&id, &draft.config, &state.cfg) else {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state.lang.tl(&ctx.locale, "DOWNLOADER_API_ADD_FAILURE", &[]),
            None,
        );
    };
    if !state.engine.downloaders.register(dl) {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state
                .lang
                .tl(&ctx.locale, "DOWNLOADER_API_CREATION_FAILED_ALREADY_EXISTS", &[]),
            None,
        );
    }
    if let Err(e) = state.engine.downloaders.save_downloaders() {
        tracing::error!("Failed to save downloaders: {}", e);
        return StdResp::err(
            StatusCode::INTERNAL_SERVER_ERROR,
            state
                .lang
                .tl(&ctx.locale, "DOWNLOADER_API_CREATION_FAILED_IO_EXCEPTION", &[]),
            None,
        );
    }
    StdResp::created(
        state.lang.tl(&ctx.locale, "DOWNLOADER_API_CREATED", &[]),
        None,
    )
}

async fn handle_downloader_patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    Json(draft): Json<DraftDownloader>,
) -> Response {
    let ctx = req_ctx(&headers);
    let Some(dl) = create_downloader_from_json(&id, &draft.config, &state.cfg) else {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state.lang.tl(&ctx.locale, "DOWNLOADER_API_UPDATE_FAILURE", &[]),
            None,
        );
    };
    state.engine.downloaders.unregister(&id);
    if !state.engine.downloaders.register(dl) {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state
                .lang
                .tl(&ctx.locale, "DOWNLOADER_API_UPDATE_FAILURE_ALREADY_EXISTS", &[]),
            None,
        );
    }
    if let Err(e) = state.engine.downloaders.save_downloaders() {
        tracing::error!("Failed to save downloaders: {}", e);
        return StdResp::err(
            StatusCode::INTERNAL_SERVER_ERROR,
            state
                .lang
                .tl(&ctx.locale, "DOWNLOADER_API_CREATION_FAILED_IO_EXCEPTION", &[]),
            None,
        );
    }
    StdResp::ok_msg(state.lang.tl(&ctx.locale, "DOWNLOADER_API_UPDATED", &[]), None)
}

async fn handle_downloader_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
) -> Response {
    let ctx = req_ctx(&headers);
    if state.engine.downloaders.find(&id).is_none() {
        return StdResp::err(
            StatusCode::NOT_FOUND,
            state.lang.tl(&ctx.locale, "DOWNLOADER_API_REMOVE_NOT_EXISTS", &[]),
            None,
        );
    }
    state.engine.downloaders.unregister(&id);
    if let Err(e) = state.engine.downloaders.save_downloaders() {
        tracing::error!("Failed to save downloaders: {}", e);
        return StdResp::err(
            StatusCode::INTERNAL_SERVER_ERROR,
            state
                .lang
                .tl(&ctx.locale, "DOWNLOADER_API_CREATION_FAILED_IO_EXCEPTION", &[]),
            None,
        );
    }
    let _ = &ctx;
    StdResp::ok_msg(
        state.lang.tl(&ctx.locale, "DOWNLOADER_API_REMOVE_SAVED", &[]),
        None,
    )
}

// ---------------------------------------------------------------------------
// OOBE
// ---------------------------------------------------------------------------

fn oobe_blocked(state: &AppState, ctx: &ReqCtx) -> Option<Response> {
    if !state.token.read().is_empty() {
        return Some(StdResp::err(
            StatusCode::FORBIDDEN,
            state.lang.tl(&ctx.locale, "OOBE_DISALLOW_REINIT", &[]),
            None,
        ));
    }
    None
}

#[derive(Deserialize)]
struct OobeInitBody {
    token: String,
    downloader: DraftDownloader,
    #[serde(default)]
    btn: Option<Value>,
    #[serde(default)]
    database: Option<Value>,
}

async fn handle_oobe_init(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OobeInitBody>,
) -> Response {
    let ctx = req_ctx(&headers);
    if let Some(r) = oobe_blocked(&state, &ctx) {
        return r;
    }
    // 验证下载器
    let id = body
        .downloader
        .id
        .clone()
        .unwrap_or_else(|| "downloader".to_string());
    let Some(dl) = create_downloader_from_json(&id, &body.downloader.config, &state.cfg) else {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state.lang.tl(&ctx.locale, "DOWNLOADER_API_ADD_FAILURE", &[]),
            None,
        );
    };
    if !dl.is_paused() {
        let r = dl.login().await;
        if !r.is_success() {
            return StdResp::err(StatusCode::OK, state.lang.tlc(&ctx.locale, &r.message), None);
        }
    }

    // 写 token
    {
        use pbh_config::yaml_file::{YamlFile, YValue};
        let mut file = match YamlFile::load(state.cfg.main_file()) {
            Ok(f) => f,
            Err(e) => {
                return StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None);
            }
        };
        if let Err(e) = file.set("server.token", &YValue::Str(body.token.clone())) {
            return StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None);
        }
        // BTN 设置（上游同字段；pbh-rs 未实现 BTN 客户端，但保留写回兼容）
        if let Some(btn) = &body.btn {
            for (jk, yk) in [
                ("enabled", "btn.enabled"),
                ("submit", "btn.submit"),
            ] {
                if let Some(v) = btn.get(jk) {
                    if !v.is_null() {
                        let _ = file.set(yk, &pbh_config::YValue::from_json(v));
                    }
                }
            }
            for (jk, yk) in [
                ("app_id", "btn.app-id"),
                ("app_secret", "btn.app-secret"),
            ] {
                if let Some(v) = btn.get(jk).and_then(|v| v.as_str()) {
                    let _ = file.set(yk, &pbh_config::YValue::Str(v.to_string()));
                }
            }
        }
        if let Some(db) = &body.database {
            if let Some(t) = db.get("type").and_then(|v| v.as_str()) {
                let _ = file.set("database.type", &pbh_config::YValue::Str(t.to_string()));
            }
        }
        if let Err(e) = file.save() {
            return StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None);
        }
    }
    // 重载内存配置视图（上游直接改写常驻 YamlConfiguration，等价）
    let _ = state.cfg.reload();
    *state.token.write() = body.token.clone();

    if !state.engine.downloaders.register(dl) {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state
                .lang
                .tl(&ctx.locale, "DOWNLOADER_API_CREATION_FAILED_ALREADY_EXISTS", &[]),
            None,
        );
    }
    if let Err(e) = state.engine.downloaders.save_downloaders() {
        tracing::error!("Failed to save downloaders: {}", e);
        return StdResp::err(
            StatusCode::INTERNAL_SERVER_ERROR,
            state
                .lang
                .tl(&ctx.locale, "DOWNLOADER_API_CREATION_FAILED_IO_EXCEPTION", &[]),
            None,
        );
    }
    StdResp::created(
        state.lang.tl(&ctx.locale, "DOWNLOADER_API_CREATED", &[]),
        None,
    )
}

async fn handle_oobe_test_downloader(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(draft): Json<DraftDownloader>,
) -> Response {
    let ctx = req_ctx(&headers);
    if let Some(r) = oobe_blocked(&state, &ctx) {
        return r;
    }
    let id = draft.id.clone().unwrap_or_else(|| "test".to_string());
    test_downloader_common(&state, &ctx, &id, &draft.config).await
}

async fn handle_oobe_scan(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ctx = req_ctx(&headers);
    if let Some(r) = oobe_blocked(&state, &ctx) {
        return r;
    }
    // 下载器自动发现暂未实现（M3），返回空列表（WebUI 支持手动填写）
    StdResp::ok(Value::Array(Vec::new()))
}

async fn handle_oobe_test_db(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let ctx = req_ctx(&headers);
    if let Some(r) = oobe_blocked(&state, &ctx) {
        return r;
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let dtype = v.get("type").and_then(|t| t.as_str()).unwrap_or("sqlite");
    if dtype == "sqlite" {
        StdResp::ok_msg(state.lang.tl(&ctx.locale, "STATUS_TEXT_OK", &[]), None)
    } else {
        StdResp::err(
            StatusCode::BAD_REQUEST,
            "pbh-rs MVP only supports sqlite".to_string(),
            None,
        )
    }
}

// ---------------------------------------------------------------------------
// general：status / global / config / profile / reload / modules
// ---------------------------------------------------------------------------

async fn handle_status(State(state): State<AppState>) -> Response {
    let stats = &state.engine.stats;
    let cfg = state.cfg.get();

    // system
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let load = read_loadavg().unwrap_or(0.0);
    let (mem_total, mem_avail) = read_meminfo();

    // pbh 进程内存（对应 jvm 段的近似）
    let rss_kb = read_self_rss_kb().unwrap_or(0);

    let data = json!({
        "jvm": {
            "memory": {
                "used": rss_kb * 1024,
                "max": mem_total.saturating_sub(mem_avail),
                "committed": rss_kb * 1024,
            },
            "threads": 0,
        },
        "system": {
            "os": {
                "architecture": arch,
                "cores": cores,
                "load": load,
                "name": os,
                "version": "",
            },
            "memory": {
                "total": mem_total,
                "available": mem_avail,
            },
            "disk": { "total": 0, "available": 0 },
            "network": { "interfaces": [] },
        },
        "peerbanhelper": {
            "version": state.version,
            "commit_id": option_env!("PBH_RS_COMMIT").unwrap_or("dev"),
            "compile_time": 0,
            "release": "pbh-rs",
            "uptime": state.engine.uptime_ms() / 1000,
            "data_dir": state.cfg.data_dir.to_string_lossy(),
            "database_type": "SQLITE",
            "bannedPeers": state.engine.banlist_size(),
            "lastBanWaveCheckAt": stats.last_wave_at.load(Ordering::Relaxed) as i64,
            "lastBanWaveDuration": stats.last_wave_duration_ms.load(Ordering::Relaxed),
            "totalBanned": stats.total_banned.load(Ordering::Relaxed),
            "totalUnbanned": stats.total_unbanned.load(Ordering::Relaxed),
            "wavesRun": stats.waves_run.load(Ordering::Relaxed),
            "checkInterval": cfg.check_interval_ms(),
        },
    });
    StdResp::ok(data)
}

fn read_loadavg() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    s.split_whitespace().next()?.parse().ok()
}

fn read_meminfo() -> (i64, i64) {
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        let mut total = 0i64;
        let mut avail = 0i64;
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("MemTotal:") {
                total = v.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0) * 1024;
            } else if let Some(v) = line.strip_prefix("MemAvailable:") {
                avail = v.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0) * 1024;
            }
        }
        return (total, avail);
    }
    (0, 0)
}

fn read_self_rss_kb() -> Option<i64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            return v.trim().trim_end_matches(" kB").trim().parse().ok();
        }
    }
    None
}

async fn handle_global_get(State(state): State<AppState>) -> Response {
    let cfg = state.cfg.get();
    StdResp::ok(json!({
        "globalPaused": state.engine.global_paused.load(Ordering::Relaxed),
        "analytics": cfg.main.get_bool("privacy.analytics", true),
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GlobalPatchBody {
    global_paused: Option<bool>,
}

async fn handle_global_patch(
    State(state): State<AppState>,
    Json(body): Json<GlobalPatchBody>,
) -> Response {
    if let Some(p) = body.global_paused {
        state.engine.global_paused.store(p, Ordering::Relaxed);
    }
    StdResp::ok_msg("OK!".to_string(), None)
}

async fn handle_config_get(
    State(state): State<AppState>,
    AxPath(name): AxPath<String>,
) -> Response {
    let cfg = state.cfg.get();
    let data = match name.as_str() {
        "config" => pbh_config::config_json_view(&cfg.main, "config"),
        "profile" => pbh_config::config_json_view(&cfg.profile, "profile"),
        _ => return StdResp::err(StatusCode::NOT_FOUND, "Unknown config".to_string(), None),
    };
    StdResp::ok(data)
}

async fn handle_config_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(name): AxPath<String>,
    body: Bytes,
) -> Response {
    let ctx = req_ctx(&headers);
    let which = match name.as_str() {
        "config" => "config",
        "profile" => "profile",
        _ => return StdResp::err(StatusCode::NOT_FOUND, "Unknown config".to_string(), None),
    };
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return StdResp::err(StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e), None)
        }
    };
    let path = if which == "config" {
        state.cfg.main_file()
    } else {
        state.cfg.profile_file()
    };
    if let Err(e) = pbh_config::merge_json_into_file(&path, &v, which) {
        return StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None);
    }
    // 热重载（对应上游 handleReloading）
    let entries = do_reload(&state);
    if which == "config" {
        let token = state.cfg.get().token();
        *state.token.write() = token;
    }
    let data: Vec<Value> = entries
        .into_iter()
        .map(|(n, s)| json!({ "entryName": n, "status": s }))
        .collect();
    json_resp(
        StatusCode::CREATED,
        StdResp {
            success: true,
            message: Some(state.lang.tl(&ctx.locale, "RELOAD_RESULT_SUCCESS", &[])),
            data: Some(Value::Array(data)),
        },
    )
}

async fn handle_reload(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ctx = req_ctx(&headers);
    let entries = do_reload(&state);
    let token = state.cfg.get().token();
    *state.token.write() = token;
    let data: Vec<Value> = entries
        .into_iter()
        .map(|(n, s)| json!({ "entryName": n, "status": s }))
        .collect();
    StdResp::ok_msg(
        state.lang.tl(&ctx.locale, "RELOAD_RESULT_SUCCESS", &[]),
        Some(Value::Array(data)),
    )
}

fn do_reload(state: &AppState) -> Vec<(String, String)> {
    state.engine.reload()
}

async fn handle_module(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(name): AxPath<String>,
) -> Response {
    let ctx = req_ctx(&headers);
    for m in state.engine.modules.read().iter() {
        if m.config_name() == name {
            return StdResp::ok(m.web_data(&ctx.locale, &state.lang));
        }
    }
    StdResp::err(
        StatusCode::NOT_FOUND,
        state.lang.tl(
            &ctx.locale,
            "WEBAPI_ROUTE_NOT_EXISTS",
            &["GET".into(), format!("/api/modules/{}", name).into()],
        ),
        None,
    )
}

// ---------------------------------------------------------------------------
// BlockListController：为 Transmission/BiglyBT 等以下载 URL 方式消费封禁列表的下载器
// 提供三种格式（对应上游 /blocklist/ip、/blocklist/p2p-plain-format、/blocklist/dat-emule）
// ---------------------------------------------------------------------------

fn remapped_ban_addrs(state: &AppState) -> Vec<IpNet> {
    let cfg = state.cfg.get();
    let remap = cfg.remap_config();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (net, _meta) in state.engine.snapshot_bans() {
        for r in pbh_downloader::remap_ban_address(net, true, &remap) {
            if seen.insert(r) {
                out.push(r);
            }
        }
    }
    out
}

fn net_bounds(net: IpNet) -> (String, String) {
    let lower = net.network();
    let upper = net.broadcast();
    (lower.to_string(), upper.to_string())
}

async fn handle_blocklist_ip(State(state): State<AppState>) -> Response {
    let mut out = String::new();
    for net in remapped_ban_addrs(&state) {
        out.push_str(&net.to_string());
        out.push('\n');
    }
    ([(header::CONTENT_TYPE, "text/plain")], out).into_response()
}

async fn handle_blocklist_p2p_plain(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mut out = String::new();
    for net in remapped_ban_addrs(&state) {
        let (lo, hi) = net_bounds(net);
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
        out.push(':');
        out.push_str(&lo);
        out.push('-');
        out.push_str(&hi);
        out.push('\n');
    }
    if out.trim().is_empty() {
        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if ua.starts_with("Transmission") {
            out.push_str("TransmissionWorkaround:127.127.127.127-127.127.127.127\n");
        }
    } else if let Some(first) = out.chars().next() {
        // transmission 3.x 会尝试将内容按 zlib 解压：首字节低半字节为 8（如 '8'/'x'/'h'）
        // 时可能命中 zlib 头探测导致 "need dictionary" 错误。前置一行安全前缀规避
        // （对 4.x 无影响；规则名对解析器不透明）。
        if matches!(first, '8' | 'x' | 'X' | 'h' | 'H') {
            out.insert_str(0, "pbh:0.0.0.0-0.0.0.0\n");
        }
    }
    ([(header::CONTENT_TYPE, "text/plain")], out).into_response()
}

async fn handle_blocklist_dat_emule(State(state): State<AppState>) -> Response {
    let mut out = String::new();
    for net in remapped_ban_addrs(&state) {
        let (lo, hi) = net_bounds(net);
        out.push_str(&format!("{} - {} , 000 , {}\n", lo, hi, net.addr()));
    }
    ([(header::CONTENT_TYPE, "text/plain")], out).into_response()
}

async fn handle_manifest(State(state): State<AppState>) -> Response {
    StdResp::ok(json!({
        "version": state.version,
        "parityVersion": "9.5.1",
        "runtime": "pbh-rs",
    }))
}

async fn api_fallback(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: axum::http::Request<Body>,
) -> Response {
    let ctx = req_ctx(&headers);
    StdResp::err(
        StatusCode::METHOD_NOT_ALLOWED,
        state.lang.tl(
            &ctx.locale,
            "WEBAPI_ROUTE_NOT_EXISTS",
            &[
                req.method().to_string().into(),
                req.uri().path().to_string().into(),
            ],
        ),
        None,
    )
}

// ---------------------------------------------------------------------------
// 服务器组装
// ---------------------------------------------------------------------------

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/auth/login", post(handle_login))
        .route("/api/auth/logout", post(handle_logout))
        .route(
            "/api/bans",
            get(handle_bans).post(handle_ban_add).delete(handle_ban_delete),
        )
        .route("/api/bans/logs", get(handle_ban_logs))
        .route("/api/bans/ranks", get(handle_ban_ranks))
        .route("/api/alerts", get(handle_alerts))
        .route("/api/alert/dismissAll", post(handle_alert_dismiss_all))
        .route("/api/alert/{id}/dismiss", axum::routing::patch(handle_alert_dismiss))
        .route("/api/alert/{id}", axum::routing::delete(handle_alert_delete))
        .route("/api/downloaders", get(handle_downloaders).put(handle_downloader_put))
        .route("/api/downloaders/test", post(handle_downloader_test))
        .route("/api/downloaders/scan", post(handle_oobe_scan))
        .route(
            "/api/downloaders/{id}",
            get(handle_downloader_get)
                .patch(handle_downloader_patch)
                .delete(handle_downloader_delete),
        )
        .route("/api/downloaders/{id}/status", get(handle_downloader_status))
        .route("/api/downloaders/{id}/torrents", get(handle_downloader_torrents))
        .route(
            "/api/downloaders/{id}/torrent/{tid}/peers",
            get(handle_downloader_torrent_peers),
        )
        .route("/api/general/status", get(handle_status))
        .route(
            "/api/general/global",
            get(handle_global_get).patch(handle_global_patch),
        )
        .route("/api/general/reload", post(handle_reload))
        .route(
            "/api/general/{name}",
            get(handle_config_get).put(handle_config_put),
        )
        .route("/api/modules/{name}", get(handle_module))
        .route("/api/metadata/manifest", get(handle_manifest))
        .route("/api/oobe/init", post(handle_oobe_init))
        .route("/api/oobe/testDownloader", post(handle_oobe_test_downloader))
        .route("/api/oobe/scanDownloader", post(handle_oobe_scan))
        .route("/api/oobe/testDatabaseConfig", post(handle_oobe_test_db))
        .route("/blocklist/ip", get(handle_blocklist_ip))
        .route("/blocklist/p2p-plain-format", get(handle_blocklist_p2p_plain))
        .route("/blocklist/dat-emule", get(handle_blocklist_dat_emule))
        .route("/api/{*rest}", axum::routing::any(api_fallback))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .layer(CorsLayer::permissive());

    Router::new()
        .merge(api)
        .merge(extra::extra_routes())
        .fallback(spa_fallback)
        .with_state(state)
}

/// 静态资源 + SPA 回退（对应上游 Javalin spaRoot + staticFiles 行为）：
/// 命中文件 → 原样返回；/api/* 未匹配 → 405 JSON；其余 → index.html (200)
async fn spa_fallback(State(state): State<AppState>, req: axum::http::Request<Body>) -> Response {
    use tower::ServiceExt;
    let path = req.uri().path().to_string();
    if path.starts_with("/api") || path.starts_with("/blocklist") {
        return api_fallback(State(state), HeaderMap::new(), req).await;
    }
    // 先尝试静态文件
    let dir = state.static_dir.clone();
    let (parts, body) = req.into_parts();
    let file_req = axum::http::Request::from_parts(parts, body);
    let svc = ServeDir::new(&dir).append_index_html_on_directories(true);
    let resp = svc.oneshot(file_req).await.expect("ServeDir infallible");
    if resp.status() != StatusCode::NOT_FOUND {
        return resp.map(Body::new);
    }
    // SPA 回退：index.html（200）
    let index = dir.join("index.html");
    match tokio::fs::read(&index).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            Body::from(bytes),
        )
            .into_response(),
        Err(_) => StdResp::err(
            StatusCode::NOT_FOUND,
            "WebUI static files not found. 请将 webui dist 复制到数据目录 static/ 下".to_string(),
            None,
        ),
    }
}

pub async fn serve(state: AppState, addr: SocketAddr) -> anyhow::Result<()> {
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("WebUI/API server listening on http://{}", addr);
    axum::serve(listener, router).await?;
    Ok(())
}

// 便于 CLI 侧引用类型的别名
pub type SharedDownloaderManager = Arc<DownloaderManager>;
