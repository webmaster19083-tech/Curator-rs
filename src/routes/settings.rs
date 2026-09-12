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

/// Shared with `routes::oobe` (the Appearance step reuses the exact same
/// allow-list rather than re-declaring it) — see "Do not introduce
/// conflicting configuration systems" in the OOBE build notes.
pub(crate) const VALID_THEMES: &[&str] = &[
    // Current Explorer palettes.
    "system", "atelier-dark", "midnight", "ember", "linen", "sage", "aurora", "oled",
    // Legacy names remain accepted so an older settings.json can be opened
    // and normalized by the browser without becoming an invalid preference.
    "yotsuba", "yotsuba-b", "futaba", "burichan", "tomorrow", "photon", "light", "oled-dark", "dark",
];

#[derive(Deserialize)]
pub struct PatchSettingsBody {
    pub start_with_windows: Option<bool>,
    pub keep_running_in_tray: Option<bool>,
    pub max_clip_length_secs: Option<u32>,
    pub goon_default_limit: Option<u32>,
    pub goon_log_sessions: Option<bool>,
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
    // NSFW auto-rating
    pub nsfw_filter_enabled: Option<bool>,
    pub library_layout: Option<String>,
    pub last_play_mode: Option<String>,
    pub search_providers: Option<Vec<String>>,
    pub metronome_enabled: Option<bool>,
    pub metronome_volume: Option<f64>,
    pub goon_persona: Option<String>,
    pub tts_voice: Option<String>,
    pub tts_rate: Option<f64>,
    pub tts_pitch: Option<f64>,
    pub tts_volume: Option<f64>,
    pub soundtrack_provider: Option<String>,
    /// Bootstrap settings live in config.json because they are consumed
    /// before the database is opened. They are exposed here for the normal
    /// Settings UI but intentionally take effect on the next launch.
    pub ffmpeg_bin: Option<String>,
    pub action_model_path: Option<String>,
}

// ─── GET /api/settings ───────────────────────────────────────────────────────

pub async fn get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let s = state.settings.read().await;
    let mut value = serde_json::to_value(&*s).unwrap_or_default();
    let config = crate::config::load_config();
    if let Some(object) = value.as_object_mut() {
        object.insert("ffmpeg_bin".into(), json!(config.ffmpeg_bin.unwrap_or_else(|| "ffmpeg".into())));
        object.insert("action_model_path".into(), json!(config.action_model_path));
        object.insert("external_tool_settings_restart_required".into(), json!(true));
    }
    Json(value)
}

// ─── PATCH /api/settings ─────────────────────────────────────────────────────

pub async fn patch(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<PatchSettingsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.ffmpeg_bin.is_some() || body.action_model_path.is_some() {
        let mut config = crate::config::load_config();
        if let Some(value) = body.ffmpeg_bin.as_deref() {
            let value = value.trim();
            if value.is_empty() || value.len() > 4096 || value.contains('\0') {
                return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid ffmpeg executable"}))));
            }
            config.ffmpeg_bin = Some(value.to_string());
        }
        if let Some(value) = body.action_model_path.as_deref() {
            let value = value.trim();
            if value.len() > 4096 || value.contains('\0') {
                return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid action-model path"}))));
            }
            config.action_model_path = if value.is_empty() { None } else { Some(value.to_string()) };
        }
        crate::config::save_config(&config).map_err(|error| (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":format!("Could not save external tool settings: {error}")})),
        ))?;
    }
    if let Some(enabled) = body.start_with_windows {
        crate::set_start_with_windows_preference(&state, enabled)
            .await
            .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({"error": error}))))?;
    }

    let mut settings = state.settings.write().await;

    if let Some(v) = body.max_clip_length_secs {
        settings.max_clip_length_secs = v.clamp(5, 3600);
    }
    if let Some(v) = body.goon_default_limit {
        settings.goon_default_limit = v.clamp(1, 10_000);
    }
    if let Some(v) = body.goon_log_sessions {
        settings.goon_log_sessions = v;
    }
    if let Some(v) = body.keep_running_in_tray {
        settings.keep_running_in_tray = v;
    }
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
    if let Some(v) = body.nsfw_filter_enabled {
        settings.nsfw_filter_enabled = v;
    }
    if let Some(v) = body.library_layout {
        if !["grid", "table"].contains(&v.as_str()) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Library layout must be grid or table"})),
            ));
        }
        settings.library_layout = v;
    }
    if let Some(v) = body.last_play_mode {
        let normalized = match v.as_str() {
            "feed" | "mobile-feed" => "feed",
            "slideshow" | "portrait" | "portrait-wall" | "review" | "goon" => v.as_str(),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"Unknown playback mode"})),
                ));
            }
        };
        settings.last_play_mode = normalized.to_string();
    }
    if let Some(values) = body.search_providers {
        if values.len() > 64 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Choose at most 64 search providers"})),
            ));
        }
        let mut providers = Vec::new();
        for raw in values {
            let provider = raw.trim().to_ascii_lowercase();
            if provider.is_empty()
                || provider.len() > 80
                || !provider
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"Invalid search provider id"})),
                ));
            }
            if !providers.contains(&provider) {
                providers.push(provider);
            }
        }
        // A local catalog is always available.  Keeping it in the durable
        // list makes the selection explicit while still avoiding an empty
        // search experience after a user unticks every remote provider.
        if !providers.iter().any(|id| id == "local") {
            providers.insert(0, "local".to_string());
        }
        settings.search_providers = providers;
    }
    if let Some(v) = body.metronome_enabled {
        settings.metronome_enabled = v;
    }
    if let Some(v) = body.metronome_volume {
        if !v.is_finite() {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid metronome volume"}))));
        }
        settings.metronome_volume = v.clamp(0.0, 1.0);
    }
    if let Some(v) = body.goon_persona {
        if !["neutral", "mommy", "dom", "brat"].contains(&v.as_str()) {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Unknown GOON persona"}))));
        }
        settings.goon_persona = v;
    }
    if let Some(v) = body.tts_voice {
        let voice = v.trim();
        settings.tts_voice = if voice.is_empty() { None } else { Some(voice.chars().take(160).collect()) };
    }
    if let Some(value) = body.tts_rate {
        if !value.is_finite() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid TTS rate"})))); }
        settings.tts_rate = value.clamp(0.5, 2.0);
    }
    if let Some(value) = body.tts_pitch {
        if !value.is_finite() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid TTS pitch"})))); }
        settings.tts_pitch = value.clamp(0.5, 2.0);
    }
    if let Some(value) = body.tts_volume {
        if !value.is_finite() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid TTS volume"})))); }
        settings.tts_volume = value.clamp(0.0, 1.0);
    }
    if let Some(v) = body.soundtrack_provider {
        if !["local", "youtube", "soundcloud", "apple_music", "spotify"].contains(&v.as_str()) {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Unknown soundtrack provider"}))));
        }
        settings.soundtrack_provider = v;
    }

    save_settings(&state.data_dir, &settings);
    let mut value = serde_json::to_value(&*settings).unwrap_or_default();
    let config = crate::config::load_config();
    if let Some(object) = value.as_object_mut() {
        object.insert("ffmpeg_bin".into(), json!(config.ffmpeg_bin.unwrap_or_else(|| "ffmpeg".into())));
        object.insert("action_model_path".into(), json!(config.action_model_path));
        object.insert("external_tool_settings_restart_required".into(), json!(true));
    }
    Ok(Json(value))
}
