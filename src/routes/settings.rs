use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::db::save_settings;
use crate::AppState;

/// Shared with `routes::oobe` (the Appearance step reuses the exact same
/// allow-list rather than re-declaring it) — see "Do not introduce
/// conflicting configuration systems" in the OOBE build notes.
pub(crate) const VALID_THEMES: &[&str] = &[
    "system",
    "yotsuba",
    "yotsuba-b",
    "futaba",
    "burichan",
    "tomorrow",
    "photon",
    "light",
    "oled-dark",
];

#[derive(Deserialize)]
pub struct PatchSettingsBody {
    pub max_concurrent: Option<u32>,
    pub default_slideshow_speed: Option<f64>,
    pub default_slideshow_loop: Option<bool>,
    pub default_slideshow_shuffle: Option<bool>,
    pub theme: Option<String>,
    pub export_reminder_days: Option<u32>,
    pub export_reminder_snoozed_until: Option<String>,
    // Curator-native interactive-session settings. Aliases preserve API
    // compatibility for an older settings panel without keeping it exposed.
    #[serde(alias = "ch_log_sessions")]
    pub goon_log_sessions: Option<bool>,
    #[serde(alias = "ch_default_interval")]
    pub goon_default_interval: Option<f64>,
    #[serde(alias = "ch_default_limit")]
    pub goon_default_limit: Option<u32>,
    #[serde(alias = "ch_default_shuffle")]
    pub goon_default_shuffle: Option<bool>,
    #[serde(alias = "ch_default_media_type")]
    pub goon_default_media_type: Option<String>,
    pub max_clip_length_secs: Option<u32>,
    pub last_play_mode: Option<String>,
    pub start_with_windows: Option<bool>,
    pub keep_running_in_tray: Option<bool>,
    // NSFW auto-rating
    pub nsfw_filter_enabled: Option<bool>,
}

// ─── GET /api/settings ───────────────────────────────────────────────────────

pub async fn get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let s = state.settings.read().await;
    Json(serde_json::to_value(&*s).unwrap_or_default())
}

// ─── PATCH /api/settings ─────────────────────────────────────────────────────

pub async fn patch(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PatchSettingsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if let Some(enabled) = body.start_with_windows {
        crate::set_start_with_windows_preference(&state, enabled)
            .await
            .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({"error": error}))))?;
    }

    let mut settings = state.settings.write().await;

    if let Some(v) = body.max_concurrent {
        let v = v.clamp(1, 20);
        settings.max_concurrent = v;
        // Swap the semaphore so future downloads use the new limit
        let mut sem_guard = state.download_semaphore.lock().await;
        *sem_guard = Arc::new(Semaphore::new(v as usize));
    }
    if let Some(v) = body.default_slideshow_speed {
        settings.default_slideshow_speed = v.clamp(500.0, 60000.0);
    }
    if let Some(v) = body.default_slideshow_loop {
        settings.default_slideshow_loop = v;
    }
    if let Some(v) = body.default_slideshow_shuffle {
        settings.default_slideshow_shuffle = v;
    }
    if let Some(ref theme) = body.theme {
        if !VALID_THEMES.contains(&theme.as_str()) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Unknown theme: {}", theme)})),
            ));
        }
        settings.theme = theme.clone();
    }
    if let Some(v) = body.export_reminder_days {
        settings.export_reminder_days = v.clamp(1, 365);
    }
    if let Some(v) = body.export_reminder_snoozed_until {
        settings.export_reminder_snoozed_until = if v.is_empty() { None } else { Some(v) };
    }
    if let Some(v) = body.goon_log_sessions {
        settings.goon_log_sessions = v;
    }
    if let Some(v) = body.goon_default_interval {
        settings.goon_default_interval = v.clamp(5.0, 120.0);
    }
    if let Some(v) = body.goon_default_limit {
        settings.goon_default_limit = v.clamp(1, 5_000);
    }
    if let Some(v) = body.goon_default_shuffle {
        settings.goon_default_shuffle = v;
    }
    if let Some(v) = body.goon_default_media_type {
        if !["all", "image", "video"].contains(&v.as_str()) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Unknown GOON media type"})),
            ));
        }
        settings.goon_default_media_type = v;
    }
    if let Some(v) = body.max_clip_length_secs {
        // This is a classifier boundary, not a transcoding request. Keep it
        // broad enough for a person with a non-standard workflow while
        // rejecting values that would make every normal video a "clip".
        settings.max_clip_length_secs = v.clamp(5, 3600);
    }
    if let Some(v) = body.last_play_mode {
        settings.last_play_mode = match v.as_str() {
            // The short values are what the Explorer split-button uses;
            // retain the descriptive historic spellings in settings files.
            "feed" | "mobile-feed" => "mobile-feed".to_string(),
            "slideshow" => "slideshow".to_string(),
            "portrait" | "portrait-wall" => "portrait-wall".to_string(),
            "review" => "review".to_string(),
            "goon" => "goon".to_string(),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"Unknown play mode"})),
                ))
            }
        };
    }
    if let Some(v) = body.start_with_windows {
        settings.start_with_windows = v;
    }
    if let Some(v) = body.keep_running_in_tray {
        settings.keep_running_in_tray = v;
    }
    if let Some(v) = body.nsfw_filter_enabled {
        settings.nsfw_filter_enabled = v;
    }

    save_settings(&state.data_dir, &settings);
    Ok(Json(serde_json::to_value(&*settings).unwrap_or_default()))
}
