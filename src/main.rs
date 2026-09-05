mod chpack;
mod config;
mod db;
mod downloader;
mod duration;
mod media_files;
mod nsfw;
mod oobe;
mod process;
mod routes;
mod slug;
#[cfg(test)]
mod test_support;
mod thumb_worker;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc};

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

pub type GroupTagCache = Arc<RwLock<Option<Arc<HashMap<i64, HashSet<String>>>>>>;

/// Shared application state passed to every Axum route handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: Pool<SqliteConnectionManager>,
    /// Cached group-id → effective tag set. None = dirty, rebuild on next read.
    pub group_tag_cache: GroupTagCache,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub download_tasks: tokio_util::task::TaskTracker,
    pub source_cancellations: Arc<Mutex<HashMap<i64, tokio_util::sync::CancellationToken>>>,
    pub running_sources: Arc<Mutex<HashSet<i64>>>,
    pub downloads_paused: Arc<AtomicBool>,
    /// source_id → PID of the running gallery-dl process.
    pub active_processes: Arc<Mutex<HashMap<i64, u32>>>,
    pub paused_source_ids: Arc<Mutex<HashSet<i64>>>,
    /// Swapped out when max_concurrent changes (same semantics as Python's approach).
    pub download_semaphore: Arc<Mutex<Arc<Semaphore>>>,
    /// Limits concurrent populate_placeholder scans to 3, independent of real downloads.
    pub placeholder_semaphore: Arc<Semaphore>,
    pub settings: Arc<RwLock<db::Settings>>,
    pub data_dir: PathBuf,
    pub library_dir: PathBuf,
    pub archives_dir: PathBuf,
    pub thumbs_dir: PathBuf,
    pub log_path: PathBuf,
    /// Directory `static/` assets (index.html, oobe.html, app.js, ...) are
    /// served from — kept on state (rather than only a local in `main`) so
    /// the root-gate handler in `routes::oobe::serve_root` can pick between
    /// `index.html` and `oobe.html` without extra plumbing.
    pub static_dir: PathBuf,
    pub gallery_dl_bin: String,
    pub ffprobe_bin: String,
    /// Only otherwise used to spawn the NSFW worker at startup (see
    /// `nsfw::NsfwClassifier::spawn`) — kept on `AppState` as well so the
    /// OOBE dependency check can probe the *currently configured*
    /// interpreter on demand without re-deriving it from `config.json`.
    pub python_bin: String,
    /// None if NSFW auto-rating is off or its worker never started.
    pub nsfw: Option<nsfw::NsfwClassifier>,
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
                .args([
                    "/C",
                    "start",
                    "",
                    browser,
                    &format!("--app={url}"),
                    "--window-size=1280,860",
                ])
                .spawn();
            if spawned.is_ok() {
                return;
            }
        }
        // Neither Edge nor Chrome found by name — fall back to whatever
        // the default browser is (a normal tab, not a standalone window).
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        for app in [
            "/Applications/Google Chrome.app",
            "/Applications/Microsoft Edge.app",
        ] {
            let spawned = std::process::Command::new("open")
                .args(["-na", app, "--args", &format!("--app={url}")])
                .spawn();
            if spawned.is_ok() {
                return;
            }
        }
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for browser in [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ] {
            let spawned = std::process::Command::new(browser)
                .arg(format!("--app={url}"))
                .spawn();
            if spawned.is_ok() {
                return;
            }
        }
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
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

