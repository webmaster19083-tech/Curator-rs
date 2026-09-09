pub mod clips;
pub mod downloads;
pub mod export;
pub mod goon;
pub mod groups;
pub mod library;
pub mod media;
pub mod misc;
pub mod oobe;
pub mod remote;
pub mod search;
pub mod settings;
pub mod source_tags;
pub mod sources;
pub mod tags;
pub mod thumb;

use crate::AppState;
use axum::{
    routing::{delete, get, patch, post, put},
    Router,
};
use std::sync::Arc;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/library/summary", get(crate::hierarchy::endpoint))
        // ── First-run OOBE ─────────────────────────────────────────────────
        // Explicit routes on "/" and "/index.html" take priority over the
        // static-file fallback_service registered in main.rs, so a
        // not-yet-configured install is handed oobe.html instead of the
        // normal app shell without needing any change to app.js's own
        // startup sequence.
        .route("/", get(oobe::serve_root))
        .route("/index.html", get(oobe::serve_root))
        .route("/api/oobe/status", get(oobe::status))
        .route("/api/oobe/validate", post(oobe::validate))
        .route("/api/oobe/settings", post(oobe::save_settings))
        .route("/api/oobe/complete", post(oobe::complete))
        .route("/api/oobe/reset", post(oobe::reset))
        // ── Media ──────────────────────────────────────────────────────────
        .route("/api/media", get(media::list))
        .route("/api/media/bulk", post(media::bulk))
        .route("/api/media/:id/clips", post(clips::create))
        .route("/api/clip-jobs/:id", get(clips::status))
        .route("/api/media/:id/rating", put(media::set_rating))
        .route("/api/media/:id/rating/approve", post(media::approve_rating))
        .route("/api/media/:id/rating/undo", post(media::undo_rating))
        .route("/api/media/:id/duration", put(media::set_duration))
        .route("/api/media/:id/tags", post(media::add_tag))
        .route("/api/media/:id/tags/:tag_id", delete(media::remove_tag))
        // ── Thumbnails ─────────────────────────────────────────────────────
        .route("/api/thumb/:id", get(thumb::get_thumbnail))
        // ── Tags ───────────────────────────────────────────────────────────
        .route("/api/tags", get(tags::list))
        .route("/api/tags/quick", get(tags::quick))
        .route("/api/tags/:id", delete(tags::delete_tag))
        .route(
            "/api/source-tags/review",
            get(source_tags::review_list).post(source_tags::review),
        )
        .route(
            "/api/source-tag-rules",
            get(source_tags::list_rules).post(source_tags::save_rule),
        )
        .route(
            "/api/source-tag-rules/:id",
            delete(source_tags::delete_rule),
        )
        // ── Sources ────────────────────────────────────────────────────────
        .route("/api/sources", get(sources::list).post(sources::add))
        .route("/api/sources/resync-all", post(sources::resync_all))
        .route(
            "/api/sources/:id",
            get(sources::get)
                .patch(sources::patch)
                .delete(sources::delete),
        )
        .route("/api/sources/:id/group", patch(sources::set_group))
        .route("/api/sources/:id/resync", post(sources::resync))
        .route("/api/sources/:id/log", get(misc::source_log))
        // ── Unified discovery ───────────────────────────────────────────────
        .route("/api/search", get(search::search))
        .route("/api/search/download", post(search::download_selected))
        // ── Groups ─────────────────────────────────────────────────────────
        .route("/api/groups", get(groups::list).post(groups::create))
        .route(
            "/api/groups/:id",
            patch(groups::update).delete(groups::delete),
        )
        .route("/api/groups/:id/tags", post(groups::add_tag))
        .route("/api/groups/:id/tags/:tag_id", delete(groups::remove_tag))
        // ── Downloads ──────────────────────────────────────────────────────
        .route("/api/downloads/status", get(downloads::status))
        .route("/api/downloads/pause", post(downloads::pause))
        .route("/api/downloads/resume", post(downloads::resume))
        // ── Settings ───────────────────────────────────────────────────────
        .route("/api/settings", get(settings::get).patch(settings::patch))
        .route("/api/remote-access", get(remote::status))
        // ── Export / Import ────────────────────────────────────────────────
        .route("/api/export", get(export::export_sources))
        .route("/api/import", post(export::import_sources))
        // ── Stats / Log ────────────────────────────────────────────────────
        .route("/api/stats", get(misc::stats))
        .route("/api/log", get(misc::get_log))
        // ── Curator interactive sessions ────────────────────────────────────
        .route("/api/goon/session", post(goon::start))
        .route("/api/goon/session/complete", post(goon::complete))
        .with_state(state)
}
