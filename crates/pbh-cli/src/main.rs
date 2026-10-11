//! pbh：PeerBanHelper 兼容的 Rust 无头守护进程（实验性重写）。
//!
//! 子命令：
//! - `run`    启动守护进程（BanWave 引擎 + Web API/WebUI）
//! - `doctor` 诊断数据目录、配置、数据库与下载器连通性
//! - `version` 版本信息
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pbh_config::ConfigHandle;
use pbh_core::lang::LangStore;
use pbh_core::rule;
use pbh_db::Database;
use pbh_downloader::DownloaderManager;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "pbh",
    about = "PeerBanHelper-compatible BT peer blocking daemon (Rust rewrite, experimental)",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动守护进程
    Run {
        /// 数据目录（包含 config/、db/、logs/、static/），默认 ./data 或 $PBH_DATA_DIR
        #[arg(long, global = true)]
        data_dir: Option<PathBuf>,
        /// 覆盖 WebUI/API 监听端口
        #[arg(long)]
        port: Option<u16>,
        /// 覆盖监听地址
        #[arg(long)]
        address: Option<String>,
    },
    /// 诊断安装与配置
    Doctor {
        #[arg(long, global = true)]
        data_dir: Option<PathBuf>,
    },
    /// 打印版本信息
    Version,
}

fn resolve_data_dir(arg: Option<PathBuf>) -> PathBuf {
    arg.or_else(|| std::env::var("PBH_DATA_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("data"))
}

/// 将日志事件推入 LogHub（WebUI 实时日志 SSE 的数据源）
struct HubLayer {
    hub: Arc<pbh_web::LogHub>,
}

struct ContentVisitor(String);

impl tracing::field::Visit for ContentVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(&format!("{:?}", value));
        } else {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(value);
        } else {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={}", field.name(), value);
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for HubLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = ContentVisitor(String::new());
        event.record(&mut visitor);
        let meta = event.metadata();
        let thread = std::thread::current()
            .name()
            .unwrap_or("main")
            .to_string();
        self.hub
            .push(meta.level().as_str(), &thread, visitor.0);
    }
}

fn init_logging(
    data_dir: Option<&PathBuf>,
    hub: Arc<pbh_web::LogHub>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,reqwest=warn,rustls=warn"));
    let fmt = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stdout);
    let hub_layer = HubLayer { hub };

    if let Some(dir) = data_dir {
        let log_dir = dir.join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let file_appender = tracing_appender::rolling::daily(&log_dir, "pbh.log");
        let (nb, guard) = tracing_appender::non_blocking(file_appender);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(nb)
            .with_ansi(false);
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt)
            .with(file_layer)
            .with(hub_layer)
            .init();
        return Some(guard);
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(hub_layer)
        .init();
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Version => {
            println!(
                "pbh {} (PeerBanHelper parity target: v9.5.1, BTN protocol: not implemented)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Commands::Doctor { data_dir } => {
            let dir = resolve_data_dir(data_dir);
            let hub = pbh_web::LogHub::new(1024);
            let _guard = init_logging(Some(&dir), hub);
            run_doctor(&dir).await
        }
        Commands::Run {
            data_dir,
            port,
            address,
        } => {
            let dir = resolve_data_dir(data_dir);
            let hub = pbh_web::LogHub::new(1024);
            let _guard = init_logging(Some(&dir), hub.clone());
            run_daemon(&dir, port, address, hub).await
        }
    }
}

async fn run_doctor(dir: &PathBuf) -> Result<()> {
    println!("pbh doctor — data dir: {}", dir.display());

    let handle = ConfigHandle::load_or_init(dir).context("load config")?;
    let cfg = handle.get();
    println!("  [ok] config.yml / profile.yml loaded");
    println!(
        "  [..] server: {}:{} token={}",
        cfg.server_address(),
        cfg.server_http_port(),
        if cfg.token().is_empty() {
            "<empty, OOBE required>"
        } else {
            "<set>"
        }
    );
    println!(
        "  [..] check-interval={}ms ban-duration={}ms persist-banlist={}",
        cfg.check_interval_ms(),
        cfg.ban_duration_ms(),
        cfg.persist_banlist()
    );

    let db = Database::open(cfg.db_path()).context("open database")?;
    let banlist_rows = db.read_banlist().map(|v| v.len()).unwrap_or(0);
    println!("  [ok] database {} ({} banlist rows)", cfg.db_path().display(), banlist_rows);

    let downloaders = DownloaderManager::from_config(&handle).context("downloaders")?;
    if downloaders.is_empty() {
        println!("  [!!] config.yml 中没有任何下载器（client 节为空）。");
        println!("       可通过 WebUI OOBE（http://<host>:9898/init）或手工编辑 config.yml 添加。");
    }
    for d in downloaders.iter() {
        let r = d.login().await;
        println!(
            "  [{}] downloader {} ({}) @ {} — login: {:?}",
            if r.is_success() { "ok" } else { "!!" },
            d.name(),
            d.downloader_type(),
            d.endpoint(),
            r.status
        );
    }

    let static_dir = cfg.static_dir();
    if static_dir.join("index.html").exists() {
        println!("  [ok] WebUI static files found at {}", static_dir.display());
    } else {
        println!(
            "  [!!] WebUI 静态文件缺失（{}）。将 webui 构建产物复制到该目录，或使用 --static-dir。",
            static_dir.join("index.html").display()
        );
    }
    Ok(())
}

