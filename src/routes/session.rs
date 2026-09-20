//! Thin remote/recovery adapter for the shared Rust session service.
//!
//! There is deliberately no scheduling, timing, or session mutation policy in
//! these handlers. The Slint shell will call the same `SessionService` methods.

use crate::{
    session::{GameConfig, SessionControl, SessionState, SessionUpdate},
    AppState,
};
use axum::{extract::State, http::StatusCode, Json};
use serde_json::json;
use std::sync::Arc;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn session_error(error: String) -> (StatusCode, Json<serde_json::Value>) {
    let status = if error == "A session is already active" {
        StatusCode::CONFLICT
    } else if error == "No active session" {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(json!({ "error": error })))
}

pub async fn current(State(state): State<Arc<AppState>>) -> Json<Option<SessionState>> {
    Json(state.sessions.snapshot())
}

pub async fn start(
    State(state): State<Arc<AppState>>,
    Json(config): Json<GameConfig>,
) -> ApiResult<SessionUpdate> {
    state
        .sessions
        .start_running(config)
        .map(Json)
        .map_err(session_error)
}

pub async fn control(
    State(state): State<Arc<AppState>>,
    Json(command): Json<SessionControl>,
) -> ApiResult<SessionUpdate> {
    state
        .sessions
        .control(command)
        .map(Json)
        .map_err(session_error)
}
