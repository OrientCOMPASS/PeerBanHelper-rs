//! Batch A 路由：logs(SSE)、statistic、charts(PBH Plus gate → 402)、
//! 以及尚未实现子系统的兼容 stub（autostun/laboratory/expression-engine/pbhplus/
//! btn/sub/push/graphql），保证 WebUI 各页面可加载、行为与「未激活 Plus / 功能关闭」
//! 时的上游一致。
use crate::{loghub::LogEntryDto, AppState, PBHPage, ReqCtxPub, StdResp};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

async fn handle_logs_history(State(state): State<AppState>) -> Response {
    StdResp::ok(serde_json::to_value(state.log_hub.history()).unwrap_or(Value::Array(vec![])))
}

async fn handle_logs_live(State(state): State<AppState>) -> Response {
    let initial: Vec<Result<Event, Infallible>> = state
        .log_hub
        .history()
        .into_iter()
        .map(|e| Ok(Event::default().json_data(e).unwrap_or_else(|_| Event::default().data("{}"))))
        .collect();
    let rx = state.log_hub.subscribe();
    let live = BroadcastStream::new(rx)
        .filter_map(|r| r.ok())
        .map(|e: LogEntryDto| {
            Ok(Event::default().json_data(e).unwrap_or_else(|_| Event::default().data("{}")))
        });
    let stream = tokio_stream::iter(initial).chain(live);
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

// ---------------------------------------------------------------------------
// statistic
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeQuery {
    start_at: i64,
    end_at: i64,
    #[serde(default)]
    downloader: Option<String>,
}

async fn handle_counter(State(state): State<AppState>) -> Response {
    let stats = &state.engine.stats;
    use std::sync::atomic::Ordering;
    let banlist = state.engine.banlist_size();
    let tracked = state.engine.db.count_tracked_swarm();
    let banned = stats.total_banned.load(Ordering::Relaxed);
    StdResp::ok(json!({
        "checkCounter": stats.waves_run.load(Ordering::Relaxed),
        "peerBanCounter": banned,
        "peerUnbanCounter": stats.total_unbanned.load(Ordering::Relaxed),
        "banlistCounter": banlist,
        "bannedIpCounter": banlist,
        "wastedTraffic": 0,
        "trackedSwarmCount": tracked,
        "peersBlockRate": if tracked > 0 { banned as f64 / tracked as f64 } else { 0.0 },
        "weeklySessions": 0,
    }))
}

async fn handle_ban_trends(State(state): State<AppState>, Query(q): Query<TimeQuery>) -> Response {
    match state.engine.db.ban_trends(
        q.start_at,
        q.end_at,
        q.downloader.as_deref().filter(|s| !s.is_empty()),
    ) {
        Ok(rows) => {
            let list: Vec<Value> = rows
                .into_iter()
                .map(|(day, count)| json!({ "key": day, "value": count }))
                .collect();
            StdResp::ok(Value::Array(list))
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_analysis_date(State(state): State<AppState>, Query(q): Query<TimeQuery>) -> Response {
    match state.engine.db.ban_trends(q.start_at, q.end_at, None) {
        Ok(rows) => {
            let total: i64 = rows.iter().map(|(_, c)| c).sum();
            let list: Vec<Value> = rows
                .into_iter()
                .map(|(day, count)| {
                    json!({
                        "timestamp": day,
                        "count": count,
                        "percent": if total > 0 { count as f64 * 100.0 / total as f64 } else { 0.0 },
                    })
                })
                .collect();
            StdResp::ok(Value::Array(list))
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

#[derive(Deserialize)]
struct FieldQuery {
    #[serde(rename = "type")]
    qtype: Option<String>,
    field: Option<String>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    downloader: Option<String>,
}

fn field_column(field: &str) -> Option<(&'static str, bool, Option<usize>)> {
    Some(match field {
        "peerId" | "peer_id" => ("h.peer_id", false, Some(8)),
        "peerClientName" | "peer_client_name" => ("h.peer_client_name", false, None),
        "module" | "module_name" => ("h.module_name", false, None),
        "downloader" => ("h.downloader", false, None),
        "ip" | "peerIp" => ("h.ip", false, None),
        "torrentName" | "t.name" | "name" => ("t.name", true, None),
        "peerUploaded" | "peer_uploaded" => ("h.peer_uploaded", false, None),
        "peerDownloaded" | "peer_downloaded" => ("h.peer_downloaded", false, None),
        _ => return None,
    })
}

async fn handle_analysis_field(State(state): State<AppState>, Query(q): Query<FieldQuery>) -> Response {
    let qtype = q.qtype.unwrap_or_default();
    let Some(field) = q.field.as_deref() else {
        return StdResp::err(StatusCode::BAD_REQUEST, "field cannot be null".to_string(), None);
    };
    if qtype != "count" && qtype != "sum" {
        return StdResp::err(StatusCode::BAD_REQUEST, "type invalid".to_string(), None);
    }
    let Some((column, join, substr)) = field_column(field) else {
        return StdResp::err(StatusCode::BAD_REQUEST, "field cannot be null".to_string(), None);
    };
    let filter_pct: f64 = q.filter.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    match state.engine.db.field_analysis(
        qtype == "sum",
        column,
        substr,
        q.downloader.as_deref().filter(|s| !s.is_empty()),
        filter_pct,
        join,
    ) {
        Ok(rows) => {
            let list: Vec<Value> = rows
                .into_iter()
                .map(|(data, count, percent)| {
                    json!({ "data": data, "count": count, "percent": percent })
                })
                .collect();
            StdResp::ok(Value::Array(list))
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

// ---------------------------------------------------------------------------
// charts：上游为 PBH Plus 专属；pbh-rs 无 Plus → 统一 402（与上游未激活许可一致）
// ---------------------------------------------------------------------------

async fn plus_required() -> Response {
    let msg = "PBH Plus License not activated".to_string();
    StdResp::err(StatusCode::PAYMENT_REQUIRED, msg.clone(), Some(Value::String(msg)))
}

// ---------------------------------------------------------------------------
// stubs
// ---------------------------------------------------------------------------

async fn stub_json(data: Value) -> Response {
    StdResp::ok(data)
}

async fn handle_pbhplus_status(State(_state): State<AppState>) -> Response {
    stub_json(json!({
        "activated": false,
        "valid": false,
        "type": "NOT_ACTIVATED",
        "message": "PBH Plus is not available in pbh-rs (experimental rewrite)",
    }))
    .await
}

async fn handle_not_supported(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    StdResp::err(
        StatusCode::BAD_REQUEST,
        state
            .lang
            .tl(&ctx.locale, "WEBAPI_ROUTE_NOT_EXISTS", &["GET".into(), "endpoint".into()])
            + " (not implemented in pbh-rs yet)",
        None,
    )
}

async fn handle_ipblacklist_rule(
    State(state): State<AppState>,
    axum::extract::Path(rule_type): axum::extract::Path<String>,
) -> Response {
    // 优先从已加载模块取运行期数据（含热重载后的状态），键名对齐上游 handleWebAPI
    for m in state.engine.modules.read().iter() {
        if m.config_name() == "ip-address-blocker" {
            let data = m.web_data("en_us", &state.lang);
            let key = match rule_type.as_str() {
                "nettype" => "netType",
                other => other,
            };
            let value = data.get(key).cloned().unwrap_or(Value::Array(vec![]));
            let mut out = serde_json::Map::new();
            out.insert(key.to_string(), value);
            return StdResp::ok(Value::Object(out));
        }
    }
    let cfg = state.cfg.get();
    let p = |k: &str| cfg.module_path("ip-address-blocker", k);
    let (key, data): (&str, Value) = match rule_type.as_str() {
        "ip" => ("ip", json!(cfg.profile.get_str_list(&p("ips")))),
        "port" => ("port", json!(cfg.profile.get_i64_list(&p("ports")))),
        "asn" => ("asn", json!(cfg.profile.get_i64_list(&p("asns")))),
        "region" => ("region", json!(cfg.profile.get_str_list(&p("regions")))),
        "city" => ("city", json!(cfg.profile.get_str_list(&p("cities")))),
        "netType" | "nettype" => ("netType", json!(cfg.profile.get_str_list(&p("net-type")))),
        _ => return StdResp::err(StatusCode::NOT_FOUND, "Unknown rule type".to_string(), None),
    };
    let mut out = serde_json::Map::new();
    out.insert(key.to_string(), data);
    StdResp::ok(Value::Object(out))
}

#[derive(Deserialize)]
struct IpTestQuery {
    ip: Option<String>,
}

async fn handle_ipblacklist_test(
    State(state): State<AppState>,
    Query(q): Query<IpTestQuery>,
) -> Response {
    let Some(ip) = q.ip.as_deref().and_then(|s| s.parse::<std::net::IpAddr>().ok()) else {
        return StdResp::err(StatusCode::BAD_REQUEST, "Invalid ip".to_string(), None);
    };
    // 复用模块列表中的 IP 黑名单模块做匹配测试
    for m in state.engine.modules.read().iter() {
        if m.config_name() == "ip-address-blocker" {
            let data = m.web_data("en_us", &state.lang);
            let hit_ip = data
                .get("ip")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|item| {
                        item.as_str()
                            .and_then(pbh_core::ipset::parse_cidr)
                            .map(|n| n.contains(&ip))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            return StdResp::ok(json!({ "hit": hit_ip, "ip": ip.to_string() }));
        }
    }
    StdResp::ok(json!({ "hit": false, "ip": ip.to_string() }))
}

async fn handle_check_module_available(
    State(state): State<AppState>,
    Query(q): Query<ModuleQuery>,
) -> Response {
    let name = q.module.unwrap_or_default();
    let available = state
        .engine
        .modules
        .read()
        .iter()
        .any(|m| m.name().eq_ignore_ascii_case(&name) || m.config_name() == name);
    StdResp::ok(json!({ "available": available }))
}

#[derive(Deserialize)]
struct ModuleQuery {
    module: Option<String>,
}

// ---------------------------------------------------------------------------
// torrent / peer
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TorrentQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
    #[serde(default)]
    search: Option<String>,
}

fn default_page() -> i64 {
    1
}
fn default_page_size() -> i64 {
    10
}

async fn handle_torrent_query(State(state): State<AppState>, Query(q): Query<TorrentQuery>) -> Response {
    let size = if q.page_size > 0 { q.page_size } else { 10 };
    let page = if q.page > 0 { q.page } else { 1 };
    match state.engine.db.list_torrents(page, size, q.search.as_deref()) {
        Ok((total, rows)) => {
            let results: Vec<Value> = rows
                .into_iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "infoHash": t.info_hash,
                        "name": t.name,
                        "size": t.size,
                        "privateTorrent": t.private_torrent,
                        "banCount": t.ban_count,
                    })
                })
                .collect();
            StdResp::ok(serde_json::to_value(PBHPage { page, size, total, results }).unwrap())
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_torrent_get(
    State(state): State<AppState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Response {
    match state.engine.db.list_torrents(1, 1, Some(&hash)) {
        Ok((_, rows)) => match rows.into_iter().next() {
            Some(t) => StdResp::ok(json!({
                "id": t.id, "infoHash": t.info_hash, "name": t.name,
                "size": t.size, "privateTorrent": t.private_torrent,
            })),
            None => StdResp::err(StatusCode::NOT_FOUND, "Torrent not found".to_string(), None),
        },
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_peer_get(
    State(state): State<AppState>,
    axum::extract::Path(ip): axum::extract::Path<String>,
) -> Response {
    let ban_count = state.engine.db.count_history_by_ip(&ip).unwrap_or(0);
    let banned_now = ip
        .parse::<std::net::IpAddr>()
        .map(|a| state.engine.is_banned(a))
        .unwrap_or(false);
    StdResp::ok(json!({
        "address": ip,
        "banCount": ban_count,
        "banned": banned_now,
    }))
}

async fn handle_peer_ban_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(ip): axum::extract::Path<String>,
    Query(q): Query<TorrentQuery>,
) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    let size = if q.page_size > 0 { q.page_size } else { 10 };
    let page = if q.page > 0 { q.page } else { 1 };
    match state.engine.db.get_ban_logs_by_ip(&ip, page, size) {
        Ok((total, rows)) => {
            let results: Vec<Value> = rows
                .iter()
                .map(|r| crate::ban_log_dto(&ctx, &state, r))
                .collect();
            StdResp::ok(serde_json::to_value(PBHPage { page, size, total, results }).unwrap())
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

async fn handle_peer_access_history(
    State(state): State<AppState>,
    axum::extract::Path(ip): axum::extract::Path<String>,
    Query(q): Query<TorrentQuery>,
) -> Response {
    let size = if q.page_size > 0 { q.page_size } else { 10 };
    let page = if q.page > 0 { q.page } else { 1 };
    match state.engine.db.get_access_history(&ip, page, size) {
        Ok((total, rows)) => {
            let results: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "firstTimeSeen": r.first_time_seen,
                        "lastTimeSeen": r.last_time_seen,
                        "uploaded": r.uploaded,
                        "downloaded": r.downloaded,
                        "uploadSpeed": r.upload_speed,
                        "downloadSpeed": r.download_speed,
                        "downloader": r.downloader,
                        "peerId": r.peer_id,
                        "clientName": r.client_name,
                        "torrent": {
                            "name": r.torrent_name,
                            "infoHash": r.torrent_info_hash,
                        },
                    })
                })
                .collect();
            StdResp::ok(serde_json::to_value(PBHPage { page, size, total, results }).unwrap())
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

// ---------------------------------------------------------------------------
// push（配置读写；Provider 实现在 M3）
// ---------------------------------------------------------------------------

async fn handle_push_get(State(state): State<AppState>) -> Response {
    // PushWrapperDTO {name, type, config}
    let list: Vec<Value> = state
        .engine
        .push
        .list()
        .into_iter()
        .map(|p| {
            let mut config = p.config.clone();
            if let Value::Object(m) = &mut config {
                m.remove("type");
            }
            json!({ "name": p.name, "type": p.ptype, "config": config })
        })
        .collect();
    StdResp::ok(Value::Array(list))
}

#[derive(Deserialize)]
struct PushDraft {
    name: String,
    #[serde(rename = "type")]
    ptype: String,
    #[serde(default)]
    config: Value,
}

async fn handle_push_provider_test(State(_state): State<AppState>, Json(draft): Json<PushDraft>) -> Response {
    let provider = pbh_push::PushProvider {
        name: draft.name.clone(),
        ptype: draft.ptype.clone(),
        config: draft.config.clone(),
    };
    match provider
        .send(
            "PeerBanHelper-RS Test Push",
            "This is a test push notification from pbh-rs.",
        )
        .await
    {
        Ok(()) => StdResp::ok_msg("OK".to_string(), None),
        Err(e) => StdResp::err(StatusCode::OK, e.to_string(), None),
    }
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

pub fn extra_routes() -> Router<AppState> {
    Router::new()
        // logs
        .route("/api/logs/history", get(handle_logs_history))
        .route("/api/logs/live", get(handle_logs_live))
        // statistic
        .route("/api/statistic/counter", get(handle_counter))
        .route("/api/statistic/analysis/banTrends", get(handle_ban_trends))
        .route("/api/statistic/analysis/date", get(handle_analysis_date))
        .route("/api/statistic/analysis/field", get(handle_analysis_field))
        // charts（PBH Plus gate → 402，与上游未激活行为一致）
        .route("/api/chart/geoIpInfo", get(|_: State<AppState>| async { plus_required().await }))
        .route("/api/chart/trend", get(|_: State<AppState>| async { plus_required().await }))
        .route("/api/chart/traffic", get(|_: State<AppState>| async { plus_required().await }))
        .route("/api/chart/sessionBetween", get(|_: State<AppState>| async { plus_required().await }))
        .route("/api/chart/sessionDayBucket", get(|_: State<AppState>| async { plus_required().await }))
        .route("/api/chart/sessionAnalyse", get(|_: State<AppState>| async { plus_required().await }))
        .route("/api/chart/clientAnalyse", get(|_: State<AppState>| async { plus_required().await }))
        // pbhplus
        .route("/api/pbhplus/status", get(handle_pbhplus_status))
        .route("/api/pbhplus/key", post(handle_pbhplus_unsupported))
        .route("/api/pbhplus/renewFreeLicense", post(handle_pbhplus_unsupported))
        .route("/api/pbhplus/claimRenewFreeLicenseCaptcha", get(handle_pbhplus_unsupported))
        // laboratory
        .route("/api/laboratory/experiments", get(|_: State<AppState>| async { stub_json(Value::Array(vec![])).await }))
        .route("/api/laboratory/config", get(|_: State<AppState>| async { stub_json(json!({})).await }))
        .route("/api/laboratory/isExperimentActivated", get(|_: State<AppState>| async { stub_json(Value::Bool(false)).await }))
        .route("/api/laboratory/experiment/{id}", get(handle_not_supported))
        // expression engine（已决策禁用）
        .route("/api/expression-engine/scripts", get(|_: State<AppState>| async { stub_json(Value::Array(vec![])).await }))
        .route("/api/expression-engine/editable", get(|_: State<AppState>| async { stub_json(Value::Bool(false)).await }))
        .route("/api/expression-engine/{id}", get(handle_not_supported))
        // autostun（M4）
        .route("/api/autostun/status", get(|_: State<AppState>| async { stub_json(json!({"enabled": false, "natType": "UNKNOWN"})).await }))
        .route("/api/autostun/config", get(|_: State<AppState>| async { stub_json(json!({"enabled": false})).await }))
        .route("/api/autostun/tunnels", get(|_: State<AppState>| async { stub_json(Value::Array(vec![])).await }))
        .route("/api/autostun/refreshNatType", post(handle_not_supported))
        .route("/api/autostun/restart", post(handle_not_supported))
        .route("/api/autostun/tunnel/{id}/info", get(handle_not_supported))
        .route("/api/autostun/tunnel/{id}/connections", get(handle_not_supported))
        // btn（已决策禁用）
        .route("/api/modules/btn", get(|_: State<AppState>| async { stub_json(json!({"enabled": false, "status": "DISABLED_IN_PBH_RS"})).await }))
        // ipblacklist 视图
        .route("/api/modules/ipblacklist/ip/test", get(handle_ipblacklist_test))
        .route("/api/modules/ipblacklist/{ruleType}", get(handle_ipblacklist_rule))
        // general 杂项
        .route("/api/general/checkModuleAvailable", get(handle_check_module_available))
        .route("/api/general/heapdump", get(handle_heapdump))
        .route("/api/general/stacktrace", get(handle_stacktrace))
        .route("/api/general/refreshNatStatus", post(handle_not_supported))
        // graphql（上游 EasterEgg/其它用途）→ 405
        .route("/api/graphql", post(handle_not_supported))
        // push
        .route("/api/push", get(handle_push_get).put(handle_push_put))
        .route("/api/push/test", post(handle_push_provider_test))
        .route("/api/push/{name}", delete(handle_push_delete))
        // rule subscription（IPBlackRuleList）
        .route("/api/sub/rules", get(handle_sub_rules))
        .route("/api/sub/rules/update", post(handle_sub_update_all))
        .route("/api/sub/rule", put(handle_sub_rule_put))
        .route(
            "/api/sub/rule/{id}",
            get(handle_sub_rule_get)
                .post(handle_sub_rule_put_with_id)
                .delete(handle_sub_rule_delete)
                .put(handle_sub_rule_put_with_id),
        )
        .route("/api/sub/rule/{id}/update", post(handle_sub_rule_update))
        .route("/api/sub/logs", get(handle_sub_logs_all))
        .route("/api/sub/logs/{id}", get(handle_sub_logs_one))
        .route("/api/sub/interval", get(handle_sub_interval_get).put(handle_sub_interval_put))
        // torrent / peer
        .route("/api/torrent/query", get(handle_torrent_query))
        .route("/api/torrent/{hash}", get(handle_torrent_get))
        .route("/api/peer/{ip}", get(handle_peer_get))
        .route("/api/peer/{ip}/banHistory", get(handle_peer_ban_history))
        .route("/api/peer/{ip}/accessHistory", get(handle_peer_access_history))
        .route("/api/peer/{ip}/btnQuery", get(handle_not_supported))
        .route("/api/peer/{ip}/btnQueryIframe", get(handle_not_supported))
        // plugins（已决策砍掉，返回空列表保持兼容）
        .route("/api/plugins", get(|_: State<AppState>| async { stub_json(Value::Array(vec![])).await }))
        .route("/api/plugins/operate", post(handle_not_supported))
        // 其余未覆盖端点
        .route("/api/egg", get(|_: State<AppState>| async { stub_json(json!({"runtime":"pbh-rs"})).await }))
        .route("/api/metrics", get(|_: State<AppState>| async { stub_json(json!({})).await }))
        .route("/api/backgroundtask", get(|_: State<AppState>| async { stub_json(Value::Array(vec![])).await }))
        .route("/api/device_token/get", get(handle_device_token))
        .route("/api/config/remote_access/get", get(handle_remote_access_get))
        .route("/api/config/remote_access/set", post(handle_not_supported))
        .route("/api/config/connection/get", get(handle_connection_get))
        .route("/api/config/connection/set", post(handle_not_supported))
        .route("/api/config/new_task/get", get(handle_new_task_get))
        .route("/api/config/ipfilter/get", get(handle_ipfilter_get))
        .route("/api/config/ipfilter/set", post(handle_not_supported))
        .route("/api/config/ipfilter/query", get(handle_ipfilter_get))
        .route("/api/config/ipfilter/clear", post(handle_not_supported))
        .route("/api/config/ipfilter/download", post(handle_not_supported))
        .route("/api/config/ipfilter/upload", post(handle_not_supported))
        .route("/api/statistics_list/get", get(handle_statistics_list))
        .route("/api/downloaders/{id}/torrent/{tid}/replaceTracker", post(handle_not_supported))
}

async fn handle_pbhplus_unsupported() -> Response {
    StdResp::err(
        StatusCode::BAD_REQUEST,
        "PBH Plus is not available in pbh-rs (experimental rewrite)".to_string(),
        None,
    )
}

async fn handle_heapdump() -> Response {
    StdResp::err(
        StatusCode::NOT_IMPLEMENTED,
        "heapdump is a JVM-only feature".to_string(),
        None,
    )
}

async fn handle_stacktrace(State(state): State<AppState>) -> Response {
    // JVM threadDump 的近似：返回进程基本信息文本
    let text = format!(
        "pbh-rs {} (rust) pid={} uptime_ms={} banlist={}\n",
        state.version,
        std::process::id(),
        state.engine.uptime_ms(),
        state.engine.banlist_size()
    );
    ([(header::CONTENT_TYPE, "text/plain")], text).into_response()
}

async fn handle_push_put(State(state): State<AppState>, headers: HeaderMap, Json(draft): Json<PushDraft>) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    let exists = state.engine.push.find(&draft.name).is_some();
    if let Err(e) = pbh_push::save_provider_to_config(
        &state.cfg,
        &draft.name,
        &draft.ptype,
        &draft.config,
    ) {
        return StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None);
    }
    let _ = state.cfg.reload();
    {
        let cfg = state.cfg.get();
        state.engine.push.reload(&cfg);
    }
    if exists {
        StdResp::ok_msg(state.lang.tl(&ctx.locale, "PUSH_PROVIDER_API_UPDATED", &[]), None)
    } else {
        StdResp::created(state.lang.tl(&ctx.locale, "PUSH_PROVIDER_API_CREATED", &[]), None)
    }
}

async fn handle_push_delete(State(state): State<AppState>, axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    if let Err(e) = pbh_push::remove_provider_from_config(&state.cfg, &name) {
        return StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None);
    }
    let _ = state.cfg.reload();
    {
        let cfg = state.cfg.get();
        state.engine.push.reload(&cfg);
    }
    StdResp::ok_msg("OK".to_string(), None)
}

// ---------------------------------------------------------------------------
// 规则订阅（IPBlackRuleList）API
// ---------------------------------------------------------------------------

fn rule_sub_of(state: &AppState) -> Option<Arc<pbh_modules::rule_sub::IpBlackRuleList>> {
    state.engine.rule_sub.read().clone()
}

fn sub_rules_from_cfg(state: &AppState) -> Vec<pbh_modules::rule_sub::RuleSub> {
    let cfg = state.cfg.get();
    pbh_modules::rule_sub::IpBlackRuleList::rules_from_cfg(&cfg)
}

fn write_profile_rule(
    state: &AppState,
    id: &str,
    name: &str,
    url: &str,
    enabled: bool,
) -> Result<(), String> {
    use pbh_config::yaml_file::{YamlFile, YValue};
    let mut file = YamlFile::load(state.cfg.profile_file()).map_err(|e| e.to_string())?;
    let path = format!("module.ip-address-blocker-rules.rules.{}", id);
    file.set(
        &path,
        &YValue::Map(vec![
            ("enabled".to_string(), YValue::Bool(enabled)),
            ("name".to_string(), YValue::Str(name.to_string())),
            ("url".to_string(), YValue::Str(url.to_string())),
        ]),
    )
    .map_err(|e| e.to_string())?;
    file.save().map_err(|e| e.to_string())?;
    Ok(())
}

async fn handle_sub_rules(State(state): State<AppState>) -> Response {
    let cfg_rules = sub_rules_from_cfg(&state);
    let db_info = state.engine.db.list_rule_sub_info().unwrap_or_default();
    let list: Vec<Value> = cfg_rules
        .iter()
        .map(|r| {
            let info = db_info.iter().find(|i| i.rule_id == r.id);
            json!({
                "ruleId": r.id,
                "enabled": r.enabled,
                "ruleName": r.name,
                "subUrl": r.url,
                "lastUpdate": info.map(|i| i.last_update).unwrap_or(0),
                "entCount": info.map(|i| i.ent_count).unwrap_or(0),
            })
        })
        .collect();
    StdResp::ok(Value::Array(list))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubRuleBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn handle_sub_rule_get(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match sub_rules_from_cfg(&state).into_iter().find(|r| r.id == id) {
        Some(r) => StdResp::ok(json!({
            "ruleId": r.id, "enabled": r.enabled, "ruleName": r.name, "subUrl": r.url,
        })),
        None => StdResp::err(StatusCode::NOT_FOUND, "Rule not found".to_string(), None),
    }
}

async fn handle_sub_rule_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubRuleBody>,
) -> Response {
    sub_rule_save(state, headers, None, body).await
}

async fn handle_sub_rule_put_with_id(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(path_id): axum::extract::Path<String>,
    Json(body): Json<SubRuleBody>,
) -> Response {
    sub_rule_save(state, headers, Some(path_id), body).await
}

async fn sub_rule_save(
    state: AppState,
    headers: HeaderMap,
    path_id: Option<String>,
    body: SubRuleBody,
) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    let Some(id) = body.id.clone().or(path_id) else {
        return StdResp::err(StatusCode::BAD_REQUEST, "id cannot be null".to_string(), None);
    };
    if id.contains('.') {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            format!("Illegal character (.) in id: {}", id),
            None,
        );
    }
    let existing = sub_rules_from_cfg(&state).into_iter().find(|r| r.id == id);
    let name = body.name.clone().or_else(|| existing.as_ref().map(|r| r.name.clone())).unwrap_or_else(|| id.clone());
    if name.contains('.') {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            format!("Illegal character (.) in name: {}", name),
            None,
        );
    }
    let url = body.url.clone().or_else(|| existing.as_ref().map(|r| r.url.clone())).unwrap_or_default();
    let enabled = body.enabled.or_else(|| existing.map(|r| r.enabled)).unwrap_or(true);
    if let Err(e) = write_profile_rule(&state, &id, &name, &url, enabled) {
        return StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e, None);
    }
    // 重载配置与模块（IpBlackRuleList 启动任务会立即拉取订阅）
    let _ = state.cfg.reload();
    state.engine.reload();
    StdResp::ok_msg(state.lang.tl(&ctx.locale, "IP_BAN_RULE_SAVE_SUCCESS", &[]), None)
}

async fn handle_sub_rule_delete(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    use pbh_config::yaml_file::YamlFile;
    if let Ok(mut file) = YamlFile::load(state.cfg.profile_file()) {
        file.remove(&format!("module.ip-address-blocker-rules.rules.{}", id));
        let _ = file.save();
    }
    let _ = state.engine.db.delete_rule_sub_info(&id);
    let _ = state.cfg.reload();
    state.engine.reload();
    StdResp::ok_msg("OK".to_string(), None)
}

async fn handle_sub_rule_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    let Some(rs) = rule_sub_of(&state) else {
        return StdResp::err(StatusCode::BAD_REQUEST, "Rule subscription module disabled".to_string(), None);
    };
    let Some(rule) = sub_rules_from_cfg(&state).into_iter().find(|r| r.id == id) else {
        return StdResp::err(StatusCode::NOT_FOUND, "Rule not found".to_string(), None);
    };
    match rs.update_rule(&rule, "MANUAL").await {
        Ok(count) => StdResp::ok_msg(
            state.lang.tl(&ctx.locale, "IP_BAN_RULE_UPDATE_FINISH", &[]),
            Some(json!({ "ruleId": id, "count": count })),
        ),
        Err(e) => StdResp::err(
            StatusCode::OK,
            state.lang.tl(&ctx.locale, "IP_BAN_RULE_UPDATE_FAILED", &[rule.name.into()]) + ": " + &e,
            None,
        ),
    }
}

async fn handle_sub_update_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    let Some(rs) = rule_sub_of(&state) else {
        return StdResp::err(StatusCode::BAD_REQUEST, "Rule subscription module disabled".to_string(), None);
    };
    let results = rs.update_all("MANUAL").await;
    let data: Vec<Value> = results
        .into_iter()
        .map(|(id, r)| match r {
            Ok(c) => json!({ "ruleId": id, "success": true, "count": c }),
            Err(e) => json!({ "ruleId": id, "success": false, "error": e }),
        })
        .collect();
    StdResp::ok_msg(
        state.lang.tl(&ctx.locale, "IP_BAN_RULE_ALL_UPDATED", &[]),
        Some(Value::Array(data)),
    )
}

async fn handle_sub_logs_all(
    State(state): State<AppState>,
    Query(q): Query<LogsPageQuery>,
) -> Response {
    sub_logs(state, None, q).await
}

async fn handle_sub_logs_one(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<LogsPageQuery>,
) -> Response {
    sub_logs(state, Some(id), q).await
}

async fn sub_logs(
    state: AppState,
    id: Option<String>,
    q: LogsPageQuery,
) -> Response {
    let size = if q.page_size > 0 { q.page_size } else { 10 };
    let page = if q.page > 0 { q.page } else { 1 };
    match state.engine.db.list_rule_sub_logs(id.as_deref(), page, size) {
        Ok((total, rows)) => {
            let results: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "ruleId": r.rule_id,
                        "updateTime": r.update_time,
                        "count": r.count,
                        "updateType": r.update_type,
                    })
                })
                .collect();
            StdResp::ok(serde_json::to_value(PBHPage { page, size, total, results }).unwrap())
        }
        Err(e) => StdResp::err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogsPageQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
}

async fn handle_sub_interval_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    let interval = rule_sub_of(&state)
        .map(|rs| rs.check_interval)
        .unwrap_or_else(|| {
            let cfg = state.cfg.get();
            cfg.profile
                .get_i64(&cfg.module_path("ip-address-blocker-rules", "check-interval"), 14400000)
        });
    StdResp::ok_msg(
        state.lang.tl(&ctx.locale, "IP_BAN_RULE_CHECK_INTERVAL_QUERY_SUCCESS", &[]),
        Some(json!(interval)),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntervalBody {
    check_interval: i64,
}

async fn handle_sub_interval_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IntervalBody>,
) -> Response {
    let ctx = ReqCtxPub::from_headers(&headers);
    if body.check_interval < 60000 {
        return StdResp::err(
            StatusCode::BAD_REQUEST,
            state.lang.tl(&ctx.locale, "IP_BAN_RULE_CHECK_INTERVAL_WRONG_PARAM", &[]),
            None,
        );
    }
    use pbh_config::yaml_file::{YamlFile, YValue};
    if let Ok(mut file) = YamlFile::load(state.cfg.profile_file()) {
        let _ = file.set(
            "module.ip-address-blocker-rules.check-interval",
            &YValue::Int(body.check_interval),
        );
        let _ = file.save();
    }
    let _ = state.cfg.reload();
    state.engine.reload();
    StdResp::ok_msg(
        state.lang.tl(&ctx.locale, "IP_BAN_RULE_CHECK_INTERVAL_UPDATED", &[]),
        None,
    )
}

async fn handle_device_token(State(_state): State<AppState>) -> Response {
    stub_json(json!({ "deviceToken": "" })).await
}

async fn handle_remote_access_get(State(state): State<AppState>) -> Response {
    let cfg = state.cfg.get();
    stub_json(json!({
        "address": cfg.server_address(),
        "port": cfg.server_http_port(),
        "prefix": cfg.main.get_str("server.prefix", "http://127.0.0.1:9898"),
    }))
    .await
}

async fn handle_connection_get(State(state): State<AppState>) -> Response {
    let cfg = state.cfg.get();
    stub_json(json!({
        "setting": cfg.main.get_i64("proxy.setting", 0),
        "host": cfg.main.get_str("proxy.host", "127.0.0.1"),
        "port": cfg.main.get_i64("proxy.port", 7890),
        "nonProxyHosts": cfg.main.get_str("proxy.non-proxy-hosts", ""),
    }))
    .await
}

async fn handle_new_task_get(State(_state): State<AppState>) -> Response {
    stub_json(json!({})).await
}

async fn handle_ipfilter_get(State(_state): State<AppState>) -> Response {
    stub_json(json!({ "rules": [] })).await
}

async fn handle_statistics_list(State(state): State<AppState>) -> Response {
    // 下载器流量统计（TRAFFIC_STATS 特性）
    let mut list = Vec::new();
    for d in state.engine.downloaders.iter() {
        match d.statistics().await {
            Ok(s) => list.push(json!({
                "id": d.id(),
                "name": d.name(),
                "totalUploaded": s.total_uploaded,
                "totalDownloaded": s.total_downloaded,
            })),
            Err(e) => list.push(json!({
                "id": d.id(), "name": d.name(), "error": e.to_string(),
            })),
        }
    }
    StdResp::ok(Value::Array(list))
}

#[allow(dead_code)]
fn _unused_imports(_j: Json<Value>, _m: Map<String, Value>) {}