async fn run_daemon(
    dir: &PathBuf,
    port: Option<u16>,
    address: Option<String>,
    log_hub: Arc<pbh_web::LogHub>,
) -> Result<()> {
    let handle = ConfigHandle::load_or_init(dir).context("load config")?;
    let cfg = handle.get();
    let locale = cfg.language();
    let lang = Arc::new(LangStore::embedded(locale.clone()));
    // 规则引擎构造期注释需要 LangStore（对应 Java Main.DEF_LOCALE 行为）
    rule::init_lang(LangStore::embedded(locale.clone()));

    println_banner();

    let db = Arc::new(Database::open(cfg.db_path()).context("open database")?);
    let downloaders = DownloaderManager::from_config(&handle).context("downloaders")?;

    // GeoIP（对应上游 IPDBManager：启动加载 + auto-update 后台任务）
    let geoip = {
        let ipdb_dir = handle.data_dir.join("ipdb");
        let gdb = pbh_geoip::IpDb::new(
            ipdb_dir,
            cfg.main.get_str("ip-database.database-city", "GeoLite2-City"),
            cfg.main.get_str("ip-database.database-asn", "GeoLite2-ASN"),
            cfg.main.get_str("ip-database.database-geocn", "GeoCN"),
            cfg.main.get_bool("ip-database.auto-update", true),
        );
        gdb.spawn_updater();
        Some(gdb)
    };

    let banlist: pbh_core::banlist::BanListHandle =
        Arc::new(parking_lot::RwLock::new(pbh_core::banlist::BanList::new()));
    let built =
        pbh_modules::build_modules(&cfg, &lang, db.clone(), banlist.clone(), &handle, geoip.clone());
    tracing::info!(
        "Registered {} rule modules: [{}]",
        built.list.len(),
        built.list
            .iter()
            .map(|m| m.name())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let push = pbh_push::PushManager::from_config(&cfg);
    let token = Arc::new(parking_lot::RwLock::new(cfg.token()));
    let engine = pbh_engine::Engine::new(pbh_engine::EngineBuilder {
        cfg: handle.clone(),
        lang: lang.clone(),
        db: db.clone(),
        downloaders: downloaders.clone(),
        built,
        banlist,
        geoip,
        push,
    });
    engine.load_banlist_from_db();

    if cfg.token().is_empty() {
        tracing::info!("server.token 为空：首次启动，请通过 WebUI OOBE 向导（/init）完成初始化");
    }

    let addr: std::net::SocketAddr = format!(
        "{}:{}",
        address.unwrap_or_else(|| cfg.server_address()),
        port.unwrap_or_else(|| cfg.server_http_port())
    )
    .parse()
    .context("invalid listen address")?;

    let web_state = pbh_web::AppState {
        engine: engine.clone(),
        cfg: handle.clone(),
        lang: lang.clone(),
        token: token.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        // PBH_STATIC_DIR 可覆盖 WebUI 静态资源目录（容器只读镜像场景）
        static_dir: std::env::var("PBH_STATIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| cfg.static_dir()),
        log_hub,
    };

    let web_handle = tokio::spawn(async move {
        if let Err(e) = pbh_web::serve(web_state, addr).await {
            tracing::error!("Web server crashed: {}", e);
        }
    });

    let loop_engine = engine.clone();
    let loop_handle = tokio::spawn(async move {
        loop_engine.run_loop().await;
    });

    tracing::info!(
        "PeerBanHelper-RS started. WebUI: http://{} (data dir: {})",
        addr,
        dir.display()
    );

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("install SIGTERM handler")?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutdown signal (SIGINT) received");
            }
            _ = sigterm.recv() => {
                tracing::info!("Shutdown signal (SIGTERM) received");
            }
            _ = web_handle => {
                tracing::error!("Web server exited unexpectedly, shutting down");
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutdown signal received");
            }
            _ = web_handle => {
                tracing::error!("Web server exited unexpectedly, shutting down");
            }
        }
    }

    engine.request_shutdown();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), loop_handle).await;
    if let Err(e) = engine.save_banlist() {
        tracing::error!("Failed to save banlist on shutdown: {}", e);
    }
    tracing::info!("Goodbye.");
    Ok(())
}

fn println_banner() {
    println!(
        r#"
  ____       _           ____        _   _      _   _      _        _
 | __ )  ___| |__   ___ | __ ) _   _| |_| |__  (_) (_) ___| |_ __ _| |
 |  _ \ / _ \ '_ \ / __||  _ \| | | | __| '_ \ | | | |/ __| __/ _` | |
 | |_) |  __/ | | | (__ | |_) | |_| | |_| | | || |_| |\__ \ || (_| | |
 |____/ \___|_| |_|\___||____/ \__, |\__|_| |_|_(_) |_|___/\__\__,_|_|
                                |___/
        Rust rewrite (experimental) · behavior-compatible with PeerBanHelper v9.5.1
        Licensed under GPL-3.0 · https://github.com/PBH-BTN/PeerBanHelper
"#
    );
}
