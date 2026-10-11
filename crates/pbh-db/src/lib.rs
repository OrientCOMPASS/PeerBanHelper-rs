//! pbh-db：SQLite 持久层。
//!
//! 表结构与上游 Flyway sqlite 迁移（V1_1）逐字一致（banlist / history / torrents 及索引），
//! 因此可以直接接管 Java 版的既有数据库文件（单向兼容：Rust 版可打开 Java 版的库；
//! 切换前请自行备份，参见 README）。
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::path::Path;

pub struct Database {
    conn: Mutex<Connection>,
}

/// pcb_address / pcb_range 共用行结构
#[derive(Clone, Debug)]
pub struct PcbRow {
    pub id: Option<i64>,
    pub last_report_progress: f64,
    pub last_report_uploaded: i64,
    pub tracking_uploaded_increase_total: i64,
    pub rewind_counter: i64,
    pub progress_difference_counter: i64,
    pub first_time_seen: i64,
    pub last_time_seen: i64,
    pub downloader: String,
    pub ban_delay_window_end_at: i64,
    pub fast_pcb_test_execute_at: i64,
    pub last_torrent_completed_size: i64,
}

fn map_pcb_row(row: &rusqlite::Row) -> rusqlite::Result<PcbRow> {
    Ok(PcbRow {
        id: Some(row.get(0)?),
        last_report_progress: row.get(1)?,
        last_report_uploaded: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
        tracking_uploaded_increase_total: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
        rewind_counter: row.get(4)?,
        progress_difference_counter: row.get(5)?,
        first_time_seen: row.get(6)?,
        last_time_seen: row.get(7)?,
        downloader: row.get(8)?,
        ban_delay_window_end_at: row.get(9)?,
        fast_pcb_test_execute_at: row.get(10)?,
        last_torrent_completed_size: row.get(11)?,
    })
}

/// peer_records 写入行
#[derive(Clone, Debug)]
pub struct PeerRecordRow {
    pub address: String,
    pub port: i64,
    pub torrent_id: i64,
    pub downloader: String,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub uploaded: i64,
    pub upload_speed: i64,
    pub downloaded: i64,
    pub download_speed: i64,
    pub last_flags: Option<String>,
    pub last_time_seen: i64,
    pub peer_geoip: Option<String>,
}

/// tracked_swarm 写入行
#[derive(Clone, Debug)]
pub struct SwarmRow {
    pub ip: String,
    pub port: i64,
    pub info_hash: String,
    pub private: bool,
    pub torrent_size: i64,
    pub downloader: String,
    pub downloader_progress: f64,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub peer_progress: String,
    pub uploaded: i64,
    pub upload_speed: i64,
    pub downloaded: i64,
    pub download_speed: i64,
    pub last_flags: Option<String>,
    pub last_time_seen: i64,
}

/// traffic_journal_v3 写入行
#[derive(Clone, Debug)]
pub struct TrafficJournalRow {
    pub timestamp: i64,
    pub downloader: String,
    pub data_uploaded_at_start: i64,
    pub data_uploaded: i64,
    pub data_downloaded_at_start: i64,
    pub data_downloaded: i64,
    pub protocol_uploaded_at_start: i64,
    pub protocol_uploaded: i64,
    pub protocol_downloaded_at_start: i64,
    pub protocol_downloaded: i64,
}

/// accessHistory 行
#[derive(Clone, Debug)]
pub struct AccessHistoryRow {
    pub first_time_seen: i64,
    pub last_time_seen: i64,
    pub uploaded: i64,
    pub downloaded: i64,
    pub upload_speed: i64,
    pub download_speed: i64,
    pub downloader: String,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub torrent_name: Option<String>,
    pub torrent_info_hash: Option<String>,
}

/// torrents 表行
#[derive(Clone, Debug)]
pub struct TorrentRow {
    pub id: i64,
    pub info_hash: String,
    pub name: String,
    pub size: i64,
    pub private_torrent: bool,
    pub ban_count: i64,
}

fn map_ban_log_row(r: &rusqlite::Row) -> rusqlite::Result<BanLogRow> {
    Ok(BanLogRow {
        ban_at: r.get(0)?,
        unban_at: r.get(1)?,
        ip: r.get(2)?,
        port: r.get(3)?,
        peer_id: r.get(4)?,
        peer_client_name: r.get(5)?,
        peer_uploaded: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
        peer_downloaded: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
        peer_progress: r.get(8)?,
        module_name: r.get(9)?,
        rule_name: r.get(10)?,
        description: r.get(11)?,
        downloader: r.get(12)?,
        torrent_info_hash: r.get(13)?,
        torrent_name: r.get(14)?,
        torrent_size: r.get::<_, Option<i64>>(15)?.unwrap_or(0),
    })
}

/// 封禁日志联表行
#[derive(Clone, Debug)]
pub struct BanLogRow {
    pub ban_at: i64,
    pub unban_at: i64,
    pub ip: String,
    pub port: i64,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_uploaded: i64,
    pub peer_downloaded: i64,
    pub peer_progress: f64,
    pub module_name: String,
    pub rule_name: String,
    pub description: String,
    pub downloader: String,
    pub torrent_info_hash: Option<String>,
    pub torrent_name: Option<String>,
    pub torrent_size: i64,
}

/// rule_sub_info 行
#[derive(Clone, Debug)]
pub struct RuleSubInfoRow {
    pub rule_id: String,
    pub enabled: bool,
    pub rule_name: String,
    pub sub_url: String,
    pub last_update: i64,
    pub ent_count: i64,
}

