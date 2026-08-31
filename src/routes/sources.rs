use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::AppState;
use crate::db::now_iso;
use crate::downloader::run_download;
use crate::slug::{derive_name_from_url, normalize_for_compare, normalize_url, slugify, split_bulk_input};
use crate::routes::media::db_err;

// ─── Models ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AddSourcesBody {
    #[serde(default)]
    pub urls: Vec<String>,
    pub text: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchSourceBody {
    pub name:     Option<String>,
    pub included: Option<bool>,
}

#[derive(Deserialize)]
pub struct SetGroupBody {
    pub group_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub delete_files: bool,
}

// ─── GET /api/sources ────────────────────────────────────────────────────────

pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let mut stmt = conn.prepare(
        "SELECT s.*, \
            (SELECT id FROM media m WHERE m.source_id = s.id AND m.type = 'image' \
             ORDER BY m.id LIMIT 1) AS thumbnail_id \
         FROM sources s ORDER BY s.added_at DESC"
    ).map_err(db_err)?;

    let sources: Vec<Value> = stmt.query_map([], row_to_json)
        .map_err(db_err)?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(json!({ "sources": sources })))
}

// ─── GET /api/sources/:id ────────────────────────────────────────────────────

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let row = conn.query_row("SELECT * FROM sources WHERE id=?1", [id], row_to_json);
    match row {
        Ok(v)  => Ok(Json(v)),
        Err(_) => Err((StatusCode::NOT_FOUND, Json(json!({"error": "Source not found"})))),
    }
}

// ─── POST /api/sources ───────────────────────────────────────────────────────

pub async fn add(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddSourcesBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut candidates = body.urls;
    if let Some(text) = body.text {
        candidates.extend(split_bulk_input(&text));
    }
    if candidates.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "No valid URLs provided"}))));
    }
    let result = create_sources_from_urls(Arc::clone(&state), candidates).await?;
    if result["sources"].as_array().map(|a| a.is_empty()).unwrap_or(true)
        && result["duplicates"].as_array().map(|a| a.is_empty()).unwrap_or(true)
    {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "No valid URLs provided"}))));
    }
    Ok(Json(result))
}

// ─── PATCH /api/sources/:id ──────────────────────────────────────────────────

pub async fn patch(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<PatchSourceBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;

    let mut fields: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(name) = body.name {
        fields.push("name=?".to_string());
        values.push(Box::new(name));
    }
    if let Some(included) = body.included {
        fields.push("included=?".to_string());
        values.push(Box::new(if included { 1i64 } else { 0i64 }));
    }
    if fields.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Nothing to update"}))));
    }

    let sql = format!("UPDATE sources SET {} WHERE id=?", fields.join(", "));
    values.push(Box::new(id));
    let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, refs.as_slice()).map_err(db_err)?;

    match conn.query_row("SELECT * FROM sources WHERE id=?1", [id], row_to_json) {
        Ok(v)  => Ok(Json(v)),
        Err(_) => Err((StatusCode::NOT_FOUND, Json(json!({"error": "Source not found"})))),
    }
}

// ─── PATCH /api/sources/:id/group ────────────────────────────────────────────

pub async fn set_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<SetGroupBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;

    let exists: bool = conn.query_row("SELECT COUNT(*) FROM sources WHERE id=?1", [id], |r| r.get::<_,i64>(0))
        .unwrap_or(0) > 0;
    if !exists { return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Source not found"})))); }

    if let Some(gid) = body.group_id {
        let g_exists: bool = conn.query_row("SELECT COUNT(*) FROM groups WHERE id=?1", [gid], |r| r.get::<_,i64>(0))
            .unwrap_or(0) > 0;
        if !g_exists { return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Group not found"})))); }
    }

    conn.execute("UPDATE sources SET group_id=?1 WHERE id=?2", rusqlite::params![body.group_id, id])
        .map_err(db_err)?;

    Ok(Json(conn.query_row("SELECT * FROM sources WHERE id=?1", [id], row_to_json).map_err(db_err)?))
}

// ─── POST /api/sources/:id/resync ────────────────────────────────────────────

pub async fn resync(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let row = conn.query_row(
        "SELECT status FROM sources WHERE id=?1", [id],
        |r| r.get::<_, String>(0)
    );
    let status = match row {
        Ok(s)  => s,
        Err(_) => return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Source not found"})))),
    };

    if status == "pending" || status == "downloading" {
        return Ok(Json(json!({ "status": "already_syncing" })));
    }
    if state.downloads_paused.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(Json(json!({ "status": "paused" })));
    }

    tokio::spawn(run_download(Arc::clone(&state), id));
    Ok(Json(json!({ "status": "queued" })))
}

// ─── POST /api/sources/resync-all ────────────────────────────────────────────