fn setup_logging(log_path: &std::path::Path) {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let file_appender = tracing_appender::rolling::never(
        log_path.parent().unwrap_or(std::path::Path::new(".")),
        log_path
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("curator.log")),
    );
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // Keep _guard alive for the process lifetime
    std::mem::forget(_guard);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

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

    let cfg = config::load_config();
    let data_dir = config::resolve_data_dir(&cfg);
    let library_dir = data_dir.join("library");
    let archives_dir = data_dir.join("archives");
    let thumbs_dir = data_dir.join("thumbnails");
    let log_path = data_dir.join("curator.log");

    // Ensure directories exist
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(&library_dir)?;
    std::fs::create_dir_all(&archives_dir)?;
    std::fs::create_dir_all(&thumbs_dir)?;

    setup_logging(&log_path);

    // Persist the resolved data_dir so future runs find the same place
    config::ensure_config_json(&data_dir);

    // gallery-dl binary (PATH default or config override)
    let gallery_dl_bin = cfg
        .gallery_dl_bin
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gallery-dl".to_string());

    let python_bin = cfg.python_bin.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        if cfg!(windows) {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    });

    let ffprobe_bin = cfg
        .ffprobe_bin
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ffprobe".to_string());

    // Database pool + migrations
    let pool = db::init_pool(&data_dir).map_err(|e| {
        error!(
            "FATAL: could not set up database at {:?}: {}",
            data_dir.join("data.db"),
            e
        );
        e
    })?;

    // Settings (loaded from settings.json with DEFAULT_SETTINGS fallback)
    let mut settings = db::load_settings(&data_dir);
    let max_concurrent = settings.max_concurrent as usize;

    // Second half of the OOBE self-heal (the first half lives in
    // db::load_settings, for installations that already had a
    // settings.json): a real, pre-OOBE installation whose settings.json
    // happened to never be written at all (every Settings field has a
    // default, so plenty of installs never trigger a save) would otherwise
    // still look "fresh" and get shown the first-run wizard. If the
    // database already has real sources/groups/media, treat setup as
    // already complete instead.
    if !settings.oobe_completed {
        if let Ok(conn) = pool.get() {
            if oobe::existing_installation_has_data(&conn) {
                settings.oobe_completed = true;
                db::save_settings(&data_dir, &settings);
            }
        }
    }

    let reconcile_pool = pool.clone();
    let reconcile_library = library_dir.clone();
    let missing = tokio::task::spawn_blocking(move || {
        media_files::reconcile(&reconcile_pool, &reconcile_library)
    })
    .await??;
    if missing > 0 {
        info!("Reconciled {missing} missing media files; annotations retained");
    }
    // A crashed downloader is resumable, not still running on the next startup.
    pool.get()?.execute(
        "UPDATE sources SET status='paused' WHERE status='downloading'",
        [],
    )?;

    // NSFW auto-rating (opt-in — see nsfw.rs). Always refresh the
    // embedded worker script on disk so it matches this build, even if the
    // feature is currently off; that way turning it on later doesn't need
    // a fresh copy of the exe.
    let nsfw_worker_path = data_dir.join("nsfw_worker.py");
    if let Err(e) = std::fs::write(&nsfw_worker_path, NSFW_WORKER_PY) {
        warn!(
            "Could not write nsfw_worker.py to {:?}: {}",
            nsfw_worker_path, e
        );
    }
    let nsfw_classifier = if settings.nsfw_filter_enabled {
        info!("NSFW auto-rating enabled — starting classifier worker");
        Some(nsfw::NsfwClassifier::spawn(
            python_bin.clone(),
            nsfw_worker_path,
        ))
    } else {
        None
    };
    if let Some(ref classifier) = nsfw_classifier {
        nsfw::spawn_backfill_loop(pool.clone(), classifier.clone(), library_dir.clone());
    }

    // Video duration backfill (for the clips/videos split — see
    // duration.rs). Checked once, here, rather than letting the loop
    // discover ffprobe is missing on every single video.
    if tokio::task::spawn_blocking({
        let bin = ffprobe_bin.clone();
        move || duration::ffprobe_available(&bin)
    })
    .await?
    {
        duration::spawn_backfill_loop(pool.clone(), ffprobe_bin.clone(), library_dir.clone());
    } else {
        info!("ffprobe not found (\"{}\") — video duration (clips/videos split) won't be backfilled for existing videos; newly-downloaded ones are unaffected once ffprobe is available", ffprobe_bin);
    }

    // Static directories
    let static_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("static")))
        .unwrap_or_else(|| PathBuf::from("static"));

    let state = AppState {
        pool,
        group_tag_cache: Arc::new(RwLock::new(None)),
        shutdown: tokio_util::sync::CancellationToken::new(),
        download_tasks: tokio_util::task::TaskTracker::new(),
        source_cancellations: Arc::new(Mutex::new(HashMap::new())),
        running_sources: Arc::new(Mutex::new(HashSet::new())),
        downloads_paused: Arc::new(AtomicBool::new(false)),
        active_processes: Arc::new(Mutex::new(HashMap::new())),
        paused_source_ids: Arc::new(Mutex::new(HashSet::new())),
        download_semaphore: Arc::new(Mutex::new(Arc::new(Semaphore::new(max_concurrent)))),
        placeholder_semaphore: Arc::new(Semaphore::new(3)),
        settings: Arc::new(RwLock::new(settings)),
        data_dir: data_dir.clone(),
        library_dir: library_dir.clone(),
        archives_dir,
        thumbs_dir,
        log_path,
        static_dir: static_dir.clone(),
        gallery_dl_bin,
        ffprobe_bin,
        python_bin,
        nsfw: nsfw_classifier,
    };

    // Recover completed files whose final event was lost before a crash.
    let startup_state = state.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let rows = {
            let conn = startup_state.pool.get()?;
            let mut stmt = conn.prepare("SELECT id,slug FROM sources")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (id, slug) in rows {
            let dest = startup_state.library_dir.join(slug);
            if dest.is_dir() {
                if let Err(e) = downloader::scan_and_index(&startup_state, id, &dest) {
                    warn!("Startup index recovery for source {id}: {e}");
                }
            }
        }
        Ok(())
    })
    .await??;

    let app = routes::build_router(Arc::new(state.clone()))
        .layer(CompressionLayer::new())
        .nest(
            "/library",
            axum::Router::new()
                .fallback_service(ServeDir::new(&library_dir))
                .layer(axum::middleware::from_fn_with_state(
                    Arc::new(state.clone()),
                    routes::library::reconcile_not_found,
                )),
        )
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

    let shutdown_state = state.clone();
    let recovery_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = recovery_state.shutdown.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {}
            }
            let s = recovery_state.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || media_files::reconcile(&s.pool, &s.library_dir))
                    .await
                    .unwrap_or_else(|e| Err(e.into()))
            {
                warn!("Filesystem reconciliation failed: {e}");
            }
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown_state.shutdown.cancel();
            shutdown_state.download_tasks.close();
            shutdown_state.download_tasks.wait().await;
        })
        .await?;
    Ok(())
}
