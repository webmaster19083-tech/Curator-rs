mod db;
mod slug;
mod downloader;
mod thumb_worker;
mod chpack;
mod nsfw;
mod duration;
mod routes;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    atomic::AtomicBool,
    Arc,
};

use anyhow::Result;
use clap::Parser;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

pub static DOCS_TEXT: &str = include_str!("../DOCS.txt");
pub static NSFW_WORKER_PY: &str = include_str!("../nsfw_worker.py");

/// Shared application state passed to every Axum route handler.
#[derive(Clone)]
pub struct AppState {
    pub pool:                 Pool<SqliteConnectionManager>,
    /// Cached group-id → effective tag set. None = dirty, rebuild on next read.
    pub group_tag_cache:      Arc<RwLock<Option<HashMap<i64, HashSet<String>>>>>,
    pub downloads_paused:     Arc<AtomicBool>,
    /// source_id → PID of the running gallery-dl process.
    pub active_processes:     Arc<Mutex<HashMap<i64, u32>>>,
    pub paused_source_ids:    Arc<Mutex<HashSet<i64>>>,
    /// Swapped out when max_concurrent changes (same semantics as Python's approach).
    pub download_semaphore:   Arc<Mutex<Arc<Semaphore>>>,
    /// Limits concurrent populate_placeholder scans to 3, independent of real downloads.
    pub placeholder_semaphore: Arc<Semaphore>,
    pub settings:             Arc<RwLock<db::Settings>>,
    pub data_dir:             PathBuf,
    pub library_dir:          PathBuf,
    pub archives_dir:         PathBuf,
    pub thumbs_dir:           PathBuf,
    pub log_path:             PathBuf,
    pub gallery_dl_bin:       String,
    pub ffprobe_bin:          String,
    /// None if NSFW auto-rating is off or its worker never started.
    pub nsfw:                 Option<nsfw::NsfwClassifier>,
}

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "curator",
    about = "Curator — a self-hosted, site-agnostic front-end for gallery-dl.",
    long_about = None,
)]
struct Cli {
    /// Print the full reference documentation (setup, remote access,
    /// groups & tags, troubleshooting) and exit.
    #[arg(long)]
    docs: bool,

    /// Don't auto-open a window on startup — just run the server. Use this
    /// for headless/server setups (e.g. accessed remotely, or run as a
    /// service) where no local browser/window makes sense.
    #[arg(long)]
    no_window: bool,
}

// ─── App window ────────────────────────────────────────────────────────────

/// Opens `url` in a borderless "app mode" browser window (no tabs/toolbar —
/// looks like a native window) instead of making the person open a browser
/// tab themselves. Falls back to the default browser if no Chromium-based
/// browser is found.
fn open_app_window(url: &str) {
    #[cfg(target_os = "windows")]
    {
        for browser in ["msedge", "chrome"] {
            let spawned = std::process::Command::new("cmd")
                .args(["/C", "start", "", browser,
                       &format!("--app={url}"), "--window-size=1280,860"])
                .spawn();
            if spawned.is_ok() { return; }
        }
        // Neither Edge nor Chrome found by name — fall back to whatever
        // the default browser is (a normal tab, not a standalone window).
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        for app in ["/Applications/Google Chrome.app", "/Applications/Microsoft Edge.app"] {
            let spawned = std::process::Command::new("open")
                .args(["-na", app, "--args", &format!("--app={url}")])
                .spawn();
            if spawned.is_ok() { return; }
        }
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for browser in ["google-chrome", "chromium", "chromium-browser", "microsoft-edge"] {
            let spawned = std::process::Command::new(browser)
                .arg(format!("--app={url}"))
                .spawn();
            if spawned.is_ok() { return; }
        }
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

// ─── Config loading ───────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct Config {
    data_dir:       Option<String>,
    gallery_dl_bin: Option<String>,
    python_bin:     Option<String>,
    ffprobe_bin:    Option<String>,
}

fn load_config() -> Config {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.json")))
        .unwrap_or_else(|| PathBuf::from("config.json"));

    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&text) {
            return cfg;
        }
    }
    Config::default()
}

fn resolve_data_dir(cfg: &Config) -> PathBuf {
    // 1. Environment variable
    if let Ok(env_val) = std::env::var("CURATOR_DATA_DIR") {
        if !env_val.is_empty() {
            return PathBuf::from(env_val);
        }
    }
    // 2. config.json data_dir
    if let Some(ref configured) = cfg.data_dir {
        if !configured.is_empty() {
            return PathBuf::from(configured);
        }
    }
    // 3. ~/Curator default
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Curator")
}

fn ensure_config_json(data_dir: &PathBuf) {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.json")))
        .unwrap_or_else(|| PathBuf::from("config.json"));

    if !path.exists() {
        let content = serde_json::json!({ "data_dir": data_dir.to_string_lossy() });
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&content).unwrap());
    }
}

// ─── Network helper ───────────────────────────────────────────────────────────

fn list_reachable_addresses() -> Vec<String> {
    let mut addrs = vec!["127.0.0.1".to_string()];
    // Best-effort: get non-loopback IPs via a UDP connect trick
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        let _ = socket.connect("8.8.8.8:80");
        if let Ok(local) = socket.local_addr() {
            let ip = local.ip().to_string();
            if ip != "127.0.0.1" {
                addrs.push(ip);
            }
        }
    }
    addrs
}

