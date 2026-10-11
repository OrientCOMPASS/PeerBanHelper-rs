//! pbh-geoip：GeoIP/ASN/GeoCN 查询（对应上游 util/ipdb 包）。
//!
//! - 数据源与上游一致的公开镜像（PBH-BTN GeoLite.mmdb releases 等），xz 压缩 mmdb
//! - GeoLite2-City / GeoLite2-ASN / GeoCN（含行政区划 CSV，与上游 ok_data_level3.csv 同源）
//! - IPGeoData JSON 形状与上游一致（@JsonUtil.Hidden 字段不序列化）
//! - CN/TW/HK/MO 命中时用 GeoCN 数据覆盖 city/network/as.network（mergeFrom(overwrite) 语义）
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

// ---------------------------------------------------------------------------
// IPGeoData（对应上游 util/ipdb/IPGeoData.java）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CityData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    // iso / cnProvince / cnCity / cnDistricts 在上游为 @JsonUtil.Hidden（不序列化），
    // 但参与内部匹配，故保留字段并 skip 序列化
    #[serde(skip)]
    pub iso: Option<i64>,
    #[serde(skip)]
    pub cn_province: Option<String>,
    #[serde(skip)]
    pub cn_city: Option<String>,
    #[serde(skip)]
    pub cn_districts: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountryData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsNetwork {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_length: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ASData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<AsNetwork>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_type: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpGeoData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<CityData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<CountryData>,
    #[serde(rename = "as", skip_serializing_if = "Option::is_none")]
    pub as_data: Option<ASData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkData>,
}

// ---------------------------------------------------------------------------
// 行政区划 CSV（与上游 ok_data_level3.csv 同源，GPLv3）
// ---------------------------------------------------------------------------

const DIVISION_CSV: &str = include_str!("../assets/ok_data_level3.csv");

fn load_divisions() -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in DIVISION_CSV.lines().skip(1) {
        // id,pid,deep,name,pinyin_prefix,pinyin,ext_id,ext_name（ext_name 可能带引号）
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 8 {
            continue;
        }
        let id = parts[0].trim();
        let ext_name = parts[parts.len() - 1].trim().trim_matches('"');
        if !id.is_empty() && !ext_name.is_empty() {
            map.insert(id.to_string(), ext_name.to_string());
        }
    }
    map
}

/// DivisionParser.lookup：取 division_code 的所有前缀命中，按长度顺序拼接
fn division_lookup(divisions: &HashMap<String, String>, code: &str) -> Vec<String> {
    let mut path = Vec::new();
    for i in 1..=code.len() {
        if let Some(name) = divisions.get(&code[..i]) {
            path.push(name.clone());
        }
    }
    path
}

// ---------------------------------------------------------------------------
// IpDb
// ---------------------------------------------------------------------------

type Reader = maxminddb::Reader<maxminddb::Mmap>;

pub struct IpDb {
    dir: PathBuf,
    city_name: String,
    asn_name: String,
    geocn_name: String,
    auto_update: bool,
    city: RwLock<Option<Reader>>,
    asn: RwLock<Option<Reader>>,
    geocn: RwLock<Option<Reader>>,
    divisions: HashMap<String, String>,
    cache: Mutex<HashMap<String, (Instant, Option<IpGeoData>)>>,
    shutdown: Arc<AtomicBool>,
}

const MIRRORS: &[&str] = &[
    "https://github.com/PBH-BTN/GeoLite.mmdb/releases/latest/download/",
    "https://pbh-static.paulzzh.com/ipdb/",
    "https://pbh-static.ghostchu.com/ipdb/",
];

