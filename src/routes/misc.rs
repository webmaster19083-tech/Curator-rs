use axum::{extract::State, http::header, response::IntoResponse, Json};
use serde_json::json;
use std::sync::Arc;

use super::AppError;
use crate::state::AppState;

pub async fn stats(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;

    let source_count: i64 = conn.query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))?;
    let media_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM media WHERE downloaded=1", [], |r| {
            r.get(0)
        })?;
    let tag_count: i64 = conn.query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;
    let group_count: i64 = conn.query_row("SELECT COUNT(*) FROM groups", [], |r| r.get(0))?;

    let video_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE type='video' AND downloaded=1",
        [],
        |r| r.get(0),
    )?;
    let image_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE type='image' AND downloaded=1",
        [],
        |r| r.get(0),
    )?;

    let rated_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE rating > 0 AND downloaded=1",
        [],
        |r| r.get(0),
    )?;

    Ok(Json(json!({
        "source_count": source_count,
        "media_count": media_count,
        "image_count": image_count,
        "video_count": video_count,
        "tag_count": tag_count,
        "group_count": group_count,
        "rated_count": rated_count,
    })))
}

pub async fn log(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let log_path = state.data_dir.join("curator.log");
    let content = tokio::fs::read_to_string(&log_path)
        .await
        .unwrap_or_default();
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        content,
    )
        .into_response())
}
