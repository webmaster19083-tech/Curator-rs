use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs::File;
use tokio_util::io::ReaderStream;

use crate::AppState;
use crate::chpack::{build_chpack, safe_pack_filename, ExportRow};
use crate::db::{now_iso, save_settings};
use crate::routes::media::db_err;
use crate::routes::sources::create_sources_from_urls;
use crate::slug::normalize_for_compare;

// ─── GET /api/export ─────────────────────────────────────────────────────────

pub async fn export_sources(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // rusqlite's Connection/Statement are !Send, so they must be dropped
    // before the `.await` below rather than held across it.
    let sources: Vec<Value> = {
        let conn = state.pool.get().map_err(db_err)?;
        let mut stmt = conn.prepare(
            "SELECT s.name, s.url, s.included, g.name AS group_name \
             FROM sources s LEFT JOIN groups g ON g.id = s.group_id \
             ORDER BY s.added_at"
        ).map_err(db_err)?;

        let out = stmt.query_map([], |r| Ok(json!({
            "name":     r.get::<_, String>(0)?,
            "url":      r.get::<_, String>(1)?,
            "included": r.get::<_, i64>(2)? != 0,
            "group":    r.get::<_, Option<String>>(3)?,
        }))).map_err(db_err)?
        .filter_map(|r| r.ok())
        .collect();
        out
    };

    let exported_at = now_iso();

    // Reset backup reminder clock
    {
        let mut settings = state.settings.write().await;
        settings.last_export_at               = Some(exported_at.clone());
        settings.export_reminder_snoozed_until = None;
        save_settings(&state.data_dir, &*settings);
    }

    Ok(Json(json!({ "exported_at": exported_at, "sources": sources })))
}

// ─── POST /api/import ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ImportBody {
    pub sources: Vec<Value>,
}

pub async fn import_sources(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<ImportBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let urls: Vec<String> = body.sources.iter()
        .filter_map(|s| s.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    if urls.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "No valid entries to import"}))));
    }

    let result = create_sources_from_urls(Arc::clone(&state), urls).await?;

    // Best-effort: restore group assignments from the import file
    if result["sources"].as_array().map(|a| !a.is_empty()).unwrap_or(false) {
        let entries_by_url: std::collections::HashMap<String, String> = body.sources.iter()
            .filter_map(|s| {
                let url   = s.get("url")?.as_str()?.to_string();
                let group = s.get("group")?.as_str()?.to_string();
                if group.is_empty() { return None; }
                Some((normalize_for_compare(&url), group))
            })
            .collect();

        if !entries_by_url.is_empty() {
            let conn = state.pool.get().map_err(db_err)?;
            let mut group_by_name: std::collections::HashMap<String, i64> = {
                let mut stmt = conn.prepare("SELECT id, name FROM groups").map_err(db_err)?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(0)?)))
                    .map_err(db_err)?
                    .filter_map(|r| r.ok())
                    .collect();
                rows
            };

            if let Some(created) = result["sources"].as_array() {
                for src in created {
                    let url   = src["url"].as_str().unwrap_or("");
                    let src_id = src["id"].as_i64().unwrap_or(0);
                    if src_id == 0 { continue; }

                    let gname = match entries_by_url.get(&normalize_for_compare(url)) {
                        Some(g) => g.clone(),
                        None    => continue,
                    };

                    let gid = if let Some(&id) = group_by_name.get(&gname) {
                        id
                    } else {
                        conn.execute("INSERT INTO groups (name, added_at) VALUES (?1,?2)", rusqlite::params![gname, now_iso()])
                            .map_err(db_err)?;
                        let new_id = conn.last_insert_rowid();
                        group_by_name.insert(gname, new_id);
                        new_id
                    };

                    let _ = conn.execute("UPDATE sources SET group_id=?1 WHERE id=?2", rusqlite::params![gid, src_id]);
                }
            }

            // Invalidate group tag cache
            *state.group_tag_cache.write().await = None;
        }
    }

    Ok(Json(result))
}

// ─── POST /api/export/chpack ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChpackBody {
    pub source_id:   Option<i64>,
    pub name:        Option<String>,
    #[serde(default = "default_author")]
    pub author:      String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub unlock_cost: i64,
}
fn default_author() -> String { "Curator".into() }

pub async fn export_chpack(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<ChpackBody>,
) -> Response {
    let conn = match state.pool.get() {
        Ok(c)  => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    };

    struct Row { filepath: String, kind: String, rating: i64, tags_csv: Option<String> }

    let (pack_name, rows) = if let Some(sid) = body.source_id {
        let src_name: Option<String> = conn.query_row(
            "SELECT name FROM sources WHERE id=?1", [sid], |r| r.get(0)
        ).ok();
        if src_name.is_none() {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "Source not found"}))).into_response();
        }
        let pname = body.name.as_deref().unwrap_or(src_name.as_deref().unwrap_or("Curator Export")).to_string();

        let mut stmt = conn.prepare(
            "SELECT m.filepath, m.type, m.rating, \
                (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id) AS tags_csv \
             FROM media m WHERE m.source_id=?1 AND m.downloaded=1 ORDER BY m.id"
        ).unwrap();
        let rows: Vec<Row> = stmt.query_map([sid], |r| Ok(Row {
            filepath: r.get(0)?, kind: r.get(1)?, rating: r.get(2)?, tags_csv: r.get(3)?
        })).unwrap().filter_map(|r| r.ok()).collect();

        (pname, rows)
    } else {
        let pname = body.name.as_deref().unwrap_or("Curator Export").to_string();
        let mut stmt = conn.prepare(
            "SELECT m.filepath, m.type, m.rating, \
                (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id) AS tags_csv \
             FROM media m WHERE m.downloaded=1 ORDER BY m.id"
        ).unwrap();
        let rows: Vec<Row> = stmt.query_map([], |r| Ok(Row {
            filepath: r.get(0)?, kind: r.get(1)?, rating: r.get(2)?, tags_csv: r.get(3)?
        })).unwrap().filter_map(|r| r.ok()).collect();

        (pname, rows)
    };

    if rows.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "No downloaded media found for this selection"}))).into_response();
    }

    let export_rows: Vec<ExportRow> = rows.into_iter().map(|r| ExportRow {
        filepath: r.filepath,
        kind:     r.kind,
        rating:   r.rating,
        tags:     r.tags_csv.as_deref().unwrap_or("").split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.trim().to_lowercase())
            .collect(),
    }).collect();

    let library_dir = state.library_dir.clone();
    let filename    = safe_pack_filename(&pack_name);
    let author      = body.author.clone();
    let description = body.description.clone();
    let unlock_cost = body.unlock_cost;

    let result = tokio::task::spawn_blocking(move || {
        build_chpack(pack_name, author, description, unlock_cost, export_rows, &library_dir)
    }).await;

    match result {
        Ok(Ok(tmp)) => {
            match File::open(tmp.path()).await {
                Ok(f) => {
                    let stream = ReaderStream::new(f);
                    let body   = Body::from_stream(stream);
                    let cd = format!("attachment; filename=\"{}\"", filename);
                    let mut resp = body.into_response();
                    resp.headers_mut().insert(
                        header::CONTENT_TYPE,
                        "application/zip".parse().unwrap(),
                    );
                    resp.headers_mut().insert(
                        header::CONTENT_DISPOSITION,
                        cd.parse().unwrap(),
                    );
                    resp
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
            }
        }
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
        Err(e)     => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}
