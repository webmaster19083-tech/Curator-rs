use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use std::sync::Arc;

use super::AppError;
use crate::state::AppState;

pub async fn get(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let row = conn.query_row(
        "SELECT filepath, type, downloaded, origin_url FROM media WHERE id=?",
        [id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    );

    let (filepath, mtype, downloaded, origin_url) = match row {
        Ok(v) => v,
        Err(_) => return Err(super::not_found("Media not found")),
    };

    // Placeholder — redirect to origin_url
    if downloaded == 0 {
        if let Some(url) = origin_url {
            return Ok(Redirect::temporary(&url).into_response());
        }
        return Err(super::not_found("Not downloaded yet"));
    }

    let library_dir = state.data_dir.join("library");
    let src = dunce::simplified(&library_dir.join(&filepath)).to_path_buf();

    // Videos: serve original directly (no thumbnail generation)
    if mtype != "image" {
        return serve_file(&src).await;
    }

    let thumbs_dir = state.data_dir.join("thumbnails");
    let result = tokio::task::spawn_blocking(move || {
        crate::thumb::get_or_create_thumb(id, src.clone(), thumbs_dir)
            .or_else(|_| std::fs::read(&src).map_err(anyhow::Error::from))
    })
    .await
    .map_err(anyhow::Error::from)??;

    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        result,
    )
        .into_response())
}

async fn serve_file(path: &std::path::Path) -> Result<Response, AppError> {
    let bytes = tokio::fs::read(path).await.map_err(anyhow::Error::from)?;
    Ok((
        [(header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
        bytes,
    )
        .into_response())
}