impl IpDb {
    pub fn new(
        dir: PathBuf,
        city_name: String,
        asn_name: String,
        geocn_name: String,
        auto_update: bool,
    ) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&dir);
        let me = Arc::new(Self {
            dir,
            city_name,
            asn_name,
            geocn_name,
            auto_update,
            city: RwLock::new(None),
            asn: RwLock::new(None),
            geocn: RwLock::new(None),
            divisions: load_divisions(),
            cache: Mutex::new(HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        me.load_all();
        me
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn mmdb_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.mmdb", name))
    }

    fn load_all(&self) {
        for (name, slot) in [
            (self.city_name.clone(), &self.city),
            (self.asn_name.clone(), &self.asn),
            (self.geocn_name.clone(), &self.geocn),
        ] {
            let path = self.mmdb_path(&name);
            match Reader::open_mmap(&path) {
                Ok(r) => {
                    tracing::info!("[GeoIP] Loaded {}: {}", name, path.display());
                    *slot.write() = Some(r);
                }
                Err(_) => {
                    tracing::info!("[GeoIP] {} not available ({} missing)", name, path.display());
                }
            }
        }
    }

    fn need_update(path: &Path) -> bool {
        match std::fs::metadata(path) {
            Ok(md) => match md.modified() {
                Ok(t) => match t.elapsed() {
                    Ok(age) => age > Duration::from_secs(24 * 3600),
                    Err(_) => true,
                },
                Err(_) => true,
            },
            Err(_) => true,
        }
    }

    async fn download(&self, name: &str) -> bool {
        let target = self.mmdb_path(name);
        if target.exists() && !(self.auto_update && Self::need_update(&target)) {
            return false;
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!("PeerBanHelper-RS/", env!("CARGO_PKG_VERSION")))
            .build();
        let Ok(client) = client else {
            return false;
        };
        for mirror in MIRRORS {
            let url = format!("{}{}.mmdb.xz", mirror, name);
            tracing::info!("[GeoIP] Downloading {}", url);
            let resp = match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    tracing::warn!("[GeoIP] {} → HTTP {}", url, r.status());
                    continue;
                }
                Err(e) => {
                    tracing::warn!("[GeoIP] {} → {}", url, e);
                    continue;
                }
            };
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("[GeoIP] read body failed: {}", e);
                    continue;
                }
            };
            let mut out = Vec::new();
            if lzma_rs::xz_decompress(&mut std::io::Cursor::new(&bytes), &mut out).is_err() {
                tracing::warn!("[GeoIP] xz decompress failed for {}", url);
                continue;
            }
            let tmp = target.with_extension("mmdb.tmp");
            if std::fs::write(&tmp, &out).is_err() {
                continue;
            }
            if std::fs::rename(&tmp, &target).is_err() {
                continue;
            }
            tracing::info!("[GeoIP] {} updated ({} bytes)", name, out.len());
            return true;
        }
        false
    }

    /// 启动后台更新任务（对应上游 auto-update 行为）。
    /// 使用独立 OS 线程 + 阻塞 HTTP 客户端：xz 解压为 CPU 密集操作，
    /// 不得占用 tokio worker（否则 API 响应会被饿死）。
    pub fn spawn_updater(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("pbh-geoip-updater".to_string())
            .spawn(move || {
                loop {
                    if me.shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut changed = false;
                    for name in [me.city_name.clone(), me.asn_name.clone(), me.geocn_name.clone()] {
                        if me.download_blocking(&name) {
                            changed = true;
                        }
                        if me.shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                    if changed {
                        me.load_all();
                        me.cache.lock().clear();
                    }
                    // 每 6 小时检查一次
                    for _ in 0..(6 * 3600) {
                        if me.shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            })
            .ok();
    }

    fn download_blocking(&self, name: &str) -> bool {
        let target = self.mmdb_path(name);
        if !self.auto_update {
            return false; // ip-database.auto-update=false：不自动下载/更新
        }
        if target.exists() && !Self::need_update(&target) {
            return false;
        }
        let client = match reqwest::blocking::Client::builder()
            .user_agent(concat!("PeerBanHelper-RS/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(300))
            .build()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        for mirror in MIRRORS {
            let url = format!("{}{}.mmdb.xz", mirror, name);
            tracing::info!("[GeoIP] Downloading {}", url);
            let resp = match client.get(&url).send() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("[GeoIP] {} → {}", url, e);
                    continue;
                }
            };
            if !resp.status().is_success() {
                tracing::warn!("[GeoIP] {} → HTTP {}", url, resp.status());
                continue;
            }
            let bytes = match resp.bytes() {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("[GeoIP] {} read body → {}", url, e);
                    continue;
                }
            };
            let mut out = Vec::new();
            if lzma_rs::xz_decompress(&mut std::io::Cursor::new(&bytes), &mut out).is_err() {
                tracing::warn!("[GeoIP] xz decompress failed for {}", url);
                continue;
            }
            let tmp = target.with_extension("mmdb.tmp");
            if std::fs::write(&tmp, &out).is_err() {
                continue;
            }
            if std::fs::rename(&tmp, &target).is_err() {
                continue;
            }
            tracing::info!("[GeoIP] {} updated ({} bytes)", name, out.len());
            return true;
        }
        false
    }

    fn names_pick(v: &serde_json::Value, path: &[&str]) -> Option<String> {
        let mut cur = v;
        for p in path {
            cur = cur.get(p)?;
        }
        // names 对象：优先 zh-CN，其次 en
        cur.get("zh-CN")
            .and_then(|x| x.as_str())
            .or_else(|| cur.get("en").and_then(|x| x.as_str()))
            .map(|s| s.to_string())
    }

    fn lookup_json(slot: &RwLock<Option<Reader>>, ip: IpAddr) -> Option<serde_json::Value> {
        let guard = slot.read();
        let reader = guard.as_ref()?;
        reader.lookup::<serde_json::Value>(ip).ok().flatten()
    }

    /// 查询 IP 的 Geo 数据（对应 IPDB.query：City/ASN + CN 时 GeoCN 覆盖）
    pub fn query(&self, ip: IpAddr) -> Option<IpGeoData> {
        let key = ip.to_string();
        if let Some((at, data)) = self.cache.lock().get(&key) {
            if at.elapsed() < Duration::from_secs(3600) {
                return data.clone();
            }
        }
        let data = self.query_uncached(ip);
        let mut cache = self.cache.lock();
        if cache.len() > 10_000 {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), data.clone()));
        data
    }

    fn query_uncached(&self, ip: IpAddr) -> Option<IpGeoData> {
        let mut geo = IpGeoData::default();
        let mut have_any = false;

        if let Some(v) = Self::lookup_json(&self.city, ip) {
            have_any = true;
            let country_iso = v
                .pointer("/country/iso_code")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let country_name = Self::names_pick(&v, &["country", "names"]);
            if country_iso.is_some() || country_name.is_some() {
                geo.country = Some(CountryData {
                    name: country_name,
                    iso: country_iso,
                });
            }
            let city_name = Self::names_pick(&v, &["city", "names"]);
            if city_name.is_some() {
                geo.city = Some(CityData {
                    name: city_name,
                    ..Default::default()
                });
            }
        }

        if let Some(v) = Self::lookup_json(&self.asn, ip) {
            have_any = true;
            let number = v
                .pointer("/autonomous_system_number")
                .and_then(|x| x.as_i64());
            let org = v
                .pointer("/autonomous_system_organization")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if number.is_some() || org.is_some() {
                geo.as_data = Some(ASData {
                    number,
                    organization: org,
                    ip_address: None,
                    network: None,
                });
            }
        }

        // CN/TW/HK/MO → GeoCN 覆盖
        let is_cn_region = geo
            .country
            .as_ref()
            .and_then(|c| c.iso.as_deref())
            .map(|iso| matches!(iso, "CN" | "TW" | "HK" | "MO"))
            .unwrap_or(false);
        if is_cn_region {
            if let Some(v) = Self::lookup_json(&self.geocn, ip) {
                have_any = true;
                if let Some(code) = v.get("division_code").and_then(|x| x.as_str()) {
                    let path = division_lookup(&self.divisions, code);
                    if !path.is_empty() {
                        let province = path.first().cloned();
                        let city = path.get(1).cloned();
                        let county = path.get(2).cloned();
                        let town = path.get(3).cloned();
                        let districts = match (&county, &town) {
                            (Some(c), Some(t)) => Some(format!("{} {}", c, t)),
                            (Some(c), None) => Some(c.clone()),
                            _ => None,
                        };
                        geo.city = Some(CityData {
                            name: Some(path.join(" ")),
                            iso: code.parse::<i64>().ok(),
                            cn_province: province,
                            cn_city: city,
                            cn_districts: districts,
                        });
                    }
                } else if v.get("province").is_some() {
                    // GeoCN revision 1（直接携带 province/city 字段）
                    let province = v.get("province").and_then(|x| x.as_str()).map(String::from);
                    let city = v.get("city").and_then(|x| x.as_str()).map(String::from);
                    let districts = v.get("districts").and_then(|x| x.as_str()).map(String::from);
                    let name = [province.clone(), city.clone(), districts.clone()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" ");
                    geo.city = Some(CityData {
                        name: if name.is_empty() { None } else { Some(name) },
                        iso: None,
                        cn_province: province,
                        cn_city: city,
                        cn_districts: districts,
                    });
                }
                let isp = v.get("isp").and_then(|x| x.as_str()).map(String::from);
                let net_type = v.get("type").and_then(|x| x.as_str()).map(String::from);
                if isp.is_some() || net_type.is_some() {
                    geo.network = Some(NetworkData { isp, net_type });
                }
            }
        }

        if have_any {
            Some(geo)
        } else {
            None
        }
    }

    pub fn available(&self) -> bool {
        self.city.read().is_some() || self.asn.read().is_some() || self.geocn.read().is_some()
    }

    pub fn last_modified(&self, name: &str) -> Option<SystemTime> {
        std::fs::metadata(self.mmdb_path(name)).and_then(|m| m.modified()).ok()
    }
}