// ─── Logging setup ────────────────────────────────────────────────────────────

fn setup_logging(log_path: &PathBuf) {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let file_appender = tracing_appender::rolling::never(
        log_path.parent().unwrap_or(std::path::Path::new(".")),
        log_path.file_name().unwrap_or(std::ffi::OsStr::new("curator.log")),
    );
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // Keep _guard alive for the process lifetime
    std::mem::forget(_guard);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();
}

// ─── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // --docs exits before touching anything
    if cli.docs {
        print!("{}", DOCS_TEXT);
        return Ok(());
    }

    let cfg = load_config();
    let data_dir   = resolve_data_dir(&cfg);
    let library_dir = data_dir.join("library");
    let archives_dir = data_dir.join("archives");
    let thumbs_dir  = data_dir.join("thumbnails");
    let log_path    = data_dir.join("curator.log");

    // Ensure directories exist
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(&library_dir)?;
    std::fs::create_dir_all(&archives_dir)?;
    std::fs::create_dir_all(&thumbs_dir)?;

    setup_logging(&log_path);

    // Persist the resolved data_dir so future runs find the same place
    ensure_config_json(&data_dir);

    // gallery-dl binary (PATH default or config override)
    let gallery_dl_bin = cfg.gallery_dl_bin
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gallery-dl".to_string());

    let python_bin = cfg.python_bin
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) { "python".to_string() } else { "python3".to_string() }
        });

    let ffprobe_bin = cfg.ffprobe_bin
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ffprobe".to_string());

    // Database pool + migrations
    let pool = db::init_pool(&data_dir)
        .map_err(|e| { error!("FATAL: could not set up database at {:?}: {}", data_dir.join("data.db"), e); e })?;

    // Settings (loaded from settings.json with DEFAULT_SETTINGS fallback)
    let settings = db::load_settings(&data_dir);
    let max_concurrent = settings.max_concurrent as usize;

    // NSFW auto-rating (opt-in — see nsfw.rs). Always refresh the
    // embedded worker script on disk so it matches this build, even if the
    // feature is currently off; that way turning it on later doesn't need
    // a fresh copy of the exe.
    let nsfw_worker_path = data_dir.join("nsfw_worker.py");
    if let Err(e) = std::fs::write(&nsfw_worker_path, NSFW_WORKER_PY) {
        warn!("Could not write nsfw_worker.py to {:?}: {}", nsfw_worker_path, e);
    }
    let nsfw_classifier = if settings.nsfw_filter_enabled {
        info!("NSFW auto-rating enabled — starting classifier worker");
        Some(nsfw::NsfwClassifier::spawn(python_bin, nsfw_worker_path))
    } else {
        None
    };
    if let Some(ref classifier) = nsfw_classifier {
        nsfw::spawn_backfill_loop(pool.clone(), classifier.clone(), library_dir.clone());
    }

    // Video duration backfill (for the clips/videos split — see
    // duration.rs). Checked once, here, rather than letting the loop
    // discover ffprobe is missing on every single video.
    if duration::ffprobe_available(&ffprobe_bin) {
        duration::spawn_backfill_loop(pool.clone(), ffprobe_bin.clone(), library_dir.clone());
    } else {
        info!("ffprobe not found (\"{}\") — video duration (clips/videos split) won't be backfilled for existing videos; newly-downloaded ones are unaffected once ffprobe is available", ffprobe_bin);
    }

    let state = AppState {
        pool,
        group_tag_cache:       Arc::new(RwLock::new(None)),
        downloads_paused:      Arc::new(AtomicBool::new(false)),
        active_processes:      Arc::new(Mutex::new(HashMap::new())),
        paused_source_ids:     Arc::new(Mutex::new(HashSet::new())),
        download_semaphore:    Arc::new(Mutex::new(Arc::new(Semaphore::new(max_concurrent)))),
        placeholder_semaphore: Arc::new(Semaphore::new(3)),
        settings:              Arc::new(RwLock::new(settings)),
        data_dir:              data_dir.clone(),
        library_dir:           library_dir.clone(),
        archives_dir,
        thumbs_dir,
        log_path,
        gallery_dl_bin,
        ffprobe_bin,
        nsfw:                  nsfw_classifier,
    };

    // Static directories
    let static_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("static")))
        .unwrap_or_else(|| PathBuf::from("static"));

    let app = routes::build_router(state.clone())
        .layer(CompressionLayer::new())
        .nest_service("/library", ServeDir::new(&library_dir))
        .fallback_service(ServeDir::new(&static_dir).append_index_html_on_directories(true));

    let addr: SocketAddr = "0.0.0.0:8642".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let addresses = list_reachable_addresses();
    info!("Curator is running:");
    for ip in &addresses {
        info!("  http://{}:8642", ip);
    }
    info!("Data directory: {:?}", data_dir);
    info!("Press Ctrl+C to stop.");

    if cli.no_window {
        info!("--no-window set: open one of the addresses above in a browser.");
    } else {
        info!("Opening Curator's window...");
        tokio::spawn(async {
            // Small delay so the window doesn't race the very first accept().
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            open_app_window("http://127.0.0.1:8642");
        });
    }

    axum::serve(listener, app).await?;
    Ok(())
}