/// rule_sub_log 行
#[derive(Clone, Debug)]
pub struct RuleSubLogRow {
    pub rule_id: String,
    pub update_time: i64,
    pub count: i64,
    pub update_type: String,
}

/// alert 表一行
#[derive(Clone, Debug)]
pub struct AlertRow {
    pub id: i64,
    pub create_at: i64,
    pub read_at: Option<i64>,
    pub level: String,
    pub identifier: String,
    pub title: String,
    pub content: String,
}

/// history 表一行（对应上游 HistoryEntity 的 sqlite 映射；时间为 epoch 毫秒，
/// rule/description/structured_data 为 Gson 兼容 JSON 文本）
pub struct HistoryRow {
    pub ban_at: i64,
    pub unban_at: i64,
    pub ip: String,
    pub port: i64,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_uploaded: i64,
    pub peer_downloaded: i64,
    pub peer_progress: f64,
    pub downloader_progress: f64,
    pub torrent_id: i64,
    pub module_name: String,
    pub rule_name: String,
    pub description: String,
    pub flags: Option<String>,
    pub downloader: String,
    pub structured_data: Option<String>,
    pub peer_geoip: Option<String>,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create db dir")?;
        }
        let conn = Connection::open(path).context("open sqlite")?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .ok();
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.ensure_schema()?;
        Ok(db)
    }

    /// 与上游 V1_1__initial_sqlite.sql 中对应表的 DDL 逐字一致（IF NOT EXISTS 化）
    fn ensure_schema(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            r#"
CREATE TABLE IF NOT EXISTS banlist
(
    address  TEXT NOT NULL PRIMARY KEY,
    metadata TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS history
(
    id               INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    ban_at           INTEGER     NOT NULL,
    unban_at         INTEGER     NOT NULL,
    ip               TEXT NOT NULL,
    port             INTEGER                   NOT NULL,
    peer_id          TEXT NULL,
    peer_client_name TEXT NULL,
    peer_uploaded    INTEGER NULL,
    peer_downloaded  INTEGER NULL,
    peer_progress REAL NOT NULL,
    downloader_progress REAL NOT NULL,
    torrent_id       INTEGER                NOT NULL,
    module_name      TEXT NOT NULL,
    rule_name        TEXT NOT NULL,
    description   TEXT NOT NULL,
    flags            TEXT NULL,
    downloader       TEXT NOT NULL,
    structured_data TEXT NULL,
    peer_geoip      TEXT NULL
);

CREATE TABLE IF NOT EXISTS torrents
(
    id   INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    info_hash       TEXT NOT NULL,
    name            TEXT NOT NULL,
    size INTEGER NOT NULL,
    private_torrent INTEGER NULL
);

CREATE TABLE IF NOT EXISTS alert
(
    id         INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    create_at  INTEGER     NOT NULL,
    read_at    INTEGER NULL,
    level      TEXT NOT NULL,
    identifier TEXT NOT NULL,
    title      TEXT NOT NULL,
    content TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_alert_alertExists ON alert (read_at, identifier);
CREATE INDEX IF NOT EXISTS idx_alert_readAt ON alert (read_at);
CREATE INDEX IF NOT EXISTS idx_alert_unreadAlerts ON alert (create_at, read_at);

CREATE TABLE IF NOT EXISTS rule_sub_info
(
    rule_id     TEXT NOT NULL PRIMARY KEY,
    enabled     INTEGER      NOT NULL,
    rule_name   TEXT NOT NULL,
    sub_url     TEXT NOT NULL,
    last_update INTEGER NULL,
    ent_count   INTEGER NULL
);

CREATE TABLE IF NOT EXISTS rule_sub_log
(
    id          INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    rule_id     TEXT NOT NULL,
    update_time INTEGER     NOT NULL,
    count       INTEGER          NOT NULL,
    update_type TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pcb_address
(
    id                               INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    ip                               TEXT NOT NULL,
    port                             INTEGER                   NOT NULL,
    torrent_id                  TEXT NOT NULL,
    last_report_progress REAL NOT NULL,
    last_report_uploaded             INTEGER NULL,
    tracking_uploaded_increase_total INTEGER NULL,
    rewind_counter                   INTEGER          NOT NULL,
    progress_difference_counter      INTEGER          NOT NULL,
    first_time_seen                  INTEGER     NOT NULL,
    last_time_seen                   INTEGER     NOT NULL,
    downloader                       TEXT NOT NULL,
    ban_delay_window_end_at          INTEGER     NOT NULL,
    fast_pcb_test_execute_at         INTEGER     NOT NULL,
    last_torrent_completed_size INTEGER       NOT NULL
);

CREATE TABLE IF NOT EXISTS pcb_range
(
    id                               INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    ip_range                          TEXT NOT NULL,
    torrent_id                  TEXT NOT NULL,
    last_report_progress REAL NOT NULL,
    last_report_uploaded             INTEGER NULL,
    tracking_uploaded_increase_total INTEGER NULL,
    rewind_counter                   INTEGER          NOT NULL,
    progress_difference_counter      INTEGER          NOT NULL,
    first_time_seen                  INTEGER     NOT NULL,
    last_time_seen                   INTEGER     NOT NULL,
    downloader                       TEXT NOT NULL,
    ban_delay_window_end_at          INTEGER     NOT NULL,
    fast_pcb_test_execute_at         INTEGER     NOT NULL,
    last_torrent_completed_size INTEGER       NOT NULL
);

CREATE TABLE IF NOT EXISTS peer_records
(
    id                INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    address           TEXT NOT NULL,
    port              INTEGER                   NOT NULL,
    torrent_id        INTEGER       NOT NULL,
    downloader        TEXT NOT NULL,
    peer_id           TEXT NULL,
    client_name       TEXT NULL,
    uploaded          INTEGER NOT NULL,
    uploaded_offset   INTEGER NOT NULL,
    upload_speed      INTEGER NOT NULL,
    downloaded        INTEGER NOT NULL,
    downloaded_offset INTEGER NOT NULL,
    download_speed    INTEGER NOT NULL,
    last_flags        TEXT NULL,
    first_time_seen   INTEGER     NOT NULL,
    last_time_seen    INTEGER     NOT NULL,
    peer_geoip        TEXT NULL
);

CREATE TABLE IF NOT EXISTS traffic_journal_v3
(
    id                                   INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    timestamp                            INTEGER     NOT NULL,
    downloader                           TEXT NOT NULL,
    data_overall_uploaded_at_start       INTEGER                NOT NULL,
    data_overall_uploaded                INTEGER                NOT NULL,
    data_overall_downloaded_at_start     INTEGER                NOT NULL,
    data_overall_downloaded              INTEGER                NOT NULL,
    protocol_overall_uploaded_at_start   INTEGER                NOT NULL,
    protocol_overall_uploaded            INTEGER                NOT NULL,
    protocol_overall_downloaded_at_start INTEGER                NOT NULL,
    protocol_overall_downloaded          INTEGER                NOT NULL
);

CREATE TABLE IF NOT EXISTS tracked_swarm
(
    id                 INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    ip                 TEXT NOT NULL,
    port               INTEGER NOT NULL,
    info_hash          TEXT NOT NULL,
    torrent_is_private INTEGER NOT NULL,
    torrent_size       INTEGER       NOT NULL,
    downloader         TEXT NOT NULL,
    downloader_progress REAL NOT NULL,
    peer_id            TEXT NULL,
    client_name        TEXT NULL,
    peer_progress      TEXT NOT NULL,
    uploaded           INTEGER       NOT NULL,
    uploaded_offset    INTEGER       NOT NULL,
    upload_speed       INTEGER       NOT NULL,
    downloaded         INTEGER       NOT NULL,
    downloaded_offset  INTEGER       NOT NULL,
    download_speed     INTEGER       NOT NULL,
    last_flags         TEXT NULL,
    first_time_seen    INTEGER       NOT NULL,
    last_time_seen     INTEGER       NOT NULL,
    download_speed_max INTEGER       NOT NULL,
    upload_speed_max   INTEGER       NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_peer_records_unique ON peer_records (address, port, torrent_id, downloader);
CREATE UNIQUE INDEX IF NOT EXISTS idx_traffic_journal_v3_unique ON traffic_journal_v3 (timestamp, downloader);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tracked_swarm_unique ON tracked_swarm (ip, port, info_hash, downloader);
CREATE INDEX IF NOT EXISTS idx_peer_records_address ON peer_records (address);
CREATE INDEX IF NOT EXISTS idx_peer_records_last_time_seen ON peer_records (last_time_seen);

CREATE INDEX IF NOT EXISTS idx_pcb_address_last_time_seen ON pcb_address (last_time_seen);
CREATE INDEX IF NOT EXISTS idx_pcb_range_last_time_seen ON pcb_range (last_time_seen);
CREATE INDEX IF NOT EXISTS idx_history_downloader ON history (downloader);
CREATE INDEX IF NOT EXISTS idx_history_ip ON history (ip);
CREATE INDEX IF NOT EXISTS idx_history_module_name ON history (module_name);
CREATE INDEX IF NOT EXISTS idx_history_peer_id ON history (peer_id);
CREATE INDEX IF NOT EXISTS idx_history_torrent_id ON history (torrent_id);
CREATE INDEX IF NOT EXISTS idx_history_view ON history (ban_at);
CREATE INDEX IF NOT EXISTS idx_torrents_name ON torrents (name);
"#,
        )
        .context("ensure schema")?;
        Ok(())
    }

    /// 上游 BanListService.saveBanList 语义：全量覆盖保存
    pub fn save_banlist(&self, entries: &[(String, String)]) -> Result<usize> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().context("begin tx")?;
        tx.execute("DELETE FROM banlist", []).context("clear banlist")?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO banlist (address, metadata) VALUES (?1, ?2)")
                .context("prepare insert")?;
            for (address, metadata) in entries {
                stmt.execute(params![address, metadata])?;
            }
        }
        tx.commit().context("commit")?;
        Ok(entries.len())
    }

    pub fn read_banlist(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT address, metadata FROM banlist")
            .context("prepare select")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query banlist")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 对应 TorrentEntity createIfNotExists（按 info_hash 查重）
    pub fn create_torrent_if_not_exists(
        &self,
        info_hash: &str,
        name: &str,
        size: i64,
        private_torrent: bool,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        let existing = {
            let mut stmt = conn
                .prepare("SELECT id FROM torrents WHERE info_hash = ?1 LIMIT 1")
                .context("prepare select torrent")?;
            stmt.query_row(params![info_hash], |row| row.get::<_, i64>(0)).ok()
        };
        if let Some(id) = existing {
            return Ok(id);
        }
        drop(conn);
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO torrents (info_hash, name, size, private_torrent) VALUES (?1, ?2, ?3, ?4)",
            params![info_hash, name, size, private_torrent as i64],
        )
        .context("insert torrent")?;
        Ok(conn.last_insert_rowid())
    }

    // ------------------- PCB (ProgressCheatBlocker) 持久层 -------------------

    /// 返回行数据（不含 dirty 标记）
    pub fn fetch_pcb_addr(
        &self,
        torrent_id: &str,
        ip: &str,
        port: i64,
        downloader: &str,
    ) -> Result<Option<PcbRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, last_report_progress, last_report_uploaded, tracking_uploaded_increase_total,
                    rewind_counter, progress_difference_counter, first_time_seen, last_time_seen,
                    downloader, ban_delay_window_end_at, fast_pcb_test_execute_at, last_torrent_completed_size
             FROM pcb_address WHERE torrent_id=?1 AND ip=?2 AND port=?3 AND downloader=?4 LIMIT 1",
        )?;
        let mut rows = stmt.query(params![torrent_id, ip, port, downloader])?;
        Ok(rows.next()?.map(map_pcb_row).transpose()?)
    }

    pub fn fetch_pcb_range(
        &self,
        torrent_id: &str,
        range: &str,
        downloader: &str,
    ) -> Result<Option<PcbRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, last_report_progress, last_report_uploaded, tracking_uploaded_increase_total,
                    rewind_counter, progress_difference_counter, first_time_seen, last_time_seen,
                    downloader, ban_delay_window_end_at, fast_pcb_test_execute_at, last_torrent_completed_size
             FROM pcb_range WHERE torrent_id=?1 AND ip_range=?2 AND downloader=?3 LIMIT 1",
        )?;
        let mut rows = stmt.query(params![torrent_id, range, downloader])?;
        Ok(rows.next()?.map(map_pcb_row).transpose()?)
    }

    pub fn upsert_pcb_addr(&self, key: (&str, &str, i64), row: &PcbRow) -> Result<i64> {
        let (torrent_id, ip, port) = key;
        let conn = self.conn.lock();
        if let Some(id) = row.id {
            conn.execute(
                "UPDATE pcb_address SET last_report_progress=?1, last_report_uploaded=?2,
                    tracking_uploaded_increase_total=?3, rewind_counter=?4, progress_difference_counter=?5,
                    first_time_seen=?6, last_time_seen=?7, downloader=?8, ban_delay_window_end_at=?9,
                    fast_pcb_test_execute_at=?10, last_torrent_completed_size=?11 WHERE id=?12",
                params![row.last_report_progress, row.last_report_uploaded, row.tracking_uploaded_increase_total,
                        row.rewind_counter, row.progress_difference_counter, row.first_time_seen,
                        row.last_time_seen, row.downloader, row.ban_delay_window_end_at,
                        row.fast_pcb_test_execute_at, row.last_torrent_completed_size, id],
            )?;
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO pcb_address (ip, port, torrent_id, last_report_progress, last_report_uploaded,
                tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                fast_pcb_test_execute_at, last_torrent_completed_size)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![ip, port, torrent_id, row.last_report_progress, row.last_report_uploaded,
                    row.tracking_uploaded_increase_total, row.rewind_counter, row.progress_difference_counter,
                    row.first_time_seen, row.last_time_seen, row.downloader, row.ban_delay_window_end_at,
                    row.fast_pcb_test_execute_at, row.last_torrent_completed_size],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn upsert_pcb_range(&self, key: (&str, &str), row: &PcbRow) -> Result<i64> {
        let (torrent_id, range) = key;
        let conn = self.conn.lock();
        if let Some(id) = row.id {
            conn.execute(
                "UPDATE pcb_range SET last_report_progress=?1, last_report_uploaded=?2,
                    tracking_uploaded_increase_total=?3, rewind_counter=?4, progress_difference_counter=?5,
                    first_time_seen=?6, last_time_seen=?7, downloader=?8, ban_delay_window_end_at=?9,
                    fast_pcb_test_execute_at=?10, last_torrent_completed_size=?11 WHERE id=?12",
                params![row.last_report_progress, row.last_report_uploaded, row.tracking_uploaded_increase_total,
                        row.rewind_counter, row.progress_difference_counter, row.first_time_seen,
                        row.last_time_seen, row.downloader, row.ban_delay_window_end_at,
                        row.fast_pcb_test_execute_at, row.last_torrent_completed_size, id],
            )?;
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO pcb_range (ip_range, torrent_id, last_report_progress, last_report_uploaded,
                tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                fast_pcb_test_execute_at, last_torrent_completed_size)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![range, torrent_id, row.last_report_progress, row.last_report_uploaded,
                    row.tracking_uploaded_increase_total, row.rewind_counter, row.progress_difference_counter,
                    row.first_time_seen, row.last_time_seen, row.downloader, row.ban_delay_window_end_at,
                    row.fast_pcb_test_execute_at, row.last_torrent_completed_size],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 对应 cleanupDatabase：删除超过 persist-duration 未出现的记录
    pub fn cleanup_pcb(&self, before_ms: i64) -> Result<usize> {
        let conn = self.conn.lock();
        let a = conn.execute("DELETE FROM pcb_address WHERE last_time_seen < ?1", params![before_ms])?;
        let b = conn.execute("DELETE FROM pcb_range WHERE last_time_seen < ?1", params![before_ms])?;
        Ok(a + b)
    }

    /// 对应 PCBAddressDao.deleteEntry（解封事件）
    pub fn delete_pcb_addr(&self, torrent_id: &str, ip: &str) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "DELETE FROM pcb_address WHERE torrent_id=?1 AND ip=?2",
            params![torrent_id, ip],
        )?)
    }

    // ------------------- 查询（bans/logs、ranks、alerts） -------------------

    /// 分页查询封禁日志（LEFT JOIN torrents 补齐种子信息）
    pub fn get_ban_logs(
        &self,
        page: i64,
        size: i64,
        order: &[(String, bool)],
    ) -> Result<(i64, Vec<BanLogRow>)> {
        let conn = self.conn.lock();
        let total: i64 =
            conn.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))?;
        let order_sql = if order.is_empty() {
            "h.ban_at DESC".to_string()
        } else {
            order
                .iter()
                .map(|(c, asc)| format!("h.{} {}", c, if *asc { "ASC" } else { "DESC" }))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let sql = format!(
            "SELECT h.ban_at, h.unban_at, h.ip, h.port, h.peer_id, h.peer_client_name,
                    h.peer_uploaded, h.peer_downloaded, h.peer_progress, h.module_name,
                    h.rule_name, h.description, h.downloader,
                    t.info_hash, t.name, t.size
             FROM history h LEFT JOIN torrents t ON h.torrent_id = t.id
             ORDER BY {} LIMIT ?1 OFFSET ?2",
            order_sql
        );
        let mut stmt = conn.prepare(&sql)?;
        let offset = (page - 1).max(0) * size;
        let mapped = stmt.query_map(params![size, offset], |r| {
            Ok(BanLogRow {
                ban_at: r.get(0)?,
                unban_at: r.get(1)?,
                ip: r.get(2)?,
                port: r.get(3)?,
                peer_id: r.get(4)?,
                peer_client_name: r.get(5)?,
                peer_uploaded: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                peer_downloaded: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                peer_progress: r.get(8)?,
                module_name: r.get(9)?,
                rule_name: r.get(10)?,
                description: r.get(11)?,
                downloader: r.get(12)?,
                torrent_info_hash: r.get(13)?,
                torrent_name: r.get(14)?,
                torrent_size: r.get::<_, Option<i64>>(15)?.unwrap_or(0),
            })
        })?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok((total, out))
    }

    /// 封禁次数排行（对应 getBannedIps[WithFilter]）
    pub fn get_banned_ips(
        &self,
        page: i64,
        size: i64,
        filter: Option<&str>,
    ) -> Result<(i64, Vec<(String, i64)>)> {
        let conn = self.conn.lock();
        let (total, rows_sql): (i64, String) = match filter {
            Some(f) if !f.is_empty() => {
                let like = format!("%{}%", f);
                let t: i64 = conn.query_row(
                    "SELECT COUNT(DISTINCT ip) FROM history WHERE ip LIKE ?1",
                    params![like],
                    |r| r.get(0),
                )?;
                (
                    t,
                    "SELECT ip, COUNT(*) AS c FROM history WHERE ip LIKE ?1 GROUP BY ip ORDER BY c DESC LIMIT ?2 OFFSET ?3".to_string(),
                )
            }
            _ => {
                let t: i64 = conn.query_row(
                    "SELECT COUNT(DISTINCT ip) FROM history",
                    [],
                    |r| r.get(0),
                )?;
                (
                    t,
                    "SELECT ip, COUNT(*) AS c FROM history GROUP BY ip ORDER BY c DESC LIMIT ?2 OFFSET ?3".to_string(),
                )
            }
        };
        let offset = (page - 1).max(0) * size;
        let mut out = Vec::new();
        match filter {
            Some(f) if !f.is_empty() => {
                let like = format!("%{}%", f);
                let mut stmt = conn.prepare(&rows_sql)?;
                let mapped = stmt.query_map(params![like, size, offset], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for r in mapped {
                    out.push(r?);
                }
            }
            _ => {
                let mut stmt = conn.prepare(&rows_sql)?;
                let mapped = stmt.query_map(params!["", size, offset], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for r in mapped {
                    out.push(r?);
                }
            }
        }
        Ok((total, out))
    }

    // ------------------- 监视数据（ActiveMonitoring/PeerRecording/SwarmTracking） -------------------

    pub fn batch_upsert_peer_records(&self, rows: &[PeerRecordRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO peer_records (address, port, torrent_id, downloader, peer_id, client_name,
                    uploaded, uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed,
                    last_flags, first_time_seen, last_time_seen, peer_geoip)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?7,?8,?9,?9,?10,?11,?12,?12,?13)
                 ON CONFLICT(address, port, torrent_id, downloader) DO UPDATE SET
                    peer_id=excluded.peer_id, client_name=excluded.client_name,
                    uploaded_offset=peer_records.uploaded, uploaded=excluded.uploaded,
                    upload_speed=excluded.upload_speed,
                    downloaded_offset=peer_records.downloaded, downloaded=excluded.downloaded,
                    download_speed=excluded.download_speed, last_flags=excluded.last_flags,
                    last_time_seen=excluded.last_time_seen, peer_geoip=excluded.peer_geoip",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.address, r.port, r.torrent_id, r.downloader, r.peer_id, r.client_name,
                    r.uploaded, r.upload_speed, r.downloaded, r.download_speed, r.last_flags,
                    r.last_time_seen, r.peer_geoip
                ])?;
            }
        }
        tx.commit()?;
        Ok(rows.len())
    }

    pub fn batch_upsert_tracked_swarm(&self, rows: &[SwarmRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tracked_swarm (ip, port, info_hash, torrent_is_private, torrent_size,
                    downloader, downloader_progress, peer_id, client_name, peer_progress,
                    uploaded, uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed,
                    last_flags, first_time_seen, last_time_seen, download_speed_max, upload_speed_max)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11,?12,?13,?13,?14,?15,?16,?16,?14,?12)
                 ON CONFLICT(ip, port, info_hash, downloader) DO UPDATE SET
                    peer_id=excluded.peer_id, client_name=excluded.client_name,
                    downloader_progress=excluded.downloader_progress, peer_progress=excluded.peer_progress,
                    uploaded_offset=tracked_swarm.uploaded, uploaded=excluded.uploaded,
                    download_speed=excluded.download_speed,
                    download_speed_max=MAX(tracked_swarm.download_speed_max, excluded.download_speed),
                    upload_speed=excluded.upload_speed,
                    upload_speed_max=MAX(tracked_swarm.upload_speed_max, excluded.upload_speed),
                    downloaded_offset=tracked_swarm.downloaded, downloaded=excluded.downloaded,
                    last_flags=excluded.last_flags, last_time_seen=excluded.last_time_seen",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.ip, r.port, r.info_hash, r.private as i64, r.torrent_size, r.downloader,
                    r.downloader_progress, r.peer_id, r.client_name, r.peer_progress,
                    r.uploaded, r.upload_speed, r.downloaded, r.download_speed,
                    r.last_flags, r.last_time_seen
                ])?;
            }
        }
        tx.commit()?;
        Ok(rows.len())
    }

    pub fn insert_traffic_journal(&self, row: &TrafficJournalRow) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO traffic_journal_v3 (timestamp, downloader,
                data_overall_uploaded_at_start, data_overall_uploaded,
                data_overall_downloaded_at_start, data_overall_downloaded,
                protocol_overall_uploaded_at_start, protocol_overall_uploaded,
                protocol_overall_downloaded_at_start, protocol_overall_downloaded)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                row.timestamp, row.downloader,
                row.data_uploaded_at_start, row.data_uploaded,
                row.data_downloaded_at_start, row.data_downloaded,
                row.protocol_uploaded_at_start, row.protocol_uploaded,
                row.protocol_downloaded_at_start, row.protocol_downloaded
            ],
        )?;
        Ok(())
    }

    pub fn count_tracked_swarm(&self) -> i64 {
        let conn = self.conn.lock();
        conn.query_row("SELECT COUNT(*) FROM tracked_swarm", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn cleanup_peer_records(&self, before_ms: i64) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "DELETE FROM peer_records WHERE last_time_seen < ?1",
            params![before_ms],
        )?)
    }

    /// /api/peer/{ip}/accessHistory：peer_records 联表 torrents
    pub fn get_access_history(
        &self,
        ip: &str,
        page: i64,
        size: i64,
    ) -> Result<(i64, Vec<AccessHistoryRow>)> {
        let conn = self.conn.lock();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM peer_records WHERE address = ?1",
            params![ip],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT p.first_time_seen, p.last_time_seen, p.uploaded, p.downloaded,
                    p.upload_speed, p.download_speed, p.downloader, p.peer_id, p.client_name,
                    t.name, t.info_hash
             FROM peer_records p LEFT JOIN torrents t ON p.torrent_id = t.id
             WHERE p.address = ?1 ORDER BY p.last_time_seen DESC LIMIT ?2 OFFSET ?3",
        )?;
        let offset = (page - 1).max(0) * size;
        let mapped = stmt.query_map(params![ip, size, offset], |r| {
            Ok(AccessHistoryRow {
                first_time_seen: r.get(0)?,
                last_time_seen: r.get(1)?,
                uploaded: r.get(2)?,
                downloaded: r.get(3)?,
                upload_speed: r.get(4)?,
                download_speed: r.get(5)?,
                downloader: r.get(6)?,
                peer_id: r.get(7)?,
                client_name: r.get(8)?,
                torrent_name: r.get(9)?,
                torrent_info_hash: r.get(10)?,
            })
        })?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok((total, out))
    }

    // ------------------- 统计查询（statistic API） -------------------

    /// 按天聚合封禁趋势（UTC 日界；对应 getStartOfToday 桶）
    pub fn ban_trends(
        &self,
        start_ms: i64,
        end_ms: i64,
        downloader: Option<&str>,
    ) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock();
        let sql = match downloader {
            Some(_) => "SELECT (ban_at/86400000)*86400000 AS day, COUNT(*) FROM history
                        WHERE ban_at >= ?1 AND ban_at <= ?2 AND downloader = ?3
                        GROUP BY day ORDER BY day",
            None => "SELECT (ban_at/86400000)*86400000 AS day, COUNT(*) FROM history
                     WHERE ban_at >= ?1 AND ban_at <= ?2
                     GROUP BY day ORDER BY day",
        };
        let mut stmt = conn.prepare(sql)?;
        let mut out = Vec::new();
        let mapper = |r: &rusqlite::Row| -> rusqlite::Result<(i64, i64)> {
            Ok((r.get(0)?, r.get(1)?))
        };
        match downloader {
            Some(d) => {
                let mapped = stmt.query_map(params![start_ms, end_ms, d], mapper)?;
                for r in mapped {
                    out.push(r?);
                }
            }
            None => {
                let mapped = stmt.query_map(params![start_ms, end_ms], mapper)?;
                for r in mapped {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// 字段聚合分析（对应 countField/sumField；percent 相对全表总量，过滤低于 filter_pct 的行）
    #[allow(clippy::too_many_arguments)]
    pub fn field_analysis(
        &self,
        sum_mode: bool,
        column: &str,
        substring: Option<usize>,
        downloader: Option<&str>,
        filter_pct: f64,
        join_torrents: bool,
    ) -> Result<Vec<(String, i64, f64)>> {
        let conn = self.conn.lock();
        let field_expr = match substring {
            Some(n) => format!("SUBSTR({}, 1, {})", column, n),
            None => column.to_string(),
        };
        let join = if join_torrents {
            " JOIN torrents t ON h.torrent_id = t.id"
        } else {
            ""
        };
        let where_dl = if downloader.is_some() {
            " WHERE h.downloader = ?1"
        } else {
            ""
        };
        let agg = if sum_mode {
            format!("SUM(CAST({} AS INTEGER))", field_expr)
        } else {
            "COUNT(*)".to_string()
        };
        let sql = format!(
            "SELECT {} AS data, {} AS c FROM history h{}{} GROUP BY data ORDER BY c DESC",
            field_expr, agg, join, where_dl
        );
        let total_sql = format!("SELECT COALESCE(SUM(x.c),0) FROM (SELECT {} AS c FROM history h{}{} GROUP BY {}) AS x",
            agg, join, where_dl, field_expr);
        let total: i64 = {
            let mut ts = conn.prepare(&total_sql)?;
            if downloader.is_some() {
                ts.query_row(params![downloader.unwrap()], |r| r.get(0))?
            } else {
                ts.query_row([], |r| r.get(0))?
            }
        };
        let mut stmt = conn.prepare(&sql)?;
        let mut out = Vec::new();
        if downloader.is_some() {
            let rows = stmt.query_map(params![downloader.unwrap()], |r| {
                let data: Option<String> = r.get(0)?;
                let c: i64 = r.get::<_, Option<i64>>(1)?.unwrap_or(0);
                Ok((data, c))
            })?;
            for r in rows {
                let (data, c) = r?;
                let pct = if total > 0 { c as f64 * 100.0 / total as f64 } else { 0.0 };
                if pct >= filter_pct {
                    out.push((data.unwrap_or_default(), c, pct));
                }
            }
        } else {
            let rows = stmt.query_map([], |r| {
                let data: Option<String> = r.get(0)?;
                let c: i64 = r.get::<_, Option<i64>>(1)?.unwrap_or(0);
                Ok((data, c))
            })?;
            for r in rows {
                let (data, c) = r?;
                let pct = if total > 0 { c as f64 * 100.0 / total as f64 } else { 0.0 };
                if pct >= filter_pct {
                    out.push((data.unwrap_or_default(), c, pct));
                }
            }
        }
        Ok(out)
    }

    /// torrents 表分页
    pub fn list_torrents(
        &self,
        page: i64,
        size: i64,
        search: Option<&str>,
    ) -> Result<(i64, Vec<TorrentRow>)> {
        let conn = self.conn.lock();
        let (total, sql, like): (i64, String, Option<String>) = match search {
            Some(f) if !f.is_empty() => {
                let like = format!("%{}%", f);
                let t: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM torrents WHERE name LIKE ?1 OR info_hash LIKE ?1",
                    params![like],
                    |r| r.get(0),
                )?;
                (
                    t,
                    "SELECT id, info_hash, name, size, private_torrent FROM torrents
                     WHERE name LIKE ?1 OR info_hash LIKE ?1 ORDER BY id DESC LIMIT ?2 OFFSET ?3"
                        .to_string(),
                    Some(like),
                )
            }
            _ => {
                let t: i64 =
                    conn.query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))?;
                (
                    t,
                    "SELECT id, info_hash, name, size, private_torrent FROM torrents
                     ORDER BY id DESC LIMIT ?2 OFFSET ?3"
                        .to_string(),
                    None,
                )
            }
        };
        let offset = (page - 1).max(0) * size;
        let mut stmt = conn.prepare(&sql)?;
        let mut out = Vec::new();
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<TorrentRow> {
            Ok(TorrentRow {
                id: r.get(0)?,
                info_hash: r.get(1)?,
                name: r.get(2)?,
                size: r.get(3)?,
                private_torrent: r.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0,
                ban_count: 0,
            })
        };
        match like {
            Some(l) => {
                let rows = stmt.query_map(params![l, size, offset], map_row)?;
                for r in rows {
                    out.push(r?);
                }
            }
            None => {
                let rows = stmt.query_map(params!["", size, offset], map_row)?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        Ok((total, out))
    }

    pub fn count_history_by_ip(&self, ip: &str) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM history WHERE ip = ?1",
            params![ip],
            |r| r.get(0),
        )?)
    }

    pub fn get_ban_logs_by_ip(
        &self,
        ip: &str,
        page: i64,
        size: i64,
    ) -> Result<(i64, Vec<BanLogRow>)> {
        let conn = self.conn.lock();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM history WHERE ip = ?1",
            params![ip],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT h.ban_at, h.unban_at, h.ip, h.port, h.peer_id, h.peer_client_name,
                    h.peer_uploaded, h.peer_downloaded, h.peer_progress, h.module_name,
                    h.rule_name, h.description, h.downloader,
                    t.info_hash, t.name, t.size
             FROM history h LEFT JOIN torrents t ON h.torrent_id = t.id
             WHERE h.ip = ?1 ORDER BY h.ban_at DESC LIMIT ?2 OFFSET ?3",
        )?;
        let offset = (page - 1).max(0) * size;
        let mapped = stmt.query_map(params![ip, size, offset], map_ban_log_row)?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok((total, out))
    }

    // ------------------- rule subscription -------------------

    pub fn upsert_rule_sub_info(
        &self,
        rule_id: &str,
        enabled: bool,
        rule_name: &str,
        sub_url: &str,
        last_update: i64,
        ent_count: i64,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO rule_sub_info (rule_id, enabled, rule_name, sub_url, last_update, ent_count)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(rule_id) DO UPDATE SET enabled=?2, rule_name=?3, sub_url=?4, last_update=?5, ent_count=?6",
            params![rule_id, enabled as i64, rule_name, sub_url, last_update, ent_count],
        )?;
        Ok(())
    }

    pub fn list_rule_sub_info(&self) -> Result<Vec<RuleSubInfoRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT rule_id, enabled, rule_name, sub_url, last_update, ent_count FROM rule_sub_info ORDER BY rule_id",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok(RuleSubInfoRow {
                rule_id: r.get(0)?,
                enabled: r.get::<_, i64>(1)? != 0,
                rule_name: r.get(2)?,
                sub_url: r.get(3)?,
                last_update: r.get(4)?,
                ent_count: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            })
        })?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_rule_sub_info(&self, rule_id: &str) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM rule_sub_info WHERE rule_id=?1", params![rule_id])?)
    }

    pub fn insert_rule_sub_log(
        &self,
        rule_id: &str,
        update_time: i64,
        count: i64,
        update_type: &str,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO rule_sub_log (rule_id, update_time, count, update_type) VALUES (?1,?2,?3,?4)",
            params![rule_id, update_time, count, update_type],
        )?;
        Ok(())
    }

    pub fn list_rule_sub_logs(
        &self,
        rule_id: Option<&str>,
        page: i64,
        size: i64,
    ) -> Result<(i64, Vec<RuleSubLogRow>)> {
        let conn = self.conn.lock();
        let offset = (page - 1).max(0) * size;
        let (total, sql, rid): (i64, String, Option<String>) = match rule_id {
            Some(rid) => (
                conn.query_row(
                    "SELECT COUNT(*) FROM rule_sub_log WHERE rule_id=?1",
                    params![rid],
                    |r| r.get(0),
                )?,
                "SELECT rule_id, update_time, count, update_type FROM rule_sub_log WHERE rule_id=?1 ORDER BY update_time DESC LIMIT ?2 OFFSET ?3".to_string(),
                Some(rid.to_string()),
            ),
            None => (
                conn.query_row("SELECT COUNT(*) FROM rule_sub_log", [], |r| r.get(0))?,
                "SELECT rule_id, update_time, count, update_type FROM rule_sub_log ORDER BY update_time DESC LIMIT ?2 OFFSET ?3".to_string(),
                None,
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let mapper = |r: &rusqlite::Row| -> rusqlite::Result<RuleSubLogRow> {
            Ok(RuleSubLogRow {
                rule_id: r.get(0)?,
                update_time: r.get(1)?,
                count: r.get(2)?,
                update_type: r.get(3)?,
            })
        };
        let mut out = Vec::new();
        match rid {
            Some(rid) => {
                let mapped = stmt.query_map(params![rid, size, offset], mapper)?;
                for r in mapped {
                    out.push(r?);
                }
            }
            None => {
                let mapped = stmt.query_map(params!["", size, offset], mapper)?;
                for r in mapped {
                    out.push(r?);
                }
            }
        }
        Ok((total, out))
    }

    // ------------------- alerts -------------------

    pub fn insert_alert(
        &self,
        create_at: i64,
        level: &str,
        identifier: &str,
        title_json: &str,
        content_json: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO alert (create_at, read_at, level, identifier, title, content) VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
            params![create_at, level, identifier, title_json, content_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn unread_alert_exists(&self, identifier: &str) -> bool {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM alert WHERE identifier=?1 AND read_at IS NULL",
            params![identifier],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    pub fn alert_exists_include_read(&self, identifier: &str) -> bool {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM alert WHERE identifier=?1",
            params![identifier],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    pub fn get_unread_alerts(&self) -> Result<Vec<AlertRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, create_at, read_at, level, identifier, title, content FROM alert
             WHERE read_at IS NULL ORDER BY create_at DESC",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok(AlertRow {
                id: r.get(0)?,
                create_at: r.get(1)?,
                read_at: r.get(2)?,
                level: r.get(3)?,
                identifier: r.get(4)?,
                title: r.get(5)?,
                content: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn dismiss_alert(&self, id: i64, read_at: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE alert SET read_at=?1 WHERE id=?2 AND read_at IS NULL",
            params![read_at, id],
        )?;
        Ok(n > 0)
    }

    pub fn dismiss_all_alerts(&self, read_at: i64) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "UPDATE alert SET read_at=?1 WHERE read_at IS NULL",
            params![read_at],
        )?)
    }

    pub fn delete_alert(&self, id: i64) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM alert WHERE id=?1", params![id])?)
    }

    pub fn insert_history(&self, h: &HistoryRow) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            r#"INSERT INTO history (ban_at, unban_at, ip, port, peer_id, peer_client_name,
                peer_uploaded, peer_downloaded, peer_progress, downloader_progress, torrent_id,
                module_name, rule_name, description, flags, downloader, structured_data, peer_geoip)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)"#,
            params![
                h.ban_at,
                h.unban_at,
                h.ip,
                h.port,
                h.peer_id,
                h.peer_client_name,
                h.peer_uploaded,
                h.peer_downloaded,
                h.peer_progress,
                h.downloader_progress,
                h.torrent_id,
                h.module_name,
                h.rule_name,
                h.description,
                h.flags,
                h.downloader,
                h.structured_data,
                h.peer_geoip
            ],
        )
        .context("insert history")?;
        Ok(())
    }
}
