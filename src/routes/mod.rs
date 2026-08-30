pub mod downloads;
pub mod export;
pub mod groups;
pub mod media;
pub mod misc;
pub mod preview;
pub mod settings;
pub mod sources;
pub mod tags;
pub mod thumb;

use axum::{http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::json;

/// Unified error type for route handlers
pub struct AppError(pub StatusCode, pub String);

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"detail": self.1}))).into_response()
    }
}

pub fn not_found(msg: &str) -> AppError {
    AppError(StatusCode::NOT_FOUND, msg.to_string())
}
pub fn bad_request(msg: &str) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.to_string())
}
