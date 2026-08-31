use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde_json::json;

use crate::AppState;
use crate::thumb_worker::get_or_create_thumb;

pub async fn get_thumbnail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Response {
    let conn = match state.pool.get() {
        Ok(c)  => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    };

    let row = conn.query_row(
        "SELECT filepath, type, downloaded, origin_url FROM media WHERE id=?1",
        [id],
        |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    );

    let (filepath, kind, downloaded, origin_url) = match row {
        Ok(v)  => v,
        Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "Media not found"}))).into_response(),
    };

    // Placeholder: redirect to origin_url for the frontend to stream directly
    if downloaded == 0 {
        return if let Some(url) = origin_url {
            Redirect::temporary(&url).into_response()
        } else {
            (StatusCode::NOT_FOUND, Json(json!({"error": "Not downloaded yet"}))).into_response()
        };
    }

    let src_path = dunce::simplified(&state.library_dir.join(&filepath)).to_path_buf();

    // Videos — serve the real file (no thumbnail for video)
    if kind == "video" {
        return serve_file_with_cache(&src_path).await;
    }

    // Images — generate/serve cached thumbnail
    match get_or_create_thumb(id, src_path.clone(), state.thumbs_dir.clone()).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE,  "image/jpeg"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            bytes,
        ).into_response(),
        Err(_) => {
            // Thumbnail failed — fall back to original
            serve_file_with_cache(&src_path).await
        }
    }
}

async fn serve_file_with_cache(path: &std::path::Path) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let mime = mime_from_path(path);
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE,  mime),
                    (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
                ],
                bytes,
            ).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "File not found").into_response(),
    }
}

fn mime_from_path(path: &std::path::Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "png"  => "image/png",
        "gif"  => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp"  => "image/bmp",
        "tiff" => "image/tiff",
        "mp4"  => "video/mp4",
        "webm" => "video/webm",
        "mov"  => "video/quicktime",
        "avi"  => "video/x-msvideo",
        "mkv"  => "video/x-matroska",
        "m4v"  => "video/mp4",
        _      => "application/octet-stream",
    }
}
