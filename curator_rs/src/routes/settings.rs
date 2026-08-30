use std::sync::Arc;
use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;

use crate::{settings::Settings, state::AppState};
use super::AppError;

pub async fn get(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let s = state.settings.read().await.clone();
    Ok(Json(s))
}

pub async fn patch(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, AppError> {
    let mut s = state.settings.write().await;
    // Merge incoming fields over existing settings
    if let Some(v) = body.get("max_concurrent").and_then(|v| v.as_u64()) {
        s.max_concurrent = v as u32;
    }
    if let Some(v) = body.get("default_slideshow_speed").and_then(|v| v.as_f64()) {
        s.default_slideshow_speed = v;
    }
    if let Some(v) = body.get("default_slideshow_loop").and_then(|v| v.as_bool()) {
        s.default_slideshow_loop = v;
    }
    if let Some(v) = body.get("default_slideshow_shuffle").and_then(|v| v.as_bool()) {
        s.default_slideshow_shuffle = v;
    }
    if let Some(v) = body.get("theme").and_then(|v| v.as_str()) {
        s.theme = v.to_string();
    }
    if let Some(v) = body.get("export_reminder_days").and_then(|v| v.as_u64()) {
        s.export_reminder_days = v as u32;
    }
    if let Some(v) = body.get("last_export_at").and_then(|v| v.as_str()) {
        s.last_export_at = Some(v.to_string());
    }
    if body.get("last_export_at").map(|v| v.is_null()).unwrap_or(false) {
        s.last_export_at = None;
    }
    if let Some(v) = body.get("export_reminder_snoozed_until").and_then(|v| v.as_str()) {
        s.export_reminder_snoozed_until = Some(v.to_string());
    }
    if body.get("export_reminder_snoozed_until").map(|v| v.is_null()).unwrap_or(false) {
        s.export_reminder_snoozed_until = None;
    }
    // CH settings
    if let Some(v) = body.get("ch_log_sessions").and_then(|v| v.as_bool()) {
        s.ch_log_sessions = v;
    }
    if let Some(v) = body.get("ch_default_interval").and_then(|v| v.as_f64()) {
        s.ch_default_interval = v;
    }
    if let Some(v) = body.get("ch_default_limit").and_then(|v| v.as_u64()) {
        s.ch_default_limit = v as u32;
    }
    if let Some(v) = body.get("ch_default_shuffle").and_then(|v| v.as_bool()) {
        s.ch_default_shuffle = v;
    }
    if let Some(v) = body.get("ch_default_media_type").and_then(|v| v.as_str()) {
        s.ch_default_media_type = v.to_string();
    }

    s.save(&state.data_dir).map_err(anyhow::Error::from)?;
    Ok(Json(s.clone()))
}
