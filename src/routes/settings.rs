use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::AppState;
use crate::db::save_settings;

const VALID_THEMES: &[&str] = &[
    "system", "yotsuba", "yotsuba-b", "futaba", "burichan",
    "tomorrow", "photon", "light", "oled-dark",
];

#[derive(Deserialize)]
pub struct PatchSettingsBody {
    pub max_concurrent:               Option<u32>,
    pub default_slideshow_speed:      Option<f64>,
    pub default_slideshow_loop:       Option<bool>,
    pub default_slideshow_shuffle:    Option<bool>,
    pub theme:                        Option<String>,
    pub export_reminder_days:         Option<u32>,
    pub export_reminder_snoozed_until: Option<String>,
    // Cock Hero settings
    pub ch_log_sessions:              Option<bool>,
    pub ch_default_interval:          Option<f64>,
    pub ch_default_limit:             Option<u32>,
    pub ch_default_shuffle:           Option<bool>,
    pub ch_default_media_type:        Option<String>,
}

// ─── GET /api/settings ───────────────────────────────────────────────────────

pub async fn get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let s = state.settings.read().await;
    Json(serde_json::to_value(&*s).unwrap_or_default())
}

// ─── PATCH /api/settings ─────────────────────────────────────────────────────

pub async fn patch(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<PatchSettingsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error": format!("Unknown theme: {}", theme)}))));
        }
        settings.theme = theme.clone();
    }
    if let Some(v) = body.export_reminder_days {
        settings.export_reminder_days = v.clamp(1, 365);
    }
    if let Some(v) = body.export_reminder_snoozed_until {
        settings.export_reminder_snoozed_until = if v.is_empty() { None } else { Some(v) };
    }
    if let Some(v) = body.ch_log_sessions        { settings.ch_log_sessions = v; }
    if let Some(v) = body.ch_default_interval    { settings.ch_default_interval = v; }
    if let Some(v) = body.ch_default_limit        { settings.ch_default_limit = v; }
    if let Some(v) = body.ch_default_shuffle      { settings.ch_default_shuffle = v; }
    if let Some(v) = body.ch_default_media_type  { settings.ch_default_media_type = v; }

    save_settings(&state.data_dir, &*settings);
    Ok(Json(serde_json::to_value(&*settings).unwrap_or_default()))
}
