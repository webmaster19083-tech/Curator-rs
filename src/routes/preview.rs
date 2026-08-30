use std::sync::Arc;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{downloader::run_gallery_dl_j, state::AppState};
use super::{bad_request, AppError};

#[derive(Deserialize)]
pub struct ScanQuery {
    pub url: Option<String>,
}

#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub templates: Vec<String>,
}

pub async fn scan(
    Query(q): Query<ScanQuery>,
    State(_state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let url = q.url.as_deref().unwrap_or("").trim().to_string();
    if url.is_empty() { return Err(bad_request("url param required")); }

    let raw = run_gallery_dl_j(&url, 120).await
        .map_err(|e| AppError(axum::http::StatusCode::BAD_GATEWAY, e.to_string()))?;

    let items = parse_preview_output(&raw);
    Ok(Json(json!({"items": items, "source_url": url})))
}

pub async fn search(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, AppError> {
    if body.query.trim().is_empty() { return Err(bad_request("query required")); }
    if body.templates.is_empty() { return Err(bad_request("templates required")); }

    let enc = percent_encoding::utf8_percent_encode(
        body.query.trim(),
        percent_encoding::NON_ALPHANUMERIC,
    ).to_string();

    let urls: Vec<String> = body.templates.iter()
        .map(|t| t.replace("{query}", &enc))
        .collect();

    let mut all_items: Vec<Value> = Vec::new();
    for url in urls {
        if let Ok(raw) = run_gallery_dl_j(&url, 60).await {
            let mut items = parse_preview_output(&raw);
            all_items.append(&mut items);
        }
    }

    Ok(Json(json!({"items": all_items, "query": body.query})))
}

fn parse_preview_output(raw: &str) -> Vec<Value> {
    let Ok(root) = serde_json::from_str::<Value>(raw) else { return vec![] };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk(&root, &mut out, &mut seen);
    out
}

fn walk(node: &Value, out: &mut Vec<Value>, seen: &mut std::collections::HashSet<String>) {
    let Some(arr) = node.as_array() else { return };
    if arr.len() >= 2 {
        if let Some(url) = arr[1].as_str() {
            if url.starts_with("http") && !seen.contains(url) {
                let meta = arr.last().and_then(|v| v.as_object());
                let ext = meta
                    .and_then(|m| m.get("extension")).and_then(|v| v.as_str())
                    .unwrap_or("").to_lowercase();
                let ext = if ext.is_empty() {
                    url.split('?').next().unwrap_or("").rsplit('.').next().unwrap_or("").to_lowercase()
                } else { ext };
                let mtype = media_type(&ext);
                if mtype != "unknown" {
                    seen.insert(url.to_string());
                    let title = meta.and_then(|m| m.get("title")).and_then(|v| v.as_str())
                        .unwrap_or("").to_string();
                    let author = meta.and_then(|m| m.get("uploader")
                        .or_else(|| m.get("author")).or_else(|| m.get("user")))
                        .and_then(|v| v.as_str()).unwrap_or("").to_string();
                    out.push(json!({
                        "url": url,
                        "type": mtype,
                        "title": title,
                        "author": author,
                        "ext": ext,
                    }));
                }
            }
        }
    }
    for item in arr { if item.is_array() { walk(item, out, seen); } }
}

fn media_type(ext: &str) -> &'static str {
    match ext {
        "jpg"|"jpeg"|"png"|"gif"|"webp"|"bmp"|"tiff"|"tif"|"avif"|"heic"|"heif" => "image",
        "mp4"|"mkv"|"webm"|"avi"|"mov"|"wmv"|"flv"|"m4v"|"ts" => "video",
        _ => "unknown",
    }
}