/// netType（中文）→ 配置标识 映射（对应上游 checkIPDB 的 switch）
pub fn net_type_matches(net_type: &str, configured: &[String]) -> bool {
    match net_type {
        "宽带" => configured.iter().any(|s| s == "wideband"),
        "基站" => configured.iter().any(|s| s == "baseStation"),
        "政企专线" | "专线" => configured.iter().any(|s| s == "governmentAndEnterpriseLine"),
        "业务平台" => configured.iter().any(|s| s == "businessPlatform"),
        "骨干网" => configured.iter().any(|s| s == "backboneNetwork"),
        "IP 专网" | "IP专网" => configured.iter().any(|s| s == "ipPrivateNetwork"),
        "网吧" => configured.iter().any(|s| s == "internetCafe"),
        "物联网" => configured.iter().any(|s| s == "iot"),
        "数据中心" | "IDC" => configured
            .iter()
            .any(|s| s == "dataCenter" || s == "datacenter"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisions_loaded() {
        let d = load_divisions();
        assert!(d.len() > 1000, "divisions too small: {}", d.len());
        assert_eq!(d.get("11").map(|s| s.as_str()), Some("北京市"));
    }

    #[test]
    fn division_prefix_lookup() {
        let d = load_divisions();
        let path = division_lookup(&d, "110101000000");
        assert!(!path.is_empty());
        assert_eq!(path[0], "北京市");
    }

    #[test]
    fn net_type_mapping() {
        assert!(net_type_matches("宽带", &["wideband".to_string()]));
        assert!(net_type_matches("IDC", &["dataCenter".to_string()]));
        assert!(!net_type_matches("宽带", &["baseStation".to_string()]));
    }
}
