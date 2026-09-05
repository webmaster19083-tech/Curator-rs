pub mod media;
pub mod sources;
pub mod groups;
pub mod tags;
pub mod thumb;
pub mod library;
pub mod export;
pub mod settings;
pub mod downloads;
pub mod misc;
pub mod ch;
pub mod oobe;

use std::sync::Arc;
use axum::{
    routing::{delete, get, patch, post, put},
    Router,
};
use crate::AppState;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // ── First-run OOBE ─────────────────────────────────────────────────
        // Explicit routes on "/" and "/index.html" take priority over the
        // static-file fallback_service registered in main.rs, so a
        // not-yet-configured install is handed oobe.html instead of the
        // normal app shell without needing any change to app.js's own
        // startup sequence.
        .route("/",                                     get(oobe::serve_root))
        .route("/index.html",                            get(oobe::serve_root))
        .route("/api/oobe/status",                       get(oobe::status))
        .route("/api/oobe/validate",                     post(oobe::validate))
        .route("/api/oobe/settings",                     post(oobe::save_settings))
        .route("/api/oobe/complete",                     post(oobe::complete))
        .route("/api/oobe/reset",                        post(oobe::reset))
        // ── Media ──────────────────────────────────────────────────────────
        .route("/api/media",                            get(media::list))
        .route("/api/media/:id/rating",                 put(media::set_rating))
        .route("/api/media/:id/tags",                   post(media::add_tag))
        .route("/api/media/:id/tags/:tag_id",           delete(media::remove_tag))
        // ── Thumbnails ─────────────────────────────────────────────────────
        .route("/api/thumb/:id",                        get(thumb::get_thumbnail))
        // ── Tags ───────────────────────────────────────────────────────────
        .route("/api/tags",                             get(tags::list))
        .route("/api/tags/:id",                         delete(tags::delete_tag))
        // ── Sources ────────────────────────────────────────────────────────
        .route("/api/sources",                          get(sources::list).post(sources::add))
        .route("/api/sources/resync-all",               post(sources::resync_all))
        .route("/api/sources/:id",                      get(sources::get).patch(sources::patch).delete(sources::delete))
        .route("/api/sources/:id/group",                patch(sources::set_group))
        .route("/api/sources/:id/resync",               post(sources::resync))
        .route("/api/sources/:id/log",                  get(misc::source_log))
        // ── Groups ─────────────────────────────────────────────────────────
        .route("/api/groups",                           get(groups::list).post(groups::create))
        .route("/api/groups/:id",                       patch(groups::update).delete(groups::delete))
        .route("/api/groups/:id/tags",                  post(groups::add_tag))
        .route("/api/groups/:id/tags/:tag_id",          delete(groups::remove_tag))
        // ── Downloads ──────────────────────────────────────────────────────
        .route("/api/downloads/status",                 get(downloads::status))
        .route("/api/downloads/pause",                  post(downloads::pause))
        .route("/api/downloads/resume",                 post(downloads::resume))
        // ── Settings ───────────────────────────────────────────────────────
        .route("/api/settings",                         get(settings::get).patch(settings::patch))
        // ── Export / Import ────────────────────────────────────────────────
        .route("/api/export",                           get(export::export_sources))
        .route("/api/export/chpack",                    post(export::export_chpack))
        .route("/api/import",                           post(export::import_sources))
        // ── Stats / Log ────────────────────────────────────────────────────
        .route("/api/stats",                            get(misc::stats))
        .route("/api/log",                              get(misc::get_log))
        // ── Cock Hero (Tier 3) ─────────────────────────────────────────────
        .route("/api/ch/playlist",                      get(ch::get_playlist))
        .route("/api/ch/session",                       post(ch::log_session))
        .route("/api/ch/sessions",                      get(ch::get_sessions))
        .with_state(state)
}
