mod db;
mod downloader;
mod thumb;
mod chpack;
mod slug;
mod settings;
mod state;
mod routes;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use axum::{Router, routing::{delete, get, patch, post, put}};
use clap::Parser;
use tower_http::{compression::CompressionLayer, services::ServeDir};
use tracing::info;

static DOCS_TEXT: &str = include_str!("../DOCS.txt");

#[derive(Parser)]
#[command(name = "curator", about = "Curator — self-hosted gallery-dl front-end")]
struct Cli {
    /// Print full reference documentation and exit
    #[arg(long)]
    docs: bool,

    /// Port to listen on
    #[arg(long, default_value = "8642")]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.docs {
        println!("{DOCS_TEXT}");
        return Ok(());
    }

    // Data dir resolution: CURATOR_DATA_DIR env → config.json data_dir → ~/Curator
    let data_dir = resolve_data_dir();
    std::fs::create_dir_all(&data_dir)?;

    // Logging — file + stderr
    let log_path = data_dir.join("curator.log");
    init_logging(&log_path)?;

    info!("Curator starting — data dir: {}", data_dir.display());

    let pool = db::init_pool(&data_dir)?;
    let app_state = Arc::new(state::AppState::new(pool, data_dir.clone())?);

    let static_dir = PathBuf::from("static");
    let library_dir = data_dir.join("library");

    let app = Router::new()
        // sources
        .route("/api/sources", get(routes::sources::list))
        .route("/api/sources", post(routes::sources::add))
        .route("/api/sources/resync-all", post(routes::sources::resync_all))
        .route("/api/sources/:id", get(routes::sources::get_one))
        .route("/api/sources/:id", patch(routes::sources::patch))
        .route("/api/sources/:id", delete(routes::sources::delete))
        .route("/api/sources/:id/resync", post(routes::sources::resync))
        .route("/api/sources/:id/group", patch(routes::sources::set_group))
        .route("/api/sources/:id/log", get(routes::sources::get_log))
        // groups
        .route("/api/groups", get(routes::groups::list))
        .route("/api/groups", post(routes::groups::create))
        .route("/api/groups/:id", patch(routes::groups::update))
        .route("/api/groups/:id", delete(routes::groups::delete))
        .route("/api/groups/:id/tags", post(routes::groups::add_tag))
        .route("/api/groups/:id/tags/:tag_id", delete(routes::groups::remove_tag))
        // media
        .route("/api/media", get(routes::media::list))
        .route("/api/media/:id/rating", put(routes::media::set_rating))
        .route("/api/media/:id/tags", post(routes::media::add_tag))
        .route("/api/media/:id/tags/:tag_id", delete(routes::media::remove_tag))
        // tags
        .route("/api/tags", get(routes::tags::list))
        .route("/api/tags/:id", delete(routes::tags::delete))
        // thumb
        .route("/api/thumb/:id", get(routes::thumb::get))
        // settings
        .route("/api/settings", get(routes::settings::get))
        .route("/api/settings", patch(routes::settings::patch))
        // stats + log
        .route("/api/stats", get(routes::misc::stats))
        .route("/api/log", get(routes::misc::log))
        // downloads
        .route("/api/downloads/status", get(routes::downloads::status))
        .route("/api/downloads/pause", post(routes::downloads::pause))
        .route("/api/downloads/resume", post(routes::downloads::resume))
        // export / import
        .route("/api/export", get(routes::export::export_sources))
        .route("/api/import", post(routes::export::import_sources))
        .route("/api/export/chpack", post(routes::export::export_chpack))
        // preview (live browse)
        .route("/api/preview/scan", get(routes::preview::scan))
        .route("/api/preview/search", post(routes::preview::search))
        // static file serving
        .nest_service(
            "/library",
            tower_http::services::ServeDir::new(&library_dir)
                .append_index_html_on_directories(false),
        )
        .nest_service(
            "/",
            ServeDir::new(&static_dir).append_index_html_on_directories(true),
        )
        .layer(CompressionLayer::new())
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    info!("Listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn resolve_data_dir() -> PathBuf {
    if let Ok(v) = std::env::var("CURATOR_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Ok(raw) = std::fs::read_to_string("config.json") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(s) = v["data_dir"].as_str() {
                return PathBuf::from(s);
            }
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Curator")
}

fn init_logging(log_path: &PathBuf) -> anyhow::Result<()> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("curator=info,tower_http=warn"));

    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(true);

    // File layer — append mode
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let file_layer = fmt::layer().with_writer(file).with_ansi(false);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Ok(())
}