pub async fn resync_all(State(state): State<Arc<AppState>>) -> Json<Value> {
    if state.downloads_paused.load(std::sync::atomic::Ordering::SeqCst) {
        return Json(json!({ "queued": 0, "paused": true }));
    }
    let ids: Vec<i64> = {
        let conn = state.pool.get().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id FROM sources WHERE status NOT IN ('pending','downloading')"
        ).unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap()
            .filter_map(|r| r.ok()).collect()
    };
    let count = ids.len();
    for id in ids {
        tokio::spawn(run_download(Arc::clone(&state), id));
    }
    Json(json!({ "queued": count }))
}

// ─── DELETE /api/sources/:id ─────────────────────────────────────────────────

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;

    let (slug,): (String,) = conn.query_row(
        "SELECT slug FROM sources WHERE id=?1", [id], |r| Ok((r.get(0)?,))
    ).map_err(|_| (StatusCode::NOT_FOUND, Json(json!({"error": "Source not found"}))))?;

    conn.execute("DELETE FROM sources WHERE id=?1", [id]).map_err(db_err)?;

    // Kill any in-flight process BEFORE touching files (Windows PermissionError guard)
    if let Some(pid) = state.active_processes.lock().await.remove(&id) {
        crate::downloader::kill_pid(pid).await;
        // Give the OS a moment to release file handles
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    if q.delete_files {
        let dest = state.library_dir.join(&slug);
        if dest.exists() {
            let dest_long = dunce::simplified(&dest).to_path_buf();
            let _ = std::fs::remove_dir_all(&dest_long);
        }
        let archive = state.archives_dir.join(format!("{}.sqlite3", slug));
        if archive.exists() {
            let _ = std::fs::remove_file(dunce::simplified(&archive));
        }
    }

    Ok(Json(json!({ "status": "deleted" })))
}

// ─── Shared create logic ──────────────────────────────────────────────────────

pub async fn create_sources_from_urls(
    state:      Arc<AppState>,
    candidates: Vec<String>,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let mut normalized: Vec<String> = Vec::new();
    let mut seen_norm = std::collections::HashSet::new();
    for c in candidates {
        let u = normalize_url(&c);
        if !u.is_empty() && !seen_norm.contains(&u) {
            seen_norm.insert(u.clone());
            normalized.push(u);
        }
    }
    if normalized.is_empty() {
        return Ok(json!({ "sources": [], "duplicates": [] }));
    }

    let conn = state.pool.get().map_err(db_err)?;
    let existing: std::collections::HashMap<String, String> = {
        let mut stmt = conn.prepare("SELECT url, name FROM sources").map_err(db_err)?;
        let out = stmt.query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .map(|(url, name)| (normalize_for_compare(&url), name))
            .collect();
        out
    };

    let mut to_create: Vec<String> = Vec::new();
    let mut duplicates: Vec<Value> = Vec::new();
    let mut existing = existing;

    for url in &normalized {
        let key = normalize_for_compare(url);
        if let Some(name) = existing.get(&key) {
            duplicates.push(json!({ "url": url, "name": name }));
        } else {
            existing.insert(key, String::new());
            to_create.push(url.clone());
        }
    }

    let mut created_ids: Vec<i64> = Vec::new();
    for url in &to_create {
        let name = derive_name_from_url(url);
        let base_slug = slugify(&name);
        conn.execute(
            "INSERT INTO sources (name, url, slug, status, added_at) VALUES (?1,?2,?3,'pending',?4)",
            rusqlite::params![name, url, base_slug, now_iso()],
        ).map_err(db_err)?;
        let source_id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE sources SET slug=?1 WHERE id=?2",
            rusqlite::params![format!("{}-{}", source_id, base_slug), source_id],
        ).map_err(db_err)?;
        created_ids.push(source_id);
    }

    for &id in &created_ids {
        tokio::spawn(run_download(Arc::clone(&state), id));
    }

    let sources: Vec<Value> = if !created_ids.is_empty() {
        let placeholders = created_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT * FROM sources WHERE id IN ({})", placeholders);
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let params: Vec<&dyn rusqlite::ToSql> = created_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let out = stmt.query_map(params.as_slice(), row_to_json)
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();
        out
    } else {
        Vec::new()
    };

    Ok(json!({ "sources": sources, "duplicates": duplicates }))
}

// ─── Row → serde_json::Value helper ──────────────────────────────────────────

fn row_to_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let count = row.as_ref().column_count();
    let mut map = serde_json::Map::new();
    for i in 0..count {
        let name = row.as_ref().column_name(i).unwrap_or("?").to_string();
        let val: Value = match row.get_ref(i)? {
            rusqlite::types::ValueRef::Null       => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => json!(n),
            rusqlite::types::ValueRef::Real(f)    => json!(f),
            rusqlite::types::ValueRef::Text(s)    => json!(std::str::from_utf8(s).unwrap_or("")),
            rusqlite::types::ValueRef::Blob(b)    => json!(std::str::from_utf8(b).unwrap_or("")),
        };
        map.insert(name, val);
    }
    Ok(Value::Object(map))
}
